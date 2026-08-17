// The parser cases are copied BYTE-FOR-BYTE from the app's gpsSources.test.ts
// (parseCradlepointGps describe block) — same keep-in-lockstep discipline as the NMEA and batch
// fixtures. If you change a case here, change it there in the same PR.

import { describe, it, expect, vi } from 'vitest';
import { parseCradlepointGps, pollCradlepoint } from '../src/cradlepoint.js';

describe('parseCradlepointGps (fixtures shared with the app)', () => {
  it('parses the NCOS DMS fix shape (sign on degree)', () => {
    const body = {
      data: {
        fix: {
          latitude: { degree: 45, minute: 33, second: 26.7 },
          longitude: { degree: -122, minute: 36, second: 18.0 },
          satellites: 8,
        },
      },
    };
    const fix = parseCradlepointGps(body)!;
    expect(fix.lat).toBeCloseTo(45 + 33 / 60 + 26.7 / 3600, 5);
    expect(fix.lon).toBeCloseTo(-(122 + 36 / 60 + 18 / 3600), 5);
  });

  it('parses decimal-degree variants and nested/flat shapes', () => {
    expect(parseCradlepointGps({ fix: { latitude: 39.1, longitude: -120.03 } })).toEqual({ lat: 39.1, lon: -120.03, acc: undefined });
    expect(parseCradlepointGps({ latitude: '39.1', longitude: '-120.03' })).toEqual({ lat: 39.1, lon: -120.03, acc: undefined });
  });

  it('rejects no-fix payloads', () => {
    expect(parseCradlepointGps({ data: { fix: null } })).toBeNull();
    expect(parseCradlepointGps({ data: { fix: { latitude: 0, longitude: 0 } } })).toBeNull();
    expect(parseCradlepointGps(undefined)).toBeNull();
  });
});

describe('pollCradlepoint', () => {
  const OPTS = { host: '192.168.0.1', port: 80, username: 'admin', password: 'pw' };

  it('GETs /api/status/gps with Basic auth and parses the fix', async () => {
    const fetchImpl = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({ data: { fix: { latitude: 41.49, longitude: -81.73 } } }),
    });
    const fix = await pollCradlepoint(OPTS, fetchImpl as unknown as typeof fetch);
    expect(fix).toEqual({ lat: 41.49, lon: -81.73, acc: undefined });
    const [url, init] = fetchImpl.mock.calls[0]!;
    expect(url).toBe('http://192.168.0.1:80/api/status/gps');
    expect((init.headers as Record<string, string>).Authorization)
      .toBe('Basic ' + Buffer.from('admin:pw').toString('base64'));
  });

  it('returns null (never throws) on HTTP errors, bad JSON, and network failure', async () => {
    const unauth = vi.fn().mockResolvedValue({ ok: false, json: async () => ({}) });
    expect(await pollCradlepoint(OPTS, unauth as unknown as typeof fetch)).toBeNull();
    const badJson = vi.fn().mockResolvedValue({ ok: true, json: async () => { throw new Error('nope'); } });
    expect(await pollCradlepoint(OPTS, badJson as unknown as typeof fetch)).toBeNull();
    const down = vi.fn().mockRejectedValue(new Error('ECONNREFUSED'));
    expect(await pollCradlepoint(OPTS, down as unknown as typeof fetch)).toBeNull();
  });

  it('returns null when the router answers without a fix', async () => {
    const noFix = vi.fn().mockResolvedValue({ ok: true, json: async () => ({ data: { fix: null } }) });
    expect(await pollCradlepoint(OPTS, noFix as unknown as typeof fetch)).toBeNull();
  });
});
