// Cradlepoint GPS by HTTP POLL (owner ruling 2026-08-17: the hub/app POLLS the Cradlepoint — no
// stream, nothing configured on the router to send anywhere). Same model and same endpoint as the
// app's pollCradlepoint: `GET /api/status/gps` with Basic auth, on the GPS_INTERVAL timer. The
// parser is the app's parseCradlepointGps ported verbatim (NCOS DMS `{degree,minute,second}` with
// the sign riding on `degree`, decimal fallback, nested/flat envelopes); the test fixtures are
// copied byte-for-byte from gpsSources.test.ts — change them together.
//
// No governor here: one poll per interval IS the cadence (the shell hub-lite's push_gps behaves the
// same way), and the aggregator's unchanged-signature dedup means a stationary vessel rides the
// `ok` list instead of resending the same coordinates.

import type { GpsFix } from './nmea.js';

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

/** Degrees from Cradlepoint's `{degree, minute, second}` (sign rides on `degree`). */
function fromDms(v: any): number | null {
  const d = num(v?.degree);
  if (d === null) return null;
  const m = num(v?.minute) ?? 0;
  const s = num(v?.second) ?? 0;
  const sign = d < 0 || Object.is(d, -0) ? -1 : 1;
  return sign * (Math.abs(d) + Math.abs(m) / 60 + Math.abs(s) / 3600);
}

/** Cradlepoint NCOS `GET /api/status/gps` — fix under data.fix (or fix), DMS or decimal. */
export function parseCradlepointGps(body: any): GpsFix | null {
  const fix = body?.data?.fix ?? body?.fix ?? body?.data ?? body;
  if (!fix || typeof fix !== 'object') return null;
  let lat: number | null, lon: number | null;
  if (fix.latitude && typeof fix.latitude === 'object') {
    lat = fromDms(fix.latitude);
    lon = fromDms(fix.longitude);
  } else {
    lat = num(fix.latitude ?? fix.lat);
    lon = num(fix.longitude ?? fix.lon ?? fix.lng);
  }
  return validFix(lat, lon, num(fix.accuracy));
}

const POLL_TIMEOUT_MS = 15_000;

export interface CradlepointOpts {
  host: string;
  port: number;
  username: string;
  password: string;
}

/**
 * One poll: fetch the router's GPS status and parse it. Returns null on any failure — an offline
 * router or a fix-less antenna must not throw the poll loop down; the server-side gps.offline
 * sweep is what surfaces a feed that stays quiet.
 */
export async function pollCradlepoint(
  opts: CradlepointOpts,
  fetchImpl: typeof fetch = fetch,
): Promise<GpsFix | null> {
  const auth = Buffer.from(`${opts.username}:${opts.password}`).toString('base64');
  try {
    const res = await fetchImpl(`http://${opts.host}:${opts.port}/api/status/gps`, {
      headers: { Authorization: `Basic ${auth}` },
      signal: AbortSignal.timeout(POLL_TIMEOUT_MS),
    });
    if (!res.ok) return null;
    return parseCradlepointGps(await res.json());
  } catch {
    return null;
  }
}
