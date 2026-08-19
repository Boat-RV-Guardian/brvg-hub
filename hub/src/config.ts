// Hub configuration — read from the environment (12-factor; Docker- and systemd-friendly). Mirrors
// the shell agent's /etc/brvg-agent.conf names where they overlap, so the app's enrollment config
// can drive either tier.

export interface HubConfig {
  vid: string;
  deviceId: string;
  token: string;
  workerBase: string;
  /** LAN port the Shelly webhook receiver listens on. */
  port: number;
  /** Seconds between roll-up drains. */
  drainIntervalSec: number;
  keyframeEvery: number;
  /** NMEA 0183 TCP source (chartplotter, AIS, gpsd). Empty host = feature off. */
  nmeaHost: string;
  nmeaPort: number;
  /** Cradlepoint local HTTP API, POLLED (owner: never streamed-to). Empty host = feature off. */
  cradlepointHost: string;
  cradlepointPort: number;
  cradlepointUser: string;
  cradlepointPassword: string;
  /** GPS poll / stationary report cadence, seconds — mirrors the shell agent's GPS_INTERVAL. */
  gpsIntervalSec: number;
  /** LinkTap gateway on the LAN (hub-only model, owner 2026-08-19). Empty host = feature off. */
  linktapHost: string;
  linktapGwId: string;
  /** The valves this hub watches — comma-separated 16-hex dev ids. */
  linktapDevIds: string[];
  /** Poll floor, seconds. Push (the gateway's HTTP client aimed at this hub) is the primary. */
  linktapPollSec: number;
  /** Normal Run profile: ALWAYS a duration and a volume cap (the three-mode rules). */
  linktapNormalSecs: number;
  linktapNormalVolL: number;
  linktapAutoRestart: boolean;
}

class ConfigError extends Error {}

/** Parse + validate. Throws ConfigError with a single actionable message on the first problem. */
export function loadConfig(env: Record<string, string | undefined>): HubConfig {
  const vid = (env.VID || '').trim();
  const deviceId = (env.DEVICE_ID || '').trim();
  const token = (env.DEVICE_TOKEN || '').trim();
  const workerBase = (env.WORKER_URL || 'https://api.boatrvguardian.com').trim();

  if (!vid) throw new ConfigError('VID is required (the vehicle this hub reports into).');
  if (!deviceId) throw new ConfigError('DEVICE_ID is required (this hub’s device id in the vehicle).');
  if (!token) throw new ConfigError('DEVICE_TOKEN is required — enroll this hub in the app to mint one.');
  if (!/^https:\/\//.test(workerBase)) throw new ConfigError('WORKER_URL must be https.');

  const num = (v: string | undefined, def: number, min: number): number => {
    const n = Number(v);
    return Number.isFinite(n) && n >= min ? Math.floor(n) : def;
  };

  return {
    vid,
    deviceId,
    token,
    workerBase: workerBase.replace(/\/$/, ''),
    port: num(env.RECEIVER_PORT, 8181, 1),
    drainIntervalSec: num(env.DRAIN_INTERVAL, 120, 30), // floor 30 s — this rides a metered link
    keyframeEvery: num(env.KEYFRAME_EVERY, 6, 1),
    nmeaHost: (env.NMEA_HOST || '').trim(),
    nmeaPort: num(env.NMEA_PORT, 10110, 1),
    cradlepointHost: (env.CRADLEPOINT_HOST || '').trim(),
    cradlepointPort: num(env.CRADLEPOINT_PORT, 80, 1),
    cradlepointUser: (env.CRADLEPOINT_USER || 'admin').trim(),
    cradlepointPassword: env.CRADLEPOINT_PASSWORD || '',
    gpsIntervalSec: num(env.GPS_INTERVAL, 120, 30), // same name, default and floor as the agent
    linktapHost: (env.LINKTAP_HOST || '').trim(),
    linktapGwId: (env.LINKTAP_GW_ID || '').trim(),
    linktapDevIds: (env.LINKTAP_DEV_IDS || '').split(',').map((v) => v.trim().slice(0, 16)).filter(Boolean),
    // Floor 15 s: below that the hub spends its time polling; the gateway's own push heartbeat is
    // 2 min, so the poll is only the floor under it anyway.
    linktapPollSec: num(env.LINKTAP_POLL_INTERVAL, 60, 15),
    linktapNormalSecs: num(env.LINKTAP_NORMAL_SECS, 24 * 3600, 60),
    // 378.5 L = the 100 gal Normal Run default (owner spec 2026-07-30).
    linktapNormalVolL: num(env.LINKTAP_NORMAL_VOL_L, 378, 1),
    linktapAutoRestart: env.LINKTAP_AUTO_RESTART === '1',
  };
}

export { ConfigError };
