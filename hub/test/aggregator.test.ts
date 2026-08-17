import { describe, it, expect } from 'vitest';
import { Aggregator } from '../src/aggregator.js';
import type { BatchReport } from '../src/contract.js';

// A send() that records every report and answers with a scripted ok/fail sequence.
function recorder(oks: boolean[] = []) {
  const reports: BatchReport[] = [];
  let i = 0;
  const send = async (r: BatchReport) => {
    reports.push(structuredClone(r));
    const ok = i < oks.length ? oks[i]! : true;
    i += 1;
    return ok;
  };
  return { reports, send };
}

const item = (device: string, event: string, params: Record<string, string>) => ({ device, event, params });

describe('roll-up', () => {
  it('sends spooled telemetry as one delta, newest value per device+event', async () => {
    const agg = new Aggregator('0.1.0');
    agg.add(item('ht-a', 'humidity.change', { rh: '50' }));
    agg.add(item('ht-a', 'humidity.change', { rh: '55' })); // supersedes
    agg.add(item('pm-b', 'pm1.voltage_change', { v: '118' }));
    const { reports, send } = recorder();
    const r = await agg.drain(send);
    expect(r).toMatchObject({ sent: true, seq: 1, kind: 'delta', items: 2, ok: 0 });
    expect(reports[0]!.items).toEqual([
      item('ht-a', 'humidity.change', { rh: '55' }),
      item('pm-b', 'pm1.voltage_change', { v: '118' }),
    ]);
  });

  it('puts an UNCHANGED device on the ok list, not in items', async () => {
    const agg = new Aggregator('0.1.0');
    const { reports, send } = recorder();
    agg.add(item('ht-a', 'humidity.change', { rh: '55' }));
    await agg.drain(send); // seq 1: ht-a in items, remembered
    agg.add(item('ht-a', 'humidity.change', { rh: '55' })); // same value
    agg.add(item('pm-b', 'pm1.voltage_change', { v: '118' })); // new
    const r = await agg.drain(send); // seq 2
    expect(r).toMatchObject({ items: 1, ok: 1 });
    expect(reports[1]!.ok).toEqual(['ht-a']);
    expect(reports[1]!.items.map((i) => i.device)).toEqual(['pm-b']);
  });

  it('a keyframe resends everything even when unchanged', async () => {
    const agg = new Aggregator('0.1.0', 2); // keyframe every 2nd drain
    const { reports, send } = recorder();
    agg.add(item('ht-a', 'humidity.change', { rh: '55' }));
    await agg.drain(send); // seq 1 delta
    agg.add(item('ht-a', 'humidity.change', { rh: '55' })); // unchanged
    await agg.drain(send); // seq 2 → keyframe
    expect(reports[1]!.kind).toBe('keyframe');
    expect(reports[1]!.items.map((i) => i.device)).toEqual(['ht-a']); // resent despite no change
    expect(reports[1]!.ok).toEqual([]);
  });

  it('does nothing when the spool is empty', async () => {
    const agg = new Aggregator('0.1.0');
    const { send } = recorder();
    expect(await agg.drain(send)).toBeNull();
  });
});

describe('retry under the same seq (the replay-safety property)', () => {
  it('keeps a failed batch and re-sends it with the same seq before new data', async () => {
    const agg = new Aggregator('0.1.0');
    agg.add(item('ht-a', 'humidity.change', { rh: '55' }));
    const { reports, send } = recorder([false]); // first send fails
    const r1 = await agg.drain(send);
    expect(r1).toMatchObject({ sent: false, seq: 1 });

    // New telemetry arrives, but the next drain must retry seq 1 first — not skip it.
    agg.add(item('pm-b', 'pm1.voltage_change', { v: '118' }));
    const r2 = await agg.drain(send);
    expect(r2).toMatchObject({ sent: true, seq: 1 });      // same seq
    expect(reports[1]!.seq).toBe(1);
    expect(reports[1]!.items.map((i) => i.device)).toEqual(['ht-a']); // the ORIGINAL batch

    // Now the new data goes as seq 2.
    const r3 = await agg.drain(send);
    expect(r3).toMatchObject({ sent: true, seq: 2 });
    expect(reports[2]!.items.map((i) => i.device)).toEqual(['pm-b']);
  });

  it('does not advance last-sent state on a failed send — so the ok/changed split stays honest', async () => {
    const agg = new Aggregator('0.1.0');
    agg.add(item('ht-a', 'humidity.change', { rh: '55' }));
    const { reports, send } = recorder([false, true, true]);
    await agg.drain(send);                                   // seq1 fails
    await agg.drain(send);                                   // retries seq1, succeeds
    agg.add(item('ht-a', 'humidity.change', { rh: '55' }));  // same value as the (now) delivered one
    const r = await agg.drain(send);                          // seq2
    // ht-a is genuinely unchanged relative to what the cloud received, so it rides `ok`.
    expect(r).toMatchObject({ ok: 1, items: 0 });
    expect(reports[2]!.ok).toEqual(['ht-a']);
  });

  // A hub's seq lives in MEMORY, so a container bounce or power cut takes it back to 1 while the
  // cloud still holds the old high-water mark. Without a per-process boot id the worker reads that
  // as a replay, answers 200 {duplicate:true}, and the sender treats the loss as success.
  it('stamps a per-process boot id, stable across drains', async () => {
    const agg = new Aggregator('0.1.0', 6, 'boot-a');
    agg.add(item('ht-a', 'humidity.change', { rh: '55' }));
    const { reports, send } = recorder([true, true]);
    await agg.drain(send);
    agg.add(item('ht-a', 'humidity.change', { rh: '56' }));
    await agg.drain(send);
    expect(reports.map((r) => r.boot)).toEqual(['boot-a', 'boot-a']);
  });

  it('a RESTARTED hub reuses seq 1 but under a different boot id', async () => {
    const first = new Aggregator('0.1.0', 6, 'boot-a');
    first.add(item('ht-a', 'humidity.change', { rh: '55' }));
    const a = recorder([true]);
    await first.drain(a.send);

    const afterRestart = new Aggregator('0.1.0', 6, 'boot-b');   // fresh process: seq restarts
    afterRestart.add(item('ht-a', 'humidity.change', { rh: '99' }));
    const b = recorder([true]);
    await afterRestart.drain(b.send);

    expect(a.reports[0]!.seq).toBe(1);
    expect(b.reports[0]!.seq).toBe(1);          // the collision that used to read as a replay
    expect(b.reports[0]!.boot).not.toBe(a.reports[0]!.boot);
  });

  it('defaults to a unique boot id per instance when none is supplied', () => {
    const ids = new Set([new Aggregator('0.1.0'), new Aggregator('0.1.0')]
      .map((x) => (x as unknown as { boot: string }).boot));
    expect(ids.size).toBe(2);
  });
});
