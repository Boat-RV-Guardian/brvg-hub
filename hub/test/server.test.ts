import { describe, it, expect, afterEach } from 'vitest';
import { startHub, DELIVERY_UNHEALTHY_AFTER, type HubHandle } from '../src/server.js';
import type { BatchReport } from '../src/contract.js';
import type { HubConfig } from '../src/config.js';

const config = (over: Partial<HubConfig> = {}): HubConfig => ({
  vid: 'v1', deviceId: 'brv_hub_1', token: 'tok', workerBase: 'https://api.example.com',
  port: 0, drainIntervalSec: 3600, keyframeEvery: 6, ...over, // port 0 = ephemeral; slow drain so tests drive it
});

const ok = { ok: true, commands: [] };
const fail = { ok: false, commands: [] };

let handle: HubHandle | null = null;
afterEach(async () => { if (handle) await handle.stop(); handle = null; });

function port(h: HubHandle): number {
  const addr = h.server.address();
  if (addr && typeof addr === 'object') return addr.port;
  throw new Error('no port');
}

describe('the hub receiver end to end (in-process)', () => {
  it('spools telemetry and answers the sensor 200 immediately', async () => {
    const sent: BatchReport[] = [];
    handle = startHub(config(), async (r) => { sent.push(r); return ok; });
    const p = port(handle);
    const res = await fetch(`http://127.0.0.1:${p}/cgi-bin/report?device=ht-a&event=humidity.change&rh=55`);
    expect(res.status).toBe(200);
    expect(await res.text()).toBe('ok');
    // telemetry did NOT send immediately
    expect(sent).toHaveLength(0);
    const health = await (await fetch(`http://127.0.0.1:${p}/healthz`)).json();
    expect(health).toMatchObject({ ok: true, spooled: 1, tier: 'hub' });
  });

  it('sends an ALARM immediately, on its own, without waiting for a drain', async () => {
    const sent: BatchReport[] = [];
    handle = startHub(config(), async (r) => { sent.push(r); return ok; });
    const p = port(handle);
    await fetch(`http://127.0.0.1:${p}/cgi-bin/report?device=flood-a&event=flood.alarm`);
    // give the fire-and-forget send a tick
    await new Promise((r) => setTimeout(r, 50));
    expect(sent).toHaveLength(1);
    expect(sent[0]!.items).toEqual([{ device: 'flood-a', event: 'flood.alarm', params: {} }]);
    expect(sent[0]!.seq).toBeUndefined(); // urgent envelope has no seq
  });

  it('spools an alarm only if the immediate send fails — never both', async () => {
    const sent: BatchReport[] = [];
    handle = startHub(config(), async (r) => { sent.push(r); return fail; }); // send always fails
    const p = port(handle);
    await fetch(`http://127.0.0.1:${p}/cgi-bin/report?device=flood-a&event=flood.alarm`);
    await new Promise((r) => setTimeout(r, 50));
    const health = await (await fetch(`http://127.0.0.1:${p}/healthz`)).json();
    expect(sent).toHaveLength(1);       // tried immediately
    expect(health.spooled).toBe(1);     // and fell back to the spool for the drain
  });

  it('404s an unknown path', async () => {
    handle = startHub(config(), async () => ok);
    const res = await fetch(`http://127.0.0.1:${port(handle)}/nope`);
    expect(res.status).toBe(404);
  });
});

describe('health reports DELIVERY, not just liveness', () => {
  // The router's fail-open watchdog keys off this endpoint. Answering 200 while every drain fails
  // would leave a lockdown armed around a hub that cannot deliver — the exact situation fail-open
  // exists to escape. So health has to mean "the vessel is still being heard from".
  it('stays healthy while deliveries succeed', async () => {
    handle = startHub(config(), async () => ok);
    const p = port(handle);
    await fetch(`http://127.0.0.1:${p}/cgi-bin/report?device=d&event=x.change&v=1`);
    const j: any = await (await fetch(`http://127.0.0.1:${p}/healthz`)).json();
    expect(j.ok).toBe(true);
    expect(j.consecutiveFailures).toBe(0);
  });

  it('goes UNHEALTHY (503) after sustained delivery failure, so the watchdog can fail open', async () => {
    // A fast drain interval so the timer really runs: the first failed drain retains the batch as
    // `pending`, and every subsequent tick retries it, so each tick is another counted failure.
    handle = startHub(config({ drainIntervalSec: 0.03 }), async () => fail);
    const p = port(handle);
    await fetch(`http://127.0.0.1:${p}/cgi-bin/report?device=d&event=x.change&v=1`);

    const deadline = Date.now() + 4000;
    let res = await fetch(`http://127.0.0.1:${p}/healthz`);
    while (res.status === 200 && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 40));
      res = await fetch(`http://127.0.0.1:${p}/healthz`);
    }
    const j: any = await res.json();
    expect(res.status).toBe(503);          // the watchdog's curl -fsS fails ⇒ it fails open
    expect(j.ok).toBe(false);
    expect(j.consecutiveFailures).toBeGreaterThanOrEqual(DELIVERY_UNHEALTHY_AFTER);
    expect(j.reason).toMatch(/no successful delivery/);
  }, 15000);

  it('a blip does not trip it — the threshold is generous by design', () => {
    // A WAN blip must not disarm someone's lockdown; at the default 120 s interval this threshold
    // is ~10 minutes of sustained failure, and the watchdog wants several failed probes on top.
    expect(DELIVERY_UNHEALTHY_AFTER).toBeGreaterThanOrEqual(5);
  });
});

describe('Phase B command channel (reply piggyback)', () => {
  it('executes report_now once, acks it on the next request, and never re-runs a re-delivered command', async () => {
    const calls: Array<{ report: BatchReport; acks: string[] }> = [];
    // Reply carries the same un-acked command TWICE (worker re-sends until acked).
    handle = startHub(config(), async (report, ackIds = []) => {
      calls.push({ report, acks: ackIds });
      return { ok: true, commands: [{ id: 'c1', cmd: 'report_now' }] };
    });
    const p = port(handle);
    // An alarm delivers immediately; its reply queues report_now → a near-immediate drain.
    await fetch(`http://127.0.0.1:${p}/cgi-bin/report?device=f&event=flood.alarm`);
    await fetch(`http://127.0.0.1:${p}/cgi-bin/report?device=t&event=x.change&v=1`); // spool something to drain
    await new Promise((r) => setTimeout(r, 200));
    expect(calls.length).toBe(2); // the alarm + ONE commanded drain (re-delivered c1 didn't re-trigger)
    expect(calls[0]!.acks).toEqual([]);
    expect(calls[1]!.acks).toEqual(['c1']); // the ack rode the next request out
  });

  it('acknowledges and DROPS a verb the hub does not know', async () => {
    const calls: Array<string[]> = [];
    const logs: string[] = [];
    handle = startHub(config(), async (_r, ackIds = []) => {
      calls.push(ackIds);
      return { ok: true, commands: calls.length === 1 ? [{ id: 'x9', cmd: 'reboot' }] : [] };
    }, (m) => logs.push(m));
    const p = port(handle);
    await fetch(`http://127.0.0.1:${p}/cgi-bin/report?device=f&event=flood.alarm`);
    await new Promise((r) => setTimeout(r, 60));
    await fetch(`http://127.0.0.1:${p}/cgi-bin/report?device=f2&event=door.alarm`);
    await new Promise((r) => setTimeout(r, 60));
    expect(calls[1]).toEqual(['x9']); // acked...
    expect(logs.some((m) => m.includes("ignoring 'reboot'"))).toBe(true); // ...and dropped, not executed
  });
});

describe('version consistency', () => {
  // HUB_VERSION is the number the worker actually sees — it rides every batch envelope. After the
  // 0.2.0 → 0.3.0 bump, package.json sat at 0.1.0: harmless to the wire, but the next person to
  // read it (or a handoff doc citing it) inherits a wrong fact. Same failure family as the app's
  // hardcoded APP_VERSION badge, same cure: one source, and a test that notices a fork.
  it('package.json matches HUB_VERSION — the wire version is the only version', async () => {
    const { HUB_VERSION } = await import('../src/server.js');
    const { readFileSync } = await import('node:fs');
    const pkg = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8'));
    expect(pkg.version).toBe(HUB_VERSION);
  });
});
