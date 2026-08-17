// NMEA 0183 over TCP — the hub's GPS source (owner direction 2026-08-17, brvg-internal
// docs/HUB-PROXY.md). A router with onboard GNSS (Cradlepoint "Send to Client(s)", a chartplotter,
// gpsd) serves a live sentence stream on a LAN port; the hub dials it, parses fixes, and reports
// `gps.measurement` through the ordinary roll-up. The hub is always the CLIENT — nothing on the
// boat listens except the router on its LAN side, and the stream never leaves the LAN.
//
// The parser is the app's (dashboard/src/utils/gpsSources.ts parseNmea) ported verbatim — RMC
// preferred, GGA fallback with HDOP×5 as rough accuracy, talker-agnostic, checksum optional. The
// canonical sentences in test/nmea.test.ts are copied byte-for-byte from the app's
// gpsSources.test.ts: if either side's parser drifts, a test goes red somewhere — same discipline
// as the batch contract fixture.
//
// The stream runs at 1–10 Hz; the governor between it and the spool is the report half of the
// app's gpsSmart rules (same constants): movement ≥ REPORT_MOVE_M reports within REPORT_MIN_MS —
// a dragging anchor moves meters per second — while a boat swinging on the hook costs one
// stationary heartbeat per GPS_INTERVAL. The worker's per-tier telemetry resolution still applies
// on top, server-side.

import { Socket } from 'node:net';

export interface GpsFix {
  lat: number;
  lon: number;
  /** Accuracy radius in meters when the source provides one (HDOP-derived — rough). */
  acc?: number;
}

// --- Parser (ported from the app; keep in lockstep) --------------------------------------------

function num(v: unknown): number | null {
  const n = typeof v === 'string' ? Number(v) : (v as number);
  return typeof n === 'number' && Number.isFinite(n) ? n : null;
}

function validFix(lat: number | null, lon: number | null, acc?: number | null): GpsFix | null {
  if (lat === null || lon === null) return null;
  if (Math.abs(lat) > 90 || Math.abs(lon) > 180) return null;
  if (lat === 0 && lon === 0) return null; // "no fix yet" placeholder on several firmwares
  return { lat, lon, acc: acc != null && acc >= 0 ? acc : undefined };
}

/** NMEA ddmm.mmmm (lat) / dddmm.mmmm (lon) + hemisphere → signed decimal degrees. */
function fromNmeaCoord(raw: string, hemi: string): number | null {
  const v = num(raw);
  if (v === null || !hemi) return null;
  const deg = Math.floor(v / 100);
  const min = v - deg * 100;
  if (min >= 60) return null;
  const dd = deg + min / 60;
  return hemi === 'S' || hemi === 'W' ? -dd : dd;
}

function nmeaChecksumOk(line: string): boolean {
  const star = line.lastIndexOf('*');
  if (star < 0) return true; // checksum optional in the wild — accept when absent
  let sum = 0;
  for (let i = 1; i < star; i++) sum ^= line.charCodeAt(i);
  const want = parseInt(line.slice(star + 1, star + 3), 16);
  return Number.isFinite(want) && sum === want;
}

/**
 * Latest valid fix from a blob of NMEA 0183 sentences (RMC preferred, GGA fallback — GGA carries
 * HDOP, surfaced as a rough accuracy of hdop × 5 m). Invalid/void sentences are skipped.
 */
export function parseNmea(text: string): GpsFix | null {
  let rmc: GpsFix | null = null;
  let gga: GpsFix | null = null;
  for (const raw of text.split(/[\r\n]+/)) {
    const line = raw.trim();
    if (!line.startsWith('$') || !nmeaChecksumOk(line)) continue;
    const body = line.slice(1, line.lastIndexOf('*') > 0 ? line.lastIndexOf('*') : undefined);
    const f = body.split(',');
    const type = (f[0] ?? '').slice(-3); // talker-agnostic: GPRMC, GNRMC, GLRMC...
    if (type === 'RMC' && f[2] === 'A') {
      const fix = validFix(fromNmeaCoord(f[3] ?? '', f[4] ?? ''), fromNmeaCoord(f[5] ?? '', f[6] ?? ''));
      if (fix) rmc = fix; // keep the LAST valid one — freshest
    } else if (type === 'GGA' && num(f[6]) !== null && Number(f[6]) > 0) {
      const hdop = num(f[8]);
      const fix = validFix(fromNmeaCoord(f[2] ?? '', f[3] ?? ''), fromNmeaCoord(f[4] ?? '', f[5] ?? ''), hdop !== null ? hdop * 5 : null);
      if (fix) gga = fix;
    }
  }
  return rmc ?? gga;
}

// --- Governor (report half of the app's gpsSmart rules, same constants) -------------------------

export const REPORT_MOVE_M = 25;
export const REPORT_MIN_MS = 30_000;

/** Great-circle distance in meters (haversine — exact enough at anchor-watch scale). */
export function distanceMeters(a: { lat: number; lon: number }, b: { lat: number; lon: number }): number {
  const R = 6371000;
  const toRad = (d: number) => (d * Math.PI) / 180;
  const dLat = toRad(b.lat - a.lat);
  const dLon = toRad(b.lon - a.lon);
  const h = Math.sin(dLat / 2) ** 2 + Math.cos(toRad(a.lat)) * Math.cos(toRad(b.lat)) * Math.sin(dLon / 2) ** 2;
  return 2 * R * Math.asin(Math.min(1, Math.sqrt(h)));
}

export interface ReportGovernor {
  /** Feed one fix; true = report it to the cloud. `now` injected for tests. */
  decide(fix: GpsFix, now: number): boolean;
}

/** @param heartbeatMs stationary report cadence — GPS_INTERVAL, in ms. */
export function createReportGovernor(heartbeatMs: number): ReportGovernor {
  let last: { lat: number; lon: number; at: number } | null = null;
  return {
    decide(fix, now) {
      let report = false;
      if (!last) report = true;
      else if (now - last.at >= REPORT_MIN_MS) {
        report = distanceMeters(last, fix) >= REPORT_MOVE_M || now - last.at >= heartbeatMs;
      }
      if (report) last = { lat: fix.lat, lon: fix.lon, at: now };
      return report;
    },
  };
}

/** `gps.measurement` params in the shell agent's exact shape: lat/lon at %.5f (≈1 m), acc whole m. */
export function fixToParams(fix: GpsFix): Record<string, string> {
  const params: Record<string, string> = { lat: fix.lat.toFixed(5), lon: fix.lon.toFixed(5) };
  if (fix.acc != null) params.acc = String(Math.round(fix.acc));
  return params;
}

// --- Line buffering ----------------------------------------------------------------------------

/** Max held partial-line bytes. A source that never sends a newline is garbage, not NMEA — drop. */
const BUFFER_MAX = 8192;

/**
 * Reassemble complete lines from arbitrary TCP chunk boundaries — a sentence split mid-coordinate
 * must not reach the parser as two garbage halves. Returns only completed lines; keeps the tail.
 */
export class LineBuffer {
  private tail = '';

  push(chunk: string): string {
    const data = this.tail + chunk;
    const lastBreak = Math.max(data.lastIndexOf('\n'), data.lastIndexOf('\r'));
    if (lastBreak < 0) {
      this.tail = data.length > BUFFER_MAX ? '' : data;
      return '';
    }
    this.tail = data.slice(lastBreak + 1);
    if (this.tail.length > BUFFER_MAX) this.tail = '';
    return data.slice(0, lastBreak + 1);
  }
}

// --- TCP client --------------------------------------------------------------------------------

const RECONNECT_MIN_MS = 5_000;
const RECONNECT_MAX_MS = 60_000;

export interface NmeaClientHandle {
  stop: () => void;
}

/**
 * Persistent TCP NMEA client with reconnect (backoff 5 s → 60 s, reset once data flows). Every
 * complete chunk of lines goes through `parseNmea`; each fix is handed to `onFix`. Errors are
 * logged, never thrown — a dead GPS source must not take the webhook receiver down with it, and
 * the gps.offline sweep server-side is what tells the owner the feed went quiet.
 */
export function startNmeaClient(
  opts: { host: string; port: number; log?: (m: string) => void },
  onFix: (fix: GpsFix) => void,
): NmeaClientHandle {
  const log = opts.log ?? (() => {});
  let stopped = false;
  let socket: Socket | null = null;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let delay = RECONNECT_MIN_MS;

  const connect = () => {
    if (stopped) return;
    const buf = new LineBuffer();
    const s = new Socket();
    socket = s;
    s.setEncoding('utf8');
    s.setTimeout(90_000); // a silent socket is a dead source; reconnect rather than trust it
    s.connect(opts.port, opts.host, () => log(`nmea connected to ${opts.host}:${opts.port}`));
    s.on('data', (chunk: string) => {
      delay = RECONNECT_MIN_MS; // data flowing — reset the backoff
      const lines = buf.push(chunk);
      if (!lines) return;
      const fix = parseNmea(lines);
      if (fix) onFix(fix);
    });
    s.on('timeout', () => s.destroy(new Error('no data for 90s')));
    s.on('error', () => {}); // 'close' handles scheduling; this only stops the process crashing
    s.on('close', () => {
      if (stopped) return;
      log(`nmea disconnected from ${opts.host}:${opts.port}, retrying in ${Math.round(delay / 1000)}s`);
      timer = setTimeout(connect, delay);
      timer.unref?.();
      delay = Math.min(delay * 2, RECONNECT_MAX_MS);
    });
  };

  connect();
  return {
    stop: () => {
      stopped = true;
      if (timer) clearTimeout(timer);
      socket?.destroy();
    },
  };
}
