import { describe, it, expect, vi } from 'vitest';
import { LinkTapRuntime, parseGatewayPush } from '../src/linktapRuntime.js';
import type { BatchItem } from '../src/contract.js';

const T0 = Date.UTC(2026, 7, 19, 12, 0, 0);
const DEV = 'aaaabbbbccccdddd';

/** A scriptable gateway: answers cmd 16 with a unit, records every command body it receives. */
function fakeGateway(volUnit: 'gal' | 'L' = 'gal') {
  const commands: any[] = [];
  const fetchImpl = vi.fn(async (_url: any, init: any) => {
    const body = JSON.parse(init.body);
    commands.push(body);
    if (body.cmd === 16) return { ok: true, status: 200, text: async () => JSON.stringify({ cmd: 16, vol_unit: volUnit, ret: 0 }) } as any;
    return { ok: true, status: 200, text: async () => '{"ret":0}' } as any;
  });
  return { fetchImpl, commands };
}

function makeRuntime(gw: ReturnType<typeof fakeGateway>, overrides: any = {}) {
  const spooled: BatchItem[] = [];
  let now = T0;
  const rt = new LinkTapRuntime({
    target: { host: '172.31.0.244', gatewayId: 'GW02' },
    devIds: [DEV],
    profile: { durationSecs: 24 * 3600, volumeCapL: 100 },
    autoRestart: false,
    spool: (i) => spooled.push(i),
    fetchImpl: gw.fetchImpl as any,
    now: () => now,
    ...overrides,
  });
  return { rt, spooled, advance: (ms: number) => { now += ms; } };
}

describe('the software volume cut, end to end', () => {
  it('watches volume climb, issues the stop at the cap, and never double-issues', async () => {
    const gw = fakeGateway('gal');
    const { rt, advance } = makeRuntime(gw);
    await rt.ensureVolUnit();

    await rt.observe(DEV, { is_watering: 1, volume: 5, remain_duration: 80000 }); // adopted, climbing
    advance(60_000);
    // 27 gal ≈ 102 L — past the 100 L cap
    await rt.observe(DEV, { is_watering: 1, volume: 27, remain_duration: 79940 });
    const stops = gw.commands.filter((c) => c.cmd === 7);
    expect(stops).toHaveLength(1);

    advance(15_000);
    await rt.observe(DEV, { is_watering: 1, volume: 27.5, remain_duration: 79925 }); // still closing
    expect(gw.commands.filter((c) => c.cmd === 7)).toHaveLength(1); // no re-issue storm
  });

  it('classifies the close as volume_cap and does NOT restart even with auto-restart on', async () => {
    const gw = fakeGateway('gal');
    const { rt, spooled, advance } = makeRuntime(gw, { autoRestart: true });
    await rt.ensureVolUnit();
    await rt.observe(DEV, { is_watering: 1, volume: 5 });
    advance(60_000);
    await rt.observe(DEV, { is_watering: 1, volume: 27 }); // cut issued
    advance(30_000);
    await rt.observe(DEV, { is_watering: 0, volume: 27 }); // valve confirms closed
    const end = spooled.find((i) => i.event === 'linktap.cycle.change');
    expect(end?.params.reason).toBe('volume_cap');
    expect(gw.commands.filter((c) => c.cmd === 6)).toHaveLength(0); // no restart start
  });
});

describe('auto-restart on a genuine timer expiry', () => {
  it('starts a fresh Normal Run carrying duration AND a cap in the gateway unit', async () => {
    const gw = fakeGateway('gal');
    const { rt, advance } = makeRuntime(gw, { autoRestart: true });
    await rt.ensureVolUnit();
    const ok = await rt.startNormalRun(DEV);
    expect(ok).toBe(true);
    const start = gw.commands.find((c) => c.cmd === 6);
    expect(start.duration).toBe(24 * 3600);
    // 100 L in a gal-unit gateway ≈ 26.42 gal — the cap converts to the GATEWAY's unit
    expect(start.volume_limit).toBeCloseTo(26.42, 1);

    // run to natural expiry
    advance(24 * 3600 * 1000 - 30_000);
    await rt.observe(DEV, { is_watering: 1, volume: 10 });
    advance(40_000);
    await rt.observe(DEV, { is_watering: 0, volume: 10.2 }); // closed within a minute of duration
    expect(gw.commands.filter((c) => c.cmd === 6)).toHaveLength(2); // restarted
  });
});

describe('flood shutoff', () => {
  it('closes every watched valve and classifies the end as flood_shutoff', async () => {
    const gw = fakeGateway('gal');
    const { rt, spooled, advance } = makeRuntime(gw);
    await rt.ensureVolUnit();
    await rt.observe(DEV, { is_watering: 1, volume: 3 });
    await rt.floodStopAll();
    expect(gw.commands.filter((c) => c.cmd === 7)).toHaveLength(1);
    advance(20_000);
    await rt.observe(DEV, { is_watering: 0, volume: 3.1 });
    const end = spooled.find((i) => i.event === 'linktap.cycle.change');
    expect(end?.params.reason).toBe('flood_shutoff');
  });
});

describe('the ledger rides the telemetry', () => {
  it('day totals appear as params on linktap.measurement after a cycle ends', async () => {
    const gw = fakeGateway('L'); // litre gateway: volume passes through unconverted
    const { rt, spooled, advance } = makeRuntime(gw);
    await rt.ensureVolUnit();
    await rt.observe(DEV, { is_watering: 1, volume: 40 });
    advance(60_000);
    await rt.observe(DEV, { is_watering: 0, volume: 42 });
    advance(30_000);
    await rt.observe(DEV, { is_watering: 0, volume: 0 });
    const last = spooled.filter((i) => i.event === 'linktap.measurement').pop();
    expect(last?.params.day).toBe('2026-08-19');
    expect(Number(last?.params.day_vol_l)).toBeCloseTo(42, 1);
    expect(last?.device).toBe(`lt_${DEV}`);
  });
});

describe('unit safety', () => {
  it('a gal gateway converts to litres before the cap comparison', async () => {
    const gw = fakeGateway('gal');
    const { rt, advance } = makeRuntime(gw); // cap 100 L
    await rt.ensureVolUnit();
    await rt.observe(DEV, { is_watering: 1, volume: 5 });
    advance(30_000);
    // 26 gal = 98.4 L — UNDER the cap; a raw comparison (26 < 100) would also pass, but
    // 27 gal = 102.2 L must cut, where a raw comparison (27 < 100) would NOT.
    await rt.observe(DEV, { is_watering: 1, volume: 26 });
    expect(gw.commands.filter((c) => c.cmd === 7)).toHaveLength(0);
    advance(30_000);
    await rt.observe(DEV, { is_watering: 1, volume: 27 });
    expect(gw.commands.filter((c) => c.cmd === 7)).toHaveLength(1);
  });

  it('the idle garbage latch never reaches the cap comparison', async () => {
    const gw = fakeGateway('gal');
    const { rt } = makeRuntime(gw);
    await rt.ensureVolUnit();
    // A CLOSED valve latches ~15.9M. If it leaked through, adoption would instantly "cut".
    await rt.observe(DEV, { is_watering: 1, volume: 15_886_307 });
    expect(gw.commands.filter((c) => c.cmd === 7)).toHaveLength(0);
  });
});

describe('valves this hub does not watch', () => {
  it('ignores an unknown dev_id entirely', async () => {
    const gw = fakeGateway('gal');
    const { rt, spooled } = makeRuntime(gw);
    await rt.ensureVolUnit();
    await rt.observe('ffffeeeeddddcccc', { is_watering: 1, volume: 5 });
    expect(spooled).toHaveLength(0);
  });
});

describe('parseGatewayPush', () => {
  it('reads a dev_stat array push', () => {
    const out = parseGatewayPush(JSON.stringify({ gw_id: 'GW02', dev_stat: [{ dev_id: DEV, is_watering: 1 }] }));
    expect(out).toHaveLength(1);
    expect(out[0].devId).toBe(DEV);
  });

  it('reads a single-device push and truncates long ids to the canonical 16', () => {
    const out = parseGatewayPush(JSON.stringify({ dev_id: DEV + '0042', is_watering: 0 }));
    expect(out[0].devId).toBe(DEV);
  });

  it('returns nothing for junk without throwing', () => {
    expect(parseGatewayPush('not json')).toEqual([]);
    expect(parseGatewayPush('{}')).toEqual([]);
  });
});

describe('profiles over the wire (config-as-state)', () => {
  it('a wire profile overrides the env default per valve, field by field', async () => {
    const gw = fakeGateway('L');
    const { rt } = makeRuntime(gw); // env: 24h / 100 L
    await rt.ensureVolUnit();
    rt.applyProfiles({ [DEV]: { volumeCapL: 250 } }); // duration unset ⇒ env default holds
    const p = rt.profileFor(DEV);
    expect(p.volumeCapL).toBe(250);
    expect(p.durationSecs).toBe(24 * 3600);
  });

  it('the software cutoff uses the wire cap the moment it applies', async () => {
    const gw = fakeGateway('L');
    const { rt, advance } = makeRuntime(gw);
    await rt.ensureVolUnit();
    rt.applyProfiles({ [DEV]: { volumeCapL: 30 } });
    await rt.observe(DEV, { is_watering: 1, volume: 10 });
    advance(60_000);
    await rt.observe(DEV, { is_watering: 1, volume: 31 }); // over the WIRE cap, under the env 100
    expect(gw.commands.filter((c) => c.cmd === 7)).toHaveLength(1);
  });

  it('wire autoRestart drives the restart decision', async () => {
    const gw = fakeGateway('L');
    const { rt, advance } = makeRuntime(gw, { autoRestart: false }); // env says no
    await rt.ensureVolUnit();
    rt.applyProfiles({ [DEV]: { autoRestart: true, durationSecs: 600, volumeCapL: 100 } });
    await rt.startNormalRun(DEV);
    advance(590_000);
    await rt.observe(DEV, { is_watering: 1, volume: 10 });
    advance(20_000);
    await rt.observe(DEV, { is_watering: 0, volume: 10 }); // timer expiry
    expect(gw.commands.filter((c) => c.cmd === 6)).toHaveLength(2); // restarted per the wire profile
  });

  it('ignores profiles for valves this hub does not watch, and normalises long ids', async () => {
    const gw = fakeGateway('L');
    const { rt } = makeRuntime(gw);
    rt.applyProfiles({ ffffeeeeddddcccc: { volumeCapL: 1 }, [DEV + '0042']: { volumeCapL: 77 } });
    expect(rt.profileFor(DEV).volumeCapL).toBe(77); // long id normalised onto the watched valve
  });

  it('startNormalRun issues the wire duration and cap', async () => {
    const gw = fakeGateway('L');
    const { rt } = makeRuntime(gw);
    await rt.ensureVolUnit();
    rt.applyProfiles({ [DEV]: { durationSecs: 7200, volumeCapL: 50 } });
    await rt.startNormalRun(DEV);
    const start = gw.commands.find((c) => c.cmd === 6);
    expect(start.duration).toBe(7200);
    expect(start.volume_limit).toBe(50); // litre gateway: no conversion
  });
});
