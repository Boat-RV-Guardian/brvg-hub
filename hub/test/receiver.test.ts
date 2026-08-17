import { describe, it, expect } from 'vitest';
import { parseWebhook, isTelemetryEvent } from '../src/receiver.js';
import { loadConfig, ConfigError } from '../src/config.js';

const q = (s: string) => new URLSearchParams(s);

describe('parseWebhook', () => {
  it('parses an incoming webhook into an item with its values', () => {
    const p = parseWebhook(q('device=shellyht-a1&event=humidity.change&rh=55&tC=21.5'));
    expect(p).toEqual({ item: { device: 'shellyht-a1', event: 'humidity.change', params: { rh: '55', tC: '21.5' } }, urgent: false });
  });

  it('classifies alarms as URGENT and telemetry as not (the events.ts line)', () => {
    expect(parseWebhook(q('device=d&event=flood.alarm'))!.urgent).toBe(true);
    expect(parseWebhook(q('device=d&event=flood.alarm_off'))!.urgent).toBe(true);
    expect(parseWebhook(q('device=d&event=voltmeter.measurement&v=12'))!.urgent).toBe(false);
    expect(parseWebhook(q('device=d&event=temperature.change&tC=20'))!.urgent).toBe(false);
    expect(isTelemetryEvent('humidity.change')).toBe(true);
    expect(isTelemetryEvent('btn.push')).toBe(false);
  });

  it('sanitizes the device id and event, and returns null when either is missing', () => {
    expect(parseWebhook(q('device=evil%0Aid&event=x.change&v=1'))!.item.device).toBe('evilid');
    expect(parseWebhook(q('event=x.change&v=1'))).toBeNull();  // no device
    expect(parseWebhook(q('device=d'))).toBeNull();            // no event
  });

  it('drops junk param keys and over-long values', () => {
    const p = parseWebhook(q('device=d&event=x.change&ok=1&ba;d=2'))!;
    expect(p.item.params).toEqual({ ok: '1', bad: '2' }); // ';' stripped from the key
  });
});

describe('loadConfig', () => {
  const base = { VID: 'v1', DEVICE_ID: 'brv_hub_1', DEVICE_TOKEN: 'tok' };

  it('requires vid/device/token and an https worker', () => {
    expect(() => loadConfig({})).toThrow(ConfigError);
    expect(() => loadConfig({ ...base, WORKER_URL: 'http://insecure' })).toThrow(/https/);
    const c = loadConfig(base);
    expect(c.workerBase).toBe('https://api.boatrvguardian.com');
    expect(c.port).toBe(8181);
    expect(c.drainIntervalSec).toBe(120);
  });

  it('floors the drain interval at 30 s — this rides a metered link', () => {
    expect(loadConfig({ ...base, DRAIN_INTERVAL: '5' }).drainIntervalSec).toBe(120); // below floor ⇒ default
    expect(loadConfig({ ...base, DRAIN_INTERVAL: '300' }).drainIntervalSec).toBe(300);
  });
});
