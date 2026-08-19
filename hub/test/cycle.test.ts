import { describe, it, expect } from 'vitest';
import {
  IDLE, startHubCycle, adoptCycle, noteManualStop, noteFloodStop, step, shouldAutoRestart,
  applyToLedger, dayKey, type CycleState, type StatusObservation, type NormalProfile,
} from '../src/cycle.js';

const PROFILE: NormalProfile = { durationSecs: 24 * 3600, volumeCapL: 378.5 }; // 100 gal / 24 h
const T0 = Date.UTC(2026, 7, 19, 12, 0, 0);
const obs = (o: Partial<StatusObservation>): StatusObservation =>
  ({ at: T0, watering: false, volumeL: 0, ...o });

describe('the three modes are enforced at the door', () => {
  it('normal and tankfill must carry a volume cap', () => {
    expect(() => startHubCycle(T0, 'normal', 3600, 0)).toThrow(/volume cap/);
    expect(() => startHubCycle(T0, 'tankfill', 3600, 0)).toThrow(/volume cap/);
  });

  it('washdown must NOT carry one — the outlawed shape that cut 2-hour runs at ~26 gal', () => {
    expect(() => startHubCycle(T0, 'washdown', 7200, 100)).toThrow(/outlawed/);
    const s = startHubCycle(T0, 'washdown', 7200, 0);
    expect(s.kind).toBe('running');
  });
});

describe('software volume cutoff', () => {
  it('issues a stop the moment the cap is reached — the hardware cannot be trusted to', () => {
    let s = startHubCycle(T0, 'normal', 24 * 3600, 100);
    const r = step(s, obs({ at: T0 + 60_000, watering: true, volumeL: 100.2 }), PROFILE);
    expect(r.action).toEqual({ do: 'stop', reason: 'volume_cap' });
  });

  it('does not double-issue the stop while waiting for the valve to close', () => {
    let s = startHubCycle(T0, 'normal', 24 * 3600, 100);
    const first = step(s, obs({ at: T0 + 60_000, watering: true, volumeL: 100.2 }), PROFILE);
    const second = step(first.state, obs({ at: T0 + 75_000, watering: true, volumeL: 101 }), PROFILE);
    expect(second.action.do).toBe('none');
  });

  it('washdown never volume-stops no matter what the meter says', () => {
    const s = startHubCycle(T0, 'washdown', 7200, 0);
    const r = step(s, obs({ at: T0 + 60_000, watering: true, volumeL: 5000 }), PROFILE);
    expect(r.action.do).toBe('none');
  });
});

describe('end-reason classification — what the hub DID outranks inference', () => {
  it('a hub-issued volume stop classifies as volume_cap even though it ended early on the clock', () => {
    let s = startHubCycle(T0, 'normal', 24 * 3600, 100);
    const cut = step(s, obs({ at: T0 + 60_000, watering: true, volumeL: 100.5 }), PROFILE);
    const closed = step(cut.state, obs({ at: T0 + 90_000, watering: false, volumeL: 100.5 }), PROFILE);
    expect(closed.ended?.reason).toBe('volume_cap');
  });

  it('THE BUG THIS MODULE EXISTS FOR: a hardware volume stop inside one poll interval is still volume_cap', () => {
    // The old heuristic read any close near the end of a poll gap as "natural expiry" and
    // restarted — spending more water after a cap had already fired.
    let s = startHubCycle(T0, 'normal', 600, 100); // 10-min run
    // Next observation: valve already CLOSED, volume at the cap. No stop was ever issued by us.
    const closed = step(s, obs({ at: T0 + 120_000, watering: false, volumeL: 100.3 }), PROFILE);
    expect(closed.ended?.reason).toBe('volume_cap');
    expect(shouldAutoRestart(closed.ended!, true)).toBe(false);
  });

  it('a close within a minute of the issued duration is the timer', () => {
    let s = startHubCycle(T0, 'normal', 600, 100);
    const closed = step(s, obs({ at: T0 + 590_000, watering: false, volumeL: 40 }), PROFILE);
    expect(closed.ended?.reason).toBe('timer');
  });

  it('a manual stop is manual, and an early close with no explanation is unknown', () => {
    let s = startHubCycle(T0, 'normal', 600, 100);
    const manual = step(noteManualStop(s), obs({ at: T0 + 60_000, watering: false, volumeL: 5 }), PROFILE);
    expect(manual.ended?.reason).toBe('manual');

    let s2 = startHubCycle(T0, 'normal', 600, 100);
    const mystery = step(s2, obs({ at: T0 + 60_000, watering: false, volumeL: 5 }), PROFILE);
    expect(mystery.ended?.reason).toBe('unknown');
  });

  it('a flood shutoff is recorded as exactly that', () => {
    let s = startHubCycle(T0, 'washdown', 7200, 0);
    const closed = step(noteFloodStop(s), obs({ at: T0 + 60_000, watering: false, volumeL: 30 }), PROFILE);
    expect(closed.ended?.reason).toBe('flood_shutoff');
  });
});

describe('auto-restart — ONLY a timer expiry restarts', () => {
  const ended = (reason: any, mode: any = 'normal') =>
    ({ mode, endedAt: T0, reason, volumeL: 10, provenance: 'hub' as const });

  it('timer restarts when enabled; nothing else ever does', () => {
    expect(shouldAutoRestart(ended('timer'), true)).toBe(true);
    for (const r of ['volume_cap', 'manual', 'flood_shutoff', 'unknown']) {
      expect(shouldAutoRestart(ended(r), true)).toBe(false);
    }
  });

  it('disabled means disabled, and washdown/tankfill expiries do not loop either', () => {
    expect(shouldAutoRestart(ended('timer'), false)).toBe(false);
    expect(shouldAutoRestart(ended('timer', 'washdown'), true)).toBe(false);
    expect(shouldAutoRestart(ended('timer', 'tankfill'), true)).toBe(false);
  });
});

describe('manual press / adoption — an external open IS a Normal Run', () => {
  it('an open the hub did not start is adopted with the profile cap', () => {
    const r = step(IDLE, obs({ at: T0, watering: true, volumeL: 0.5 }), PROFILE);
    expect(r.state.kind).toBe('running');
    if (r.state.kind === 'running') {
      expect(r.state.mode).toBe('normal');
      expect(r.state.volumeCapL).toBe(PROFILE.volumeCapL);
      expect(r.state.provenance).toBe('adopted');
    }
  });

  it('adoption trusts the gateway remaining time when reported', () => {
    const s = adoptCycle(T0, PROFILE, 480);
    expect(s.kind === 'running' && s.durationSecs).toBe(480);
  });

  it('an adopted run is volume-cut exactly like a hub run — the cap follows the mode, not the starter', () => {
    const r = step(IDLE, obs({ at: T0, watering: true, volumeL: 0 }), PROFILE);
    const cut = step(r.state, obs({ at: T0 + 300_000, watering: true, volumeL: 380 }), PROFILE);
    expect(cut.action).toEqual({ do: 'stop', reason: 'volume_cap' });
  });
});

describe('hub restart mid-cycle — adopt, never close', () => {
  it('a fresh machine seeing a running valve adopts it rather than stopping it', () => {
    // "A hub update mid-washdown must not shut the hose off." The fresh state is IDLE; the first
    // status shows watering; nothing in the resulting action is a stop.
    const r = step(IDLE, obs({ at: T0, watering: true, volumeL: 12 }), PROFILE);
    expect(r.action.do).toBe('none');
    expect(r.state.kind).toBe('running');
  });
});

describe('the daily ledger', () => {
  const endedAt = Date.UTC(2026, 7, 19, 15, 30, 0);

  it('normal and tankfill count; washdown does NOT (owner rule)', () => {
    let l = applyToLedger(null, { mode: 'normal', endedAt, reason: 'timer', volumeL: 40, provenance: 'hub' });
    l = applyToLedger(l, { mode: 'tankfill', endedAt, reason: 'volume_cap', volumeL: 60, provenance: 'hub' });
    l = applyToLedger(l, { mode: 'washdown', endedAt, reason: 'timer', volumeL: 500, provenance: 'hub' });
    expect(l.volumeL).toBe(100);
  });

  it('adopted (manual hose) runs count — they are exactly the water the number exists to see', () => {
    const l = applyToLedger(null, { mode: 'normal', endedAt, reason: 'manual', volumeL: 25, provenance: 'adopted' });
    expect(l.volumeL).toBe(25);
  });

  it('rolls to a new UTC day cleanly', () => {
    const lateDay1 = Date.UTC(2026, 7, 19, 23, 59, 0);
    const earlyDay2 = Date.UTC(2026, 7, 20, 0, 5, 0);
    let l = applyToLedger(null, { mode: 'normal', endedAt: lateDay1, reason: 'timer', volumeL: 30, provenance: 'hub' });
    l = applyToLedger(l, { mode: 'normal', endedAt: earlyDay2, reason: 'timer', volumeL: 10, provenance: 'hub' });
    expect(l).toEqual({ day: '2026-08-20', volumeL: 10 });
  });

  it('day keys are UTC ISO dates — storage is UTC, display converts (house rule)', () => {
    expect(dayKey(Date.UTC(2026, 7, 19, 0, 0, 1))).toBe('2026-08-19');
  });
});

describe('full lifecycle: start → meter climbs → cap → close → no restart → ledger', () => {
  it('walks the whole owner-specified path in order', () => {
    let state: CycleState = startHubCycle(T0, 'normal', 24 * 3600, 100);
    // volume climbing under the cap: nothing to do
    let r = step(state, obs({ at: T0 + 60_000, watering: true, volumeL: 50 }), PROFILE);
    expect(r.action.do).toBe('none');
    // cap reached: stop issued
    r = step(r.state, obs({ at: T0 + 120_000, watering: true, volumeL: 100 }), PROFILE);
    expect(r.action.do).toBe('stop');
    // valve confirms closed: ended as volume_cap, restart refused, ledger credited
    r = step(r.state, obs({ at: T0 + 150_000, watering: false, volumeL: 100 }), PROFILE);
    expect(r.ended?.reason).toBe('volume_cap');
    expect(shouldAutoRestart(r.ended!, true)).toBe(false);
    expect(applyToLedger(null, r.ended!).volumeL).toBe(100);
  });
});
