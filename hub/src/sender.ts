// POST a batch report to the worker's /api/agent/batch, authenticated by this hub's device token.
// Node 18+ global fetch — no runtime dependency, which keeps the Docker image tiny and the Pi
// install free of an npm tree.
//
// Phase B: the reply can carry queued commands ({commands:[{id,cmd}]}), and `ackIds` ride the
// request as &ack= — the same piggyback contract the shell agent speaks (worker agentCommands.ts).

import type { BatchReport } from './contract.js';
import type { HubConfig } from './config.js';

export interface PendingCommand { id: string; cmd: string }
/** Per-valve Normal Run profiles riding DOWN on the reply (worker: config-as-state). */
export interface WireValveProfiles { profiles?: Record<string, { durationSecs?: number; volumeCapL?: number; autoRestart?: boolean }> }
export interface SendResult { ok: boolean; commands: PendingCommand[]; linktap?: WireValveProfiles }
export type Sender = (report: BatchReport, ackIds?: string[]) => Promise<SendResult>;

export function makeSender(config: HubConfig, fetchImpl: typeof fetch = fetch): Sender {
  const base = `${config.workerBase}/api/agent/batch`
    + `?vid=${encodeURIComponent(config.vid)}`
    + `&device=${encodeURIComponent(config.deviceId)}`
    + `&t=${encodeURIComponent(config.token)}`;
  return async (report, ackIds = []) => {
    const url = ackIds.length ? `${base}&ack=${encodeURIComponent(ackIds.join(','))}` : base;
    try {
      const res = await fetchImpl(url, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(report),
        signal: AbortSignal.timeout(20_000),
      });
      // A duplicate (replayed seq) still returns 2xx with {duplicate:true} — that IS success:
      // the worker has the data, and the aggregator should stop retrying it.
      if (!res.ok) return { ok: false, commands: [] };
      let commands: PendingCommand[] = [];
      let linktap: WireValveProfiles | undefined;
      try {
        const body: any = await res.json();
        if (Array.isArray(body?.commands)) {
          commands = body.commands.filter(
            (c: any) => c && typeof c.id === 'string' && typeof c.cmd === 'string',
          );
        }
        // Config-as-state: the worker recomputes this from the vehicle's device records on every
        // delivery, so simply passing it through is what makes a restarted hub self-heal.
        if (body?.linktap && typeof body.linktap === 'object' && body.linktap.profiles) {
          linktap = { profiles: body.linktap.profiles };
        }
      } catch { /* a bodyless 2xx is still a delivered report */ }
      return { ok: true, commands, ...(linktap ? { linktap } : {}) };
    } catch {
      return { ok: false, commands: [] }; // network/timeout — the aggregator retries under the same seq
    }
  };
}
