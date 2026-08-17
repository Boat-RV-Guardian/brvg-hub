// POST a batch report to the worker's /api/agent/batch, authenticated by this hub's device token.
// Node 18+ global fetch — no runtime dependency, which keeps the Docker image tiny and the Pi
// install free of an npm tree.

import type { BatchReport } from './contract.js';
import type { HubConfig } from './config.js';

export type Sender = (report: BatchReport) => Promise<boolean>;

export function makeSender(config: HubConfig, fetchImpl: typeof fetch = fetch): Sender {
  const url = `${config.workerBase}/api/agent/batch`
    + `?vid=${encodeURIComponent(config.vid)}`
    + `&device=${encodeURIComponent(config.deviceId)}`
    + `&t=${encodeURIComponent(config.token)}`;
  return async (report) => {
    try {
      const res = await fetchImpl(url, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(report),
        signal: AbortSignal.timeout(20_000),
      });
      // A duplicate (replayed seq) still returns 2xx with {duplicate:true} — that IS success:
      // the worker has the data, and the aggregator should stop retrying it.
      return res.ok;
    } catch {
      return false; // network/timeout — the aggregator keeps the batch and retries under the same seq
    }
  };
}
