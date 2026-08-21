// The parser cases here are copied BYTE-FOR-BYTE from the app's gpsSources.test.ts (parseNmea
// describe block) — the hub's parser is a port of the app's, and these shared sentences are what
// keep the two from drifting, same discipline as the batch-contract fixture. If you change a
// sentence here, change it there in the same PR.

import { describe, it, expect, vi, afterEach } from 'vitest';
import { createServer, type Server } from 'node:net';

import {
  parseNmea, createReportGovernor, distanceMeters, fixToParams, LineBuffer, startNmeaClient,
  REPORT_MOVE_M, REPORT_MIN_MS,
} from '../src/nmea.js';

describe('parseNmea (fixtures shared with the app)', () => {
  // 4807.038,N = 48°07.038' → 48.1173; 01131.000,E = 11°31.000' → 11.516667
  const RMC = '$GPRMC,123519,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W*6A';
  const GGA = '$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47';

  it('parses an RMC sentence with checksum', () => {
    const fix = parseNmea(RMC)!;
    expect(fix.lat).toBeCloseTo(48.1173, 4);
    expect(fix.lon).toBeCloseTo(11.516667, 4);
  });

  it('parses GGA (with HDOP-derived rough accuracy) when no RMC is present', () => {
    const fix = parseNmea(GGA)!;
    expect(fix.lat).toBeCloseTo(48.1173, 4);
    expect(fix.acc).toBeCloseTo(0.9 * 5, 5);
  });

  it('prefers RMC over GGA and uses the LAST valid RMC in the blob', () => {
    const rmc2 = '$GPRMC,123520,A,3600.000,S,07200.000,W,0.0,0.0,230394,,';
    const fix = parseNmea(`${GGA}\r\n${RMC}\r\n${rmc2}`)!;
    expect(fix.lat).toBeCloseTo(-36.0, 5);
    expect(fix.lon).toBeCloseTo(-72.0, 5);
  });

  it('skips void fixes, bad checksums, and non-NMEA noise', () => {
    const voidRmc = '$GPRMC,123519,V,4807.038,N,01131.000,E,,,230394,,';
    const badSum = '$GPRMC,123519,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W*00';
    expect(parseNmea(voidRmc)).toBeNull();
    expect(parseNmea(badSum)).toBeNull();
    expect(parseNmea('hello\nnot nmea\n')).toBeNull();
    expect(parseNmea('')).toBeNull();
  });

  it('handles talker variants (GNRMC) and sentences without checksums', () => {
    const gn = '$GNRMC,123519,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W';
    const fix = parseNmea(gn)!;
    expect(fix.lat).toBeCloseTo(48.1173, 4);
  });

  it('rejects malformed coordinates (minutes >= 60)', () => {
    const bad = '$GPRMC,123519,A,4867.000,N,01131.000,E,0.0,0.0,230394,,';
    expect(parseNmea(bad)).toBeNull();
  });
});

describe('report governor (report half of the app gpsSmart rules)', () => {
  const at = (lat: number, lon: number) => ({ lat, lon });
  // ~0.00028° lat ≈ 31 m — past REPORT_MOVE_M; ~0.0001° ≈ 11 m — under it.
  const HOME = at(41.49, -81.73);
  const NEAR = at(41.4901, -81.73); // ~11 m
  const FAR = at(41.49028, -81.73); // ~31 m

  it('reports the first fix immediately', () => {
    const g = createReportGovernor(120_000);
    expect(g.decide(HOME, 0)).toBe(true);
  });

  it('never reports again inside REPORT_MIN_MS, even for big movement', () => {
    const g = createReportGovernor(120_000);
    g.decide(HOME, 0);
    expect(g.decide(FAR, REPORT_MIN_MS - 1)).toBe(false);
  });

  it('reports movement ≥ REPORT_MOVE_M once the floor has passed, but not a swing at anchor', () => {
    const g = createReportGovernor(120_000);
    g.decide(HOME, 0);
    expect(g.decide(NEAR, REPORT_MIN_MS)).toBe(false); // ~11 m swing — stays quiet
    expect(g.decide(FAR, REPORT_MIN_MS + 1)).toBe(true); // ~31 m — dragging
  });

  it('sends a stationary heartbeat at the GPS_INTERVAL cadence', () => {
    const g = createReportGovernor(120_000);
    g.decide(HOME, 0);
    expect(g.decide(HOME, 119_999)).toBe(false);
    expect(g.decide(HOME, 120_000)).toBe(true);
  });

  it('distanceMeters sanity: the fixture distances bracket REPORT_MOVE_M', () => {
    expect(distanceMeters(HOME, NEAR)).toBeLessThan(REPORT_MOVE_M);
    expect(distanceMeters(HOME, FAR)).toBeGreaterThan(REPORT_MOVE_M);
  });
});

describe('fixToParams (the shell hub-lite wire shape)', () => {
  it('formats lat/lon at 5 decimals and acc as whole meters', () => {
    expect(fixToParams({ lat: 41.49000123, lon: -81.73, acc: 4.5 }))
      .toEqual({ lat: '41.49000', lon: '-81.73000', acc: '5' });
  });
  it('omits acc when the source gave none', () => {
    expect(fixToParams({ lat: 1, lon: 2 })).toEqual({ lat: '1.00000', lon: '2.00000' });
  });
});

describe('LineBuffer (TCP chunk reassembly)', () => {
  it('holds a partial sentence until its newline arrives', () => {
    const b = new LineBuffer();
    expect(b.push('$GPRMC,123519,A,4807.038,N,')).toBe('');
    const lines = b.push('01131.000,E,022.4,084.4,230394,003.1,W*6A\r\n');
    expect(parseNmea(lines)!.lat).toBeCloseTo(48.1173, 4);
  });

  it('returns completed lines and keeps only the tail', () => {
    const b = new LineBuffer();
    const lines = b.push('$A,1*00\n$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47\n$partial');
    expect(lines).toContain('GPGGA');
    expect(b.push('\n')).toBe('$partial\n');
  });

  it('drops a runaway newline-less stream instead of buffering it forever', () => {
    const b = new LineBuffer();
    b.push('x'.repeat(10_000));
    expect(b.push('$GPRMC\n')).toBe('$GPRMC\n'); // the junk was discarded, not prepended
  });
});

describe('startNmeaClient (integration, real socket)', () => {
  let server: Server | null = null;
  afterEach(() => new Promise<void>((r) => { server ? server.close(() => r()) : r(); server = null; }));

  it('connects, reassembles split sentences, and hands fixes to onFix', async () => {
    const RMC = '$GPRMC,123519,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W*6A\r\n';
    server = createServer((sock) => {
      // Split mid-coordinate across two writes — the buffer must reassemble.
      sock.write(RMC.slice(0, 30));
      setTimeout(() => sock.write(RMC.slice(30)), 20);
    });
    await new Promise<void>((r) => server!.listen(0, '127.0.0.1', () => r()));
    const port = (server!.address() as { port: number }).port;

    const fix = await new Promise<{ lat: number }>((resolve) => {
      const client = startNmeaClient({ host: '127.0.0.1', port }, (f) => {
        client.stop();
        resolve(f);
      });
    });
    expect(fix.lat).toBeCloseTo(48.1173, 4);
  });

  it('stop() prevents reconnect attempts after the peer closes', async () => {
    vi.useFakeTimers();
    try {
      server = createServer((sock) => sock.destroy());
      await new Promise<void>((r) => server!.listen(0, '127.0.0.1', () => r()));
      const port = (server!.address() as { port: number }).port;
      const log = vi.fn();
      const client = startNmeaClient({ host: '127.0.0.1', port, log }, () => {});
      client.stop();
      await vi.advanceTimersByTimeAsync(120_000);
      expect(log.mock.calls.filter(([m]) => String(m).includes('retrying'))).toHaveLength(0);
    } finally {
      vi.useRealTimers();
    }
  });
});
