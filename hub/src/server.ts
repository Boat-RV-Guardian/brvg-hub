// The hub HTTP receiver + drain loop. Node built-in http, no framework. The Shellys POST/GET their
// webhooks here; telemetry is spooled and rolled up on the interval, alarms go to the cloud at once.

import { createServer, type Server } from 'node:http';
import { Aggregator } from './aggregator.js';
import { parseWebhook } from './receiver.js';
import { buildUrgent } from './contract.js';
import { startNmeaClient, createReportGovernor, fixToParams, type NmeaClientHandle } from './nmea.js';
import { pollCradlepoint } from './cradlepoint.js';
import { LinkTapRuntime, parseGatewayPush } from './linktapRuntime.js';
import type { HubConfig } from './config.js';
import type { Sender } from './sender.js';

export const HUB_VERSION = '0.6.0';

/**
 * Consecutive failed drains before /healthz reports UNHEALTHY (503).
 *
 * This is what the router's fail-open watchdog actually keys off, so the threshold is deliberately
 * generous: at the default 120 s drain interval this is ~10 minutes of sustained failure, and the
 * watchdog then wants several consecutive failed probes of its own on top. A WAN blip must not
 * disarm someone's lockdown; a genuinely broken hub must.
 */
export const DELIVERY_UNHEALTHY_AFTER = 5;

export interface HubHandle {
  server: Server;
  /** Stop the drain timer and close the listener. */
  stop: () => Promise<void>;
}

export function startHub(config: HubConfig, send: Sender, log: (m: string) => void = () => {}): HubHandle {
  const agg = new Aggregator(HUB_VERSION, config.keyframeEvery);
  let lastDeliveryAt = 0;

  // Phase B command channel: every delivery may return queued commands; acks ride the NEXT
  // request out. A command is executed once (its id joins pendingAcks immediately, so the worker
  // re-sending it until acked cannot re-run it). The hub's verb set is tiny — report_now drains
  // at once; anything else is acknowledged and DROPPED, same rule as the shell agent: an unknown
  // verb must never become code execution.
  let pendingAcks: string[] = [];
  let drainSoon: ReturnType<typeof setTimeout> | null = null;
  const runCommand = (cmd: string) => {
    if (cmd === 'report_now') {
      log('command: report_now');
      if (!drainSoon) {
        drainSoon = setTimeout(() => { drainSoon = null; void drain(); }, 50);
        drainSoon.unref?.();
      }
    } else {
      log(`command: ignoring '${cmd}' (not a hub verb)`);
    }
  };
  const deliver = async (report: Parameters<Sender>[0]): Promise<boolean> => {
    const acks = pendingAcks.slice();
    const r = await send(report, acks);
    if (!r.ok) return false;
    if (linktap && r.linktap?.profiles) linktap.applyProfiles(r.linktap.profiles);
    pendingAcks = pendingAcks.filter((a) => !acks.includes(a)); // the worker saw these
    for (const c of r.commands) {
      if (pendingAcks.includes(c.id)) continue; // still un-acked from a prior reply — already ran
      pendingAcks.push(c.id);
      runCommand(c.cmd);
    }
    return true;
  };
  const drain = async () => {
    const r = await agg.drain(deliver);
    if (!r) return;
    if (r.sent) lastDeliveryAt = Date.now();
    log(`drain seq=${r.seq} ${r.kind} items=${r.items} ok=${r.ok} ${r.sent ? '' : `(failed x${agg.consecutiveFailures}, will retry)`}`);
  };

  // ── LinkTap (hub-only model, owner 2026-08-19) ─────────────────────────────────────────────
  // The gateway lives on the LAN; this hub is its only controller. Push primary (the gateway's
  // HTTP client aimed at POST /linktap here), cmd-3 poll as the floor.
  let linktap: LinkTapRuntime | null = null;
  if (config.linktapHost && config.linktapGwId && config.linktapDevIds.length) {
    linktap = new LinkTapRuntime({
      target: { host: config.linktapHost, gatewayId: config.linktapGwId },
      devIds: config.linktapDevIds,
      profile: { durationSecs: config.linktapNormalSecs, volumeCapL: config.linktapNormalVolL },
      autoRestart: config.linktapAutoRestart,
      spool: (item) => agg.add(item),
      log,
    });
    log(`linktap: watching ${config.linktapDevIds.length} valve(s) via ${config.linktapHost}, poll floor ${config.linktapPollSec}s`);
  }

  // Local flood → valve shutoff: hub-lite capability #1 (owner pick, 2026-08-19). The SAME
  // classification line the worker uses (events.ts isFloodShutoff — flood/leak/alarm, not a
  // clear, not telemetry), so the hub closes the valve on exactly the events the cloud would
  // have. This is now the ONLY automated close path when the vessel's internet is down.
  const isFloodShutoff = (event: string) =>
    /flood|leak|alarm/i.test(event) && !/(?:_off|\.off)$/i.test(event) && !/\.(measurement|change)$/.test(event);

  const server = createServer((req, res) => {
    const url = new URL(req.url || '/', 'http://localhost');
    // Answer the sleepy sensor FIRST — it is awake on borrowed battery. Work happens after.
    if (url.pathname === '/healthz') {
      // Report on DELIVERY, not just liveness. A 200 here tells the router's watchdog the vessel
      // is still being heard from; answering 200 while every drain fails would leave a lockdown
      // armed around a hub that cannot deliver — the exact situation fail-open exists for.
      const failures = agg.consecutiveFailures;
      const delivering = failures < DELIVERY_UNHEALTHY_AFTER;
      res.writeHead(delivering ? 200 : 503, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({
        ok: delivering,
        spooled: agg.spoolSize,
        tier: 'hub',
        version: HUB_VERSION,
        consecutiveFailures: failures,
        lastDeliveryAt: lastDeliveryAt || null,
        ...(delivering ? {} : { reason: `no successful delivery in ${failures} attempts` }),
      }));
      return;
    }
    // Gateway HTTP-push (vendor doc §4.1): full status on every change + a 2-min heartbeat,
    // POSTed here once the gateway's HTTP client is aimed at this hub. Same payload shape as the
    // cmd 3 reply, so it funnels into the same observe() the poll uses.
    if (url.pathname === '/linktap' && linktap) {
      let body = '';
      req.on('data', (c) => { body += c; if (body.length > 64 * 1024) req.destroy(); });
      req.on('end', () => {
        res.writeHead(200, { 'Content-Type': 'text/plain' });
        res.end('ok');
        for (const { devId, data } of parseGatewayPush(body)) void linktap!.observe(devId, data);
      });
      return;
    }
    if (url.pathname !== '/cgi-bin/report' && url.pathname !== '/report') {
      res.writeHead(404); res.end('not found'); return;
    }
    res.writeHead(200, { 'Content-Type': 'text/plain' });
    res.end('ok');

    const parsed = parseWebhook(url.searchParams);
    if (!parsed) return;
    if (parsed.urgent) {
      // LOCAL flood → valve shutoff, BEFORE the cloud send: the close must not wait on the WAN.
      // The valve self-limits regardless (every open carries duration+volume), so this only ever
      // closes it sooner — same safety model as the app's bilge shutoff.
      if (linktap && isFloodShutoff(parsed.item.event)) {
        log(`flood shutoff: ${parsed.item.device} ${parsed.item.event} — closing all valves`);
        void linktap.floodStopAll();
      }
      // Alarms never wait for the roll-up. Fire immediately, on their own. (Through `deliver`, so
      // even an urgent reply can carry commands and flush acks.)
      void deliver(buildUrgent(parsed.item, HUB_VERSION)).then((ok) => {
        // Only if the immediate send fails does it fall to the spool for the next drain — never
        // both, so an alarm is never double-reported.
        if (!ok) agg.add(parsed.item);
        log(`urgent ${parsed.item.device} ${parsed.item.event} ${ok ? 'sent' : 'spooled (send failed)'}`);
      });
    } else {
      agg.add(parsed.item);
    }
  });

  server.listen(config.port, () => log(`hub listening on :${config.port}, draining every ${config.drainIntervalSec}s`));

  // GPS sources, both opt-in, both spooling gps.measurement under the HUB's own device id —
  // exactly how the shell agent reports its router's GNSS.
  const spoolFix = (fix: { lat: number; lon: number; acc?: number }) =>
    agg.add({ device: config.deviceId, event: 'gps.measurement', params: fixToParams(fix) });

  // NMEA 0183 stream (NMEA_HOST — chartplotter, AIS, gpsd). Fixes arrive at stream rate; the
  // governor throttles to movement / heartbeat.
  let nmea: NmeaClientHandle | null = null;
  if (config.nmeaHost) {
    const governor = createReportGovernor(config.gpsIntervalSec * 1000);
    nmea = startNmeaClient({ host: config.nmeaHost, port: config.nmeaPort, log }, (fix) => {
      if (governor.decide(fix, Date.now())) spoolFix(fix);
    });
  }

  // Cradlepoint (CRADLEPOINT_HOST) is POLLED on the GPS_INTERVAL timer — the owner's ruling: the
  // hub asks the router, nothing is configured on the router to send anywhere. One poll per
  // interval IS the cadence; a stationary vessel dedups via the aggregator's unchanged signature.
  let cpTimer: ReturnType<typeof setInterval> | null = null;
  if (config.cradlepointHost) {
    const opts = {
      host: config.cradlepointHost,
      port: config.cradlepointPort,
      username: config.cradlepointUser,
      password: config.cradlepointPassword,
    };
    const poll = () => void pollCradlepoint(opts).then((fix) => { if (fix) spoolFix(fix); });
    poll();
    cpTimer = setInterval(poll, config.gpsIntervalSec * 1000);
    cpTimer.unref?.();
  }

  // The LinkTap poll floor. Push-configured gateways make most polls redundant — the aggregator's
  // unchanged-signature dedup keeps them off the wire.
  let ltTimer: ReturnType<typeof setInterval> | null = null;
  if (linktap) {
    const poll = () => void linktap!.pollOnce();
    poll();
    ltTimer = setInterval(poll, config.linktapPollSec * 1000);
    ltTimer.unref?.();
  }

  const timer = setInterval(() => { void drain(); }, config.drainIntervalSec * 1000);
  timer.unref?.();

  return {
    server,
    stop: () => new Promise((resolve) => { nmea?.stop(); if (cpTimer) clearInterval(cpTimer); if (ltTimer) clearInterval(ltTimer); if (drainSoon) clearTimeout(drainSoon); clearInterval(timer); server.close(() => resolve()); }),
  };
}
