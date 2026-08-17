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
    port: num(env.RELAY_PORT, 8181, 1),
    drainIntervalSec: num(env.DRAIN_INTERVAL, 120, 30), // floor 30 s — this rides a metered link
    keyframeEvery: num(env.RELAY_KEYFRAME_EVERY, 6, 1),
    nmeaHost: (env.NMEA_HOST || '').trim(),
    nmeaPort: num(env.NMEA_PORT, 10110, 1),
    cradlepointHost: (env.CRADLEPOINT_HOST || '').trim(),
    cradlepointPort: num(env.CRADLEPOINT_PORT, 80, 1),
    cradlepointUser: (env.CRADLEPOINT_USER || 'admin').trim(),
    cradlepointPassword: env.CRADLEPOINT_PASSWORD || '',
    gpsIntervalSec: num(env.GPS_INTERVAL, 120, 30), // same name, default and floor as the agent
  };
}

export { ConfigError };
