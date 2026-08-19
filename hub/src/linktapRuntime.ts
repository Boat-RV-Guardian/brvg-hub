// The LinkTap runtime — where the protocol client (linktap.ts) meets the cycle state machine
// (cycle.ts) and the hub's spool. This is the piece that makes the hub-only model real: the hub
// polls the gateway (and accepts its HTTP pushes), decides cuts and restarts, and reports what
// happened — no LinkTap cloud anywhere.
//
// Telemetry model, matching the vendor doc exactly: push PRIMARY (the gateway POSTs full status on
// every change + a 2-minute heartbeat when its HTTP client is configured), poll as the FLOOR
// (cmd 3 on LINKTAP_POLL_INTERVAL) so a gateway nobody configured for push still works and a missed
// push cannot strand stale state. Both paths funnel into ONE `observe()` — the machine cannot tell
// them apart, which is the point.
//
// PURE-ish: the timer lives in start(); everything else takes injected fetch/now, so the whole
// cut-restart-ledger path is testable without a gateway.

import {
  type GatewayTarget, type GatewayVolUnit,
  buildStatus, buildStart, buildStop, postCommand, readVolUnit,
  cycleVolumeLitres, coerceWateringBool, reportsVolume,
} from './linktap.js';
import {
  IDLE, step, shouldAutoRestart, applyToLedger, noteManualStop, noteFloodStop, startHubCycle,
  type CycleState, type NormalProfile, type DailyLedger, type EndedCycle,
} from './cycle.js';
import type { BatchItem } from './contract.js';

export interface LinkTapRuntimeOpts {
  target: GatewayTarget;
  /** The valves this hub watches (16-hex dev ids). */
  devIds: string[];
  profile: NormalProfile;
  autoRestart: boolean;
  /** Spool a telemetry item into the aggregator (rides the normal roll-up). */
  spool: (item: BatchItem) => void;
  log?: (m: string) => void;
  fetchImpl?: typeof fetch;
  now?: () => number;
}

interface ValveTrack {
  state: CycleState;
  ledger: DailyLedger | null;
  /** The last is_flm_plugin seen — a non-metering valve is bounded by TIME only, say so once. */
  meters: boolean | null;
}

/**
 * One gateway, N valves. `observe` is the single entry for BOTH transports; `pollOnce` drives the
 * floor; `start`/`stopWatering` are the command surface the hub's management API will call.
 */
export class LinkTapRuntime {
  private readonly o: Required<Pick<LinkTapRuntimeOpts, 'fetchImpl' | 'now' | 'log'>> & LinkTapRuntimeOpts;
  private tracks = new Map<string, ValveTrack>();
  private volUnit: GatewayVolUnit = 'gal'; // safe default — guessing litres under-reports 3.79x
  private volUnitRead = false;

  constructor(opts: LinkTapRuntimeOpts) {
    this.o = { fetchImpl: fetch, now: () => Date.now(), log: () => {}, ...opts };
    for (const id of opts.devIds) this.tracks.set(id, { state: IDLE, ledger: null, meters: null });
  }

  private track(devId: string): ValveTrack | undefined { return this.tracks.get(devId); }

  /** Read the gateway's volume unit once; every volume number in the protocol is in this unit. */
  async ensureVolUnit(): Promise<void> {
    if (this.volUnitRead) return;
    this.volUnit = await readVolUnit(this.o.target, this.o.fetchImpl);
    this.volUnitRead = true;
    this.o.log(`linktap: gateway volume unit is ${this.volUnit}`);
  }

  /**
   * Feed one status payload for one valve — from a poll reply or a gateway push, indistinguishably.
   * Runs the machine, issues the software volume cut when it says to, folds ended cycles into the
   * ledger, and spools what changed.
   */
  async observe(devId: string, data: any): Promise<void> {
    const t = this.track(devId);
    if (!t) return; // not a valve this hub watches
    const at = this.o.now();

    const meters = reportsVolume(data);
    if (t.meters === null && !meters) {
      // Same honesty the app shows: without a flow meter no volume limit can hold, only time.
      this.o.log(`linktap: ${devId} does not meter flow — cycles are bounded by TIME only`);
    }
    t.meters = meters;

    const watering = coerceWateringBool(data?.is_watering);
    const volumeL = cycleVolumeLitres(data, this.volUnit);
    const remainRaw = Number(data?.remain_duration ?? NaN);
    const obs = {
      at, watering, volumeL,
      remainSecs: Number.isFinite(remainRaw) && remainRaw > 0 ? remainRaw : undefined,
    };

    const r = step(t.state, obs, this.o.profile);
    t.state = r.state;

    if (r.action.do === 'stop') {
      this.o.log(`linktap: ${devId} volume cap reached (${volumeL.toFixed(1)} L) — issuing stop`);
      const reply = await postCommand(this.o.target, buildStop(this.o.target, devId), this.o.fetchImpl);
      if (!reply.ok) {
        // The next observation retries via the same path — stopIssued stays set, so no re-issue
        // storm; but a failed CLOSE is worth hearing about immediately.
        this.spoolEvent(devId, 'linktap.stop_failed', { error: reply.error ?? 'unknown' });
        this.o.log(`linktap: ${devId} STOP FAILED: ${reply.error}`);
      }
    }

    if (r.ended) this.onEnded(devId, t, r.ended);

    // Telemetry rides the roll-up. The ledger travels as params on the same item, so the worker's
    // sensorState carries the daily total with zero new wire surface — the app reads it from there.
    this.spoolEvent(devId, 'linktap.measurement', {
      watering: watering ? '1' : '0',
      vol_l: volumeL ? volumeL.toFixed(2) : '0',
      ...(obs.remainSecs != null ? { remain_s: String(obs.remainSecs) } : {}),
      meters: meters ? '1' : '0',
      ...(t.ledger ? { day: t.ledger.day, day_vol_l: t.ledger.volumeL.toFixed(2) } : {}),
    });
  }

  private onEnded(devId: string, t: ValveTrack, ended: EndedCycle): void {
    t.ledger = applyToLedger(t.ledger, ended);
    this.o.log(`linktap: ${devId} cycle ended (${ended.reason}) ${ended.volumeL.toFixed(1)} L — day total ${t.ledger.volumeL.toFixed(1)} L`);
    // .change so it batches — a cycle end is history, not an alarm. The worker classifies it as
    // telemetry by the same *.change rule every other component uses.
    this.spoolEvent(devId, 'linktap.cycle.change', {
      mode: ended.mode, reason: ended.reason, vol_l: ended.volumeL.toFixed(2), provenance: ended.provenance,
    });

    if (shouldAutoRestart(ended, this.o.autoRestart)) {
      this.o.log(`linktap: ${devId} timer expired with auto-restart on — starting a fresh Normal Run`);
      void this.startNormalRun(devId);
    }
  }

  /** Start a Normal Run from the profile: always a duration AND a volume cap, by the mode rules. */
  async startNormalRun(devId: string): Promise<boolean> {
    const t = this.track(devId);
    if (!t) return false;
    const { durationSecs, volumeCapL } = this.o.profile;
    // volume_limit is sent in the GATEWAY's unit — and not trusted; the machine enforces the cut.
    const capInGatewayUnit = this.volUnit === 'gal' ? volumeCapL / 3.785411784 : volumeCapL;
    const reply = await postCommand(
      this.o.target,
      buildStart(this.o.target, devId, durationSecs, Math.round(capInGatewayUnit * 100) / 100),
      this.o.fetchImpl,
    );
    if (reply.ok) t.state = startHubCycle(this.o.now(), 'normal', durationSecs, volumeCapL);
    else this.o.log(`linktap: ${devId} start failed: ${reply.error}`);
    return reply.ok;
  }

  /** A human asked for a stop (app button, via the hub's management API). */
  async stopWatering(devId: string): Promise<boolean> {
    const t = this.track(devId);
    if (!t) return false;
    t.state = noteManualStop(t.state);
    const reply = await postCommand(this.o.target, buildStop(this.o.target, devId), this.o.fetchImpl);
    if (!reply.ok) this.o.log(`linktap: ${devId} manual stop failed: ${reply.error}`);
    return reply.ok;
  }

  /** A flood event asked for a stop — hub-lite capability #1's entry point. Closes EVERY valve. */
  async floodStopAll(): Promise<void> {
    for (const [devId, t] of this.tracks) {
      t.state = noteFloodStop(t.state);
      const reply = await postCommand(this.o.target, buildStop(this.o.target, devId), this.o.fetchImpl);
      this.o.log(`linktap: flood shutoff → ${devId} ${reply.ok ? 'closed' : `FAILED: ${reply.error}`}`);
      if (!reply.ok) this.spoolEvent(devId, 'linktap.stop_failed', { error: reply.error ?? 'unknown', cause: 'flood' });
    }
  }

  /** The poll floor: ask the gateway about every valve once. Push-configured gateways make this a no-op-ish dedup via the aggregator. */
  async pollOnce(): Promise<void> {
    await this.ensureVolUnit();
    for (const devId of this.tracks.keys()) {
      const reply = await postCommand(this.o.target, buildStatus(this.o.target, devId), this.o.fetchImpl);
      if (!reply.ok) continue; // an unreachable gateway is the poll loop's normal weather
      const data = reply.data?.dev_stat?.[0] ?? reply.data;
      if (data) await this.observe(devId, data);
    }
  }

  private spoolEvent(devId: string, event: string, params: Record<string, string>): void {
    this.o.spool({ device: `lt_${devId}`, event, params });
  }
}

/**
 * Parse a gateway HTTP-push body (it POSTs the same JSON the cmd 3 reply carries — §4.1: full
 * status on every change plus a 2-minute heartbeat). Returns the per-valve payloads found.
 */
export function parseGatewayPush(body: string): Array<{ devId: string; data: any }> {
  let parsed: any;
  try { parsed = JSON.parse(body); } catch { return []; }
  const stats: any[] = Array.isArray(parsed?.dev_stat) ? parsed.dev_stat
    : parsed?.dev_id ? [parsed] : [];
  const out: Array<{ devId: string; data: any }> = [];
  for (const d of stats) {
    const devId = typeof d?.dev_id === 'string' ? d.dev_id.slice(0, 16) : '';
    if (devId) out.push({ devId, data: d });
  }
  return out;
}
