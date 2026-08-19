import { describe, it, expect, vi } from 'vitest';
import {
  LT_CMD, MAX_PLAUSIBLE_CYCLE_VOLUME, LITRES_PER_GALLON,
  extractJsonFromMaybeHtml, coerceWateringBool, reportsVolume, cycleVolumeLitres,
  buildStatus, buildStart, buildStop, buildGetConfiguration, buildDeleteWaterPlan,
  buildAddValve, buildRemoveValve, postCommand, readVolUnit,
} from '../src/linktap.js';

const GW = { host: '192.168.1.107', gatewayId: 'CCCCDDDDEEEEFFFF' };
const DEV = 'aaaabbbbccccdddd';

describe('response unwrapping', () => {
  it('pulls the JSON out of the gateway HTML wrapper it uses by default', () => {
    // The gateway wraps replies unless an operator disables it in the admin page, so we cannot
    // assume the raw form.
    const html = `<html><head><title>api</title></head><body>
      <!--#RET-->{"cmd":6,"gw_id":"CCCCDDDDEEEEFFFF","ret":0}
    </body></html>`;
    expect(JSON.parse(extractJsonFromMaybeHtml(html))).toEqual({
      cmd: 6, gw_id: 'CCCCDDDDEEEEFFFF', ret: 0,
    });
  });

  it('passes bare JSON through untouched', () => {
    expect(extractJsonFromMaybeHtml('{"cmd":3}')).toBe('{"cmd":3}');
  });
});

describe('coerceWateringBool', () => {
  it('accepts every spelling the firmware uses', () => {
    for (const v of [true, 'true', 1, '1']) expect(coerceWateringBool(v)).toBe(true);
    for (const v of [false, 'false', 0, '0', null, undefined]) expect(coerceWateringBool(v)).toBe(false);
  });
});

describe('reportsVolume', () => {
  it('trusts is_flm_plugin when the gateway sends it', () => {
    expect(reportsVolume({ is_flm_plugin: true })).toBe(true);
    expect(reportsVolume({ is_flm_plugin: false, volume: 1.2 })).toBe(false); // flag wins
  });

  it('falls back to a numeric volume field when the flag is absent', () => {
    expect(reportsVolume({ volume: 1.2 })).toBe(true);
    expect(reportsVolume({})).toBe(false);
  });
});

describe('cycleVolumeLitres — the field that has been misread twice', () => {
  it('reads the CYCLE TOTAL in the gateway unit, converting gallons to litres', () => {
    // 0.63 gal was the real opening reading captured on MVP's GW-02 mid-cycle.
    expect(cycleVolumeLitres({ volume: 0.63 }, 'gal')).toBeCloseTo(0.63 * LITRES_PER_GALLON, 6);
  });

  it('does not convert when the gateway is already reporting litres', () => {
    expect(cycleVolumeLitres({ volume: 2 }, 'L')).toBe(2);
  });

  it('rejects the idle garbage latch a CLOSED valve reports', () => {
    // Measured: a closed GW-02 sat at 15,886,307.00. Letting that reach a limit comparison would
    // instantly "exceed" any cap.
    expect(cycleVolumeLitres({ volume: 15_886_307 }, 'gal')).toBe(0);
    expect(cycleVolumeLitres({ volume: MAX_PLAUSIBLE_CYCLE_VOLUME + 1 }, 'gal')).toBe(0);
  });

  it('treats missing or nonsense readings as no reading, never as a number', () => {
    expect(cycleVolumeLitres({}, 'gal')).toBe(0);
    expect(cycleVolumeLitres({ volume: -1 }, 'gal')).toBe(0);
    expect(cycleVolumeLitres({ volume: 'abc' }, 'gal')).toBe(0);
  });
});

describe('command builders speak the app\'s dialect, not a second one', () => {
  it('status is cmd 3 with a singular dev_id', () => {
    expect(buildStatus(GW, DEV)).toEqual({ cmd: 3, gw_id: GW.gatewayId, dev_id: DEV });
  });

  it('start is cmd 6 and duration is SECONDS', () => {
    expect(buildStart(GW, DEV, 900, 50)).toEqual({
      cmd: 6, gw_id: GW.gatewayId, dev_id: DEV, duration: 900, volume_limit: 50,
    });
  });

  it('omits volume_limit when there is none — a time-only run must not carry a phantom cap', () => {
    // Washdown is time-limited with NO volume cap (owner ruling, re-ratified 2026-08-19).
    expect(buildStart(GW, DEV, 7200)).toEqual({
      cmd: 6, gw_id: GW.gatewayId, dev_id: DEV, duration: 7200,
    });
    expect(buildStart(GW, DEV, 7200, 0)).not.toHaveProperty('volume_limit');
  });

  it('floors a fractional duration and never sends a zero-second run', () => {
    expect(buildStart(GW, DEV, 0.4).duration).toBe(1);
    expect(buildStart(GW, DEV, 90.9).duration).toBe(90);
  });

  it('stop and get-configuration carry what the gateway needs and nothing else', () => {
    expect(buildStop(GW, DEV)).toEqual({ cmd: 7, gw_id: GW.gatewayId, dev_id: DEV });
    expect(buildGetConfiguration(GW)).toEqual({ cmd: 16, gw_id: GW.gatewayId });
  });

  it('offers plan DELETE but not plan write — the hub owns the schedule', () => {
    // A plan left in the gateway would fire on its own clock with nothing reconciling it against
    // the hub's cycle state machine.
    expect(buildDeleteWaterPlan(GW, DEV)).toEqual({ cmd: 5, gw_id: GW.gatewayId, dev_id: DEV });
    expect(LT_CMD.SETUP_WATER_PLAN).toBe(4); // defined for completeness, deliberately unbuilt
  });
});

describe('pairing without the LinkTap app (CMD 1 / 2)', () => {
  it('uses end_dev as an ARRAY — not the dev_id every other command takes', () => {
    // This is the single field nobody would guess. Source: the Hubitat MQTT driver's
    // 'register device' message, valid over HTTP because the vendor defines both transports as
    // carrying the same messages.
    expect(buildAddValve(GW, [DEV])).toEqual({ cmd: 1, gw_id: GW.gatewayId, end_dev: [DEV] });
    expect(buildRemoveValve(GW, [DEV])).toEqual({ cmd: 2, gw_id: GW.gatewayId, end_dev: [DEV] });
  });

  it('carries several ids in one call', () => {
    expect(buildAddValve(GW, [DEV, 'bbbbccccddddeeee']).end_dev).toHaveLength(2);
  });

  it('never emits a singular dev_id for these two, which the gateway would ignore', () => {
    expect(buildAddValve(GW, [DEV])).not.toHaveProperty('dev_id');
    expect(buildRemoveValve(GW, [DEV])).not.toHaveProperty('dev_id');
  });
});

describe('postCommand', () => {
  const okRes = (body: string) => ({ ok: true, status: 200, text: async () => body }) as any;

  it('POSTs JSON to api.shtml on the gateway', async () => {
    const fetchImpl = vi.fn(async () => okRes('{"cmd":7,"ret":0}')) as any;
    const r = await postCommand(GW, buildStop(GW, DEV), fetchImpl);
    expect(r.ok).toBe(true);
    const [url, init] = fetchImpl.mock.calls[0];
    expect(url).toBe('http://192.168.1.107/api.shtml');
    expect(init.method).toBe('POST');
    expect(JSON.parse(init.body)).toEqual({ cmd: 7, gw_id: GW.gatewayId, dev_id: DEV });
  });

  it('unwraps an HTML-wrapped reply on the real path', async () => {
    const fetchImpl = vi.fn(async () =>
      okRes('<html><body><!--#RET-->{"cmd":6,"ret":0}</body></html>')) as any;
    expect((await postCommand(GW, buildStart(GW, DEV, 60), fetchImpl)).ok).toBe(true);
  });

  it('treats a non-zero ret as failure and surfaces the code', async () => {
    const fetchImpl = vi.fn(async () => okRes('{"cmd":1,"ret":3}')) as any;
    const r = await postCommand(GW, buildAddValve(GW, [DEV]), fetchImpl);
    expect(r.ok).toBe(false);
    expect(r.ret).toBe(3);
  });

  it('treats an ABSENT ret as success — status replies carry a payload instead', async () => {
    const fetchImpl = vi.fn(async () => okRes('{"cmd":3,"dev_stat":[{"dev_id":"x"}]}')) as any;
    const r = await postCommand(GW, buildStatus(GW, DEV), fetchImpl);
    expect(r.ok).toBe(true);
    expect(r.data.dev_stat).toHaveLength(1);
  });

  it('never throws — an unreachable gateway degrades to ok:false so the poll loop survives', async () => {
    const boom = vi.fn(async () => { throw new Error('ECONNREFUSED'); }) as any;
    const r = await postCommand(GW, buildStop(GW, DEV), boom);
    expect(r.ok).toBe(false);
    expect(r.error).toContain('ECONNREFUSED');
  });

  it('reports an HTTP error status rather than pretending it parsed', async () => {
    const fetchImpl = vi.fn(async () => ({ ok: false, status: 500, text: async () => '' })) as any;
    expect((await postCommand(GW, buildStop(GW, DEV), fetchImpl)).error).toContain('500');
  });

  it('reports unparseable bodies instead of throwing', async () => {
    const fetchImpl = vi.fn(async () => okRes('not json at all')) as any;
    expect((await postCommand(GW, buildStop(GW, DEV), fetchImpl)).error).toMatch(/not JSON/i);
  });
});

describe('readVolUnit', () => {
  it('reads the unit from cmd 16', async () => {
    const fetchImpl = vi.fn(async () => ({
      ok: true, status: 200, text: async () => '{"cmd":16,"vol_unit":"L","ret":0}',
    })) as any;
    expect(await readVolUnit(GW, fetchImpl)).toBe('L');
  });

  it('defaults to GALLONS when the gateway cannot be read', async () => {
    // Guessing litres would under-report by 3.79x, and the software cutoff compares against it.
    const boom = vi.fn(async () => { throw new Error('offline'); }) as any;
    expect(await readVolUnit(GW, boom)).toBe('gal');
  });
});
