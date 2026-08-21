// The batch wire contract (v1) — the TypeScript hub's half of the SAME format the shell hub-lite and
// the worker speak (brvg-cloud-server/src/agentBatch.ts, hub-lite/brvg-hub-lite.sh). The canonical
// fixture in test/contract.test.ts is copied byte-for-byte from the worker's agentBatch.test.ts:
// if any tier changes shape, a test goes red somewhere. A hub-lite is a subset of a hub, never a
// variant — this file must never emit a field the worker's parser would reject.

export interface BatchItem {
  device: string;
  event: string;
  params: Record<string, string>;
}

export interface BatchReport {
  v: 1;
  seq?: number;
  /**
   * Per-process id, so the cloud can tell "this hub restarted and its counter reset" from "this is
   * a replay". The hub's seq lives in MEMORY ONLY, so every restart — a container bounce, a Pi
   * power cut — takes it back to 1 while the cloud still holds the old high-water mark. Without
   * this the worker answers `200 {duplicate:true}`, the sender reads that as success, and the
   * batch is dropped on both ends until the counter climbs back. See `isNewSeq` in
   * brvg-cloud-server/src/agentBatch.ts.
   */
  boot?: string;
  kind: 'keyframe' | 'delta';
  items: BatchItem[];
  ok: string[];
  agent?: { av?: string; tier?: string };
}

export const HUB_TIER = 'hub';

export function buildReport(input: {
  seq: number;
  boot: string;
  kind: 'keyframe' | 'delta';
  items: BatchItem[];
  ok: string[];
  version: string;
}): BatchReport {
  return {
    v: 1,
    seq: input.seq,
    boot: input.boot,
    kind: input.kind,
    items: input.items,
    ok: input.ok,
    agent: { av: input.version, tier: HUB_TIER },
  };
}

/** The single-item envelope the URGENT path sends immediately (no seq — it never retries). */
export function buildUrgent(item: BatchItem, version: string): BatchReport {
  return { v: 1, kind: 'delta', items: [item], ok: [], agent: { av: version, tier: HUB_TIER } };
}
