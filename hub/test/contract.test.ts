import { describe, it, expect } from 'vitest';
import { buildReport, buildUrgent, HUB_TIER } from '../src/contract.js';

// The SAME canonical v1 fixture the worker (agentBatch.test.ts) and the shell hub-lite (hub-lite/test.sh)
// check against — copied, not paraphrased. Cross-tier drift shows up as a red test somewhere.
const CANONICAL_SHAPE = {
  v: 1, seq: 42, boot: 'boot-a', kind: 'delta',
  items: [
    { device: 'shellyflood-a1', event: 'flood.alarm', params: { temp: '12.5' } },
    { device: 'shellyuni-b2', event: 'voltmeter.measurement', params: { v: '12.6' } },
  ],
  ok: ['shellyht-c3', 'shellypm-d4'],
};

describe('the hub speaks v1', () => {
  it('builds a report the worker parser accepts, tagged tier=hub', () => {
    const r = buildReport({
      seq: 42, boot: 'boot-a', kind: 'delta', items: CANONICAL_SHAPE.items, ok: CANONICAL_SHAPE.ok,
      version: '0.1.0',
    });
    expect(r.v).toBe(1);
    expect(r.seq).toBe(42);
    expect(r.boot).toBe('boot-a');
    expect(r.kind).toBe('delta');
    expect(r.items).toEqual(CANONICAL_SHAPE.items);
    expect(r.ok).toEqual(CANONICAL_SHAPE.ok);
    expect(r.agent).toEqual({ av: '0.1.0', tier: 'hub' });
    expect(HUB_TIER).toBe('hub');
  });

  it('serializes to the exact wire shape (no extra fields the worker would reject)', () => {
    const r = buildReport({ seq: 1, boot: 'boot-a', kind: 'keyframe', items: [], ok: [], version: '0.1.0' });
    expect(Object.keys(JSON.parse(JSON.stringify(r))).sort())
      .toEqual(['agent', 'boot', 'items', 'kind', 'ok', 'seq', 'v']);
  });

  it('the urgent envelope carries no seq (it never retries)', () => {
    const u = buildUrgent({ device: 'd', event: 'flood.alarm', params: {} }, '0.1.0');
    expect(u.seq).toBeUndefined();
    expect(u.items).toHaveLength(1);
    expect(u.agent?.tier).toBe('hub');
  });
});
