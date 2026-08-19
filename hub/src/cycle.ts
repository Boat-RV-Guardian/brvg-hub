// The valve CYCLE STATE MACHINE — the logic the owner ruled "does not exist on the valve alone"
// (2026-08-19). The hub owns it: watering plans are never written into the gateway, the hub decides
// starts, stops, restarts and the daily ledger, and the gateway is just the actuator.
//
// Every rule in here is an owner ruling, not a design taste:
//   * THREE MODES ONLY (2026-07-30, re-ratified 2026-08-19): washdown = TIME limit, NO volume cap;
//     tank fill = both; Normal/Daily = both (default 100 gal / 24 h clock). Do not invent a fourth.
//   * A manual press (button / external open) ENTERS NORMAL RUN and adopts the profile's cap.
//   * A cycle cut short by its VOLUME cap must NOT auto-restart. The old app-side heuristic
//     inferred "natural expiry" from time-remaining and restarts WRONGLY when the cap lands inside
//     the last poll interval — tracking the end reason explicitly is the fix.
//   * Washdown volume does NOT count against the daily total.
//   * A cycle already running when the hub starts is ADOPTED, never closed ("a hub update
//     mid-washdown must not shut the hose off") — and an adopted cycle is a Normal Run by the
//     manual-press rule, unless provenance says otherwise.
//
// PURE by construction: no timers, no I/O, no Date.now(). The caller feeds observations (status
// reports, command acks) with timestamps; this module returns the new state plus what to DO. That
// is what makes the restart bug testable instead of waiting for a boat to hit it.

export type CycleMode = 'normal' | 'washdown' | 'tankfill';

/**
 * Why a cycle ended. 'volume_cap' and 'timer' are the two that drive the restart decision;
 * 'flood_shutoff' exists so the event log can say what actually happened instead of "stopped".
 */
export type EndReason = 'timer' | 'volume_cap' | 'manual' | 'flood_shutoff' | 'unknown';

export interface RunningCycle {
  mode: CycleMode;
  /** ms epoch when the cycle was observed to start (or adopted). */
  startedAt: number;
  /** The duration the run was issued with, seconds. Always > 0 — even washdown carries time. */
  durationSecs: number;
  /**
   * Volume ceiling in LITRES, 0 = none. By the mode rules this is 0 exactly when mode is
   * washdown; normal and tankfill always carry one.
   */
  volumeCapL: number;
  /** Latest cycle volume observed, litres (already unit-converted and idle-latch-guarded). */
  volumeL: number;
  /** 'hub' = we started it; 'adopted' = it was already running (manual press, other controller). */
  provenance: 'hub' | 'adopted';
  /** Set once the hub has issued a stop it has not yet seen confirmed, so we do not double-stop. */
  stopIssued?: EndReason;
}

export interface EndedCycle {
  mode: CycleMode;
  endedAt: number;
  reason: EndReason;
  /** Final observed volume, litres. What the daily ledger consumes. */
  volumeL: number;
  provenance: 'hub' | 'adopted';
}

export type CycleState =
  | { kind: 'idle'; last?: EndedCycle }
  | ({ kind: 'running' } & RunningCycle);

export const IDLE: CycleState = { kind: 'idle' };

/** The Normal Run profile the hub holds for a valve — the source of the manual-press cap. */
export interface NormalProfile {
  durationSecs: number;
  volumeCapL: number;
}

// ── observations in ────────────────────────────────────────────────────────────────────────────

export interface StatusObservation {
  at: number;
  watering: boolean;
  /** Cycle volume in litres (0 when unknown/garbage — cycleVolumeLitres already guards). */
  volumeL: number;
  /** Seconds remaining as the gateway reports them, when it does. */
  remainSecs?: number;
}

/** What the caller must DO after a step — commands are decided here, issued there. */
export type CycleAction =
  | { do: 'none' }
  | { do: 'stop'; reason: EndReason }
  | { do: 'restart-normal' };

export interface StepResult {
  state: CycleState;
  action: CycleAction;
  /** Present exactly when this step closed a cycle — feed it to the daily ledger and event log. */
  ended?: EndedCycle;
}

/**
 * The hub started a cycle itself (its command to the gateway was acknowledged).
 * Mode decides the cap shape; the arguments must already respect the three-mode rules —
 * `startHubCycle` refuses the two combinations the owner has explicitly outlawed rather than
 * silently "fixing" them (that is how the external-cap bug shipped).
 */
export function startHubCycle(
  at: number,
  mode: CycleMode,
  durationSecs: number,
  volumeCapL: number,
): CycleState {
  if (mode === 'washdown' && volumeCapL > 0) {
    throw new Error('washdown is time-limited only — a volume cap on washdown is the outlawed shape');
  }
  if (mode !== 'washdown' && volumeCapL <= 0) {
    throw new Error(`${mode} must carry a volume cap — time-only runs are washdown by definition`);
  }
  return {
    kind: 'running', mode, startedAt: at,
    durationSecs: Math.max(1, Math.floor(durationSecs)),
    volumeCapL, volumeL: 0, provenance: 'hub',
  };
}

/**
 * Adopt a cycle the hub did not start — a manual button press, another controller, or a cycle
 * already underway when the hub booted. Owner rule: an external open IS a Normal Run and takes
 * the profile's cap. The duration is what the gateway reports remaining when it reports it, else
 * the profile's.
 */
export function adoptCycle(at: number, profile: NormalProfile, remainSecs?: number): CycleState & { kind: 'running' } {
  return {
    kind: 'running', mode: 'normal', startedAt: at,
    durationSecs: remainSecs != null && remainSecs > 0 ? Math.floor(remainSecs) : profile.durationSecs,
    volumeCapL: profile.volumeCapL, volumeL: 0, provenance: 'adopted',
  };
}

/** The hub was told to stop by a human (app button). Records intent so the close classifies right. */
export function noteManualStop(state: CycleState): CycleState {
  if (state.kind !== 'running') return state;
  return { ...state, stopIssued: 'manual' };
}

/** A flood event told the hub to close the valve. Same shape, different ledger entry. */
export function noteFloodStop(state: CycleState): CycleState {
  if (state.kind !== 'running') return state;
  return { ...state, stopIssued: 'flood_shutoff' };
}

/**
 * One observation step. This is the whole machine:
 *
 *   idle    + watering      → adopt (manual-press rule)
 *   running + volume >= cap → action:stop, reason volume_cap (the software cutoff — the hardware
 *                             "often ignores volume limits passed to cmd 6", so this IS the cap)
 *   running + !watering     → classify the end from what we know, in precedence order:
 *                             a stop the hub issued > volume cap reached > timer ran out > unknown
 *
 * The classification order matters: a volume-capped stop also looks "early" on the clock, and an
 * expired timer also shows volume near the cap sometimes. What the hub DID outranks inference.
 */
export function step(state: CycleState, obs: StatusObservation, profile: NormalProfile): StepResult {
  if (state.kind === 'idle') {
    if (!obs.watering) return { state, action: { do: 'none' } };
    const adopted = adoptCycle(obs.at, profile, obs.remainSecs);
    return { state: { ...adopted, volumeL: obs.volumeL }, action: { do: 'none' } };
  }

  // Still running.
  if (obs.watering) {
    const next: CycleState = { ...state, volumeL: obs.volumeL || state.volumeL };
    // The software-enforced volume cutoff. volumeCapL is 0 (no cap) only for washdown.
    if (!state.stopIssued && state.volumeCapL > 0 && next.kind === 'running' && next.volumeL >= state.volumeCapL) {
      return {
        state: { ...next, stopIssued: 'volume_cap' },
        action: { do: 'stop', reason: 'volume_cap' },
      };
    }
    return { state: next, action: { do: 'none' } };
  }

  // The valve closed. Classify why.
  const elapsedSecs = (obs.at - state.startedAt) / 1000;
  const finalVolume = obs.volumeL || state.volumeL;
  let reason: EndReason;
  if (state.stopIssued) {
    reason = state.stopIssued;
  } else if (state.volumeCapL > 0 && finalVolume >= state.volumeCapL) {
    // The HARDWARE honored a cap for once, or our stop raced the close. Either way: volume.
    reason = 'volume_cap';
  } else if (elapsedSecs >= state.durationSecs - 60) {
    // Within a minute of the issued duration = the timer. The margin absorbs poll jitter without
    // opening the old heuristic's hole — a volume stop was already caught above, by evidence.
    reason = 'timer';
  } else {
    reason = 'unknown';
  }

  const ended: EndedCycle = {
    mode: state.mode, endedAt: obs.at, reason, volumeL: finalVolume, provenance: state.provenance,
  };
  return { state: { kind: 'idle', last: ended }, action: { do: 'none' }, ended };
}

/**
 * Should a just-ended cycle restart the Normal Run? ONLY a timer expiry restarts — that is the
 * whole point of tracking end reasons. A volume-capped, manual, flood-stopped or unexplained end
 * never restarts: the first is the owner's explicit rule, the last is "when unsure, spend no
 * water".
 */
export function shouldAutoRestart(ended: EndedCycle, autoRestartEnabled: boolean): boolean {
  return autoRestartEnabled && ended.mode === 'normal' && ended.reason === 'timer';
}

// ── the daily ledger ───────────────────────────────────────────────────────────────────────────

export interface DailyLedger {
  /** UTC day key, YYYY-MM-DD. Display converts to the boat's zone; storage stays UTC (house rule). */
  day: string;
  /** Litres consumed by NORMAL + TANK FILL cycles. Washdown is excluded by owner rule. */
  volumeL: number;
}

export function dayKey(atMs: number): string {
  return new Date(atMs).toISOString().slice(0, 10);
}

/**
 * Fold an ended cycle into the ledger. Washdown does not count (owner: "when in washdown mode do
 * NOT count against the daily value"); everything else does, including adopted cycles — a manual
 * hose run is exactly the water the daily number exists to see.
 */
export function applyToLedger(ledger: DailyLedger | null, ended: EndedCycle): DailyLedger {
  const day = dayKey(ended.endedAt);
  const base = ledger && ledger.day === day ? ledger.volumeL : 0;
  const add = ended.mode === 'washdown' ? 0 : ended.volumeL;
  return { day, volumeL: base + add };
}
