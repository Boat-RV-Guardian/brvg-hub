// Turn an incoming Shelly webhook into a spool entry. The Shellys fire the URL the app registered
// (buildRelayWebhookUrl): ?device=<id>&event=<ev>&<values>. This is the hub's front door.
//
// URGENT vs telemetry is drawn on the SAME line the worker and the shell hub-lite use (events.ts
// isTelemetry): *.measurement / *.change batch; everything else — alarms, alarm-clears, button —
// goes to the cloud immediately, on its own. Aggregation never delays an alarm.

import type { BatchItem } from './contract.js';

const NAME_MAX = 64;
const VALUE_MAX = 256;
const MAX_PARAMS = 24;

const cleanId = (v: string | null): string =>
  (v ?? '').replace(/[^A-Za-z0-9_.:-]/g, '').slice(0, NAME_MAX);
const cleanEvent = (v: string | null): string =>
  (v ?? '').replace(/[^A-Za-z0-9_.-]/g, '').slice(0, NAME_MAX);

export interface ParsedWebhook {
  item: BatchItem;
  /** Alarms and other non-telemetry events go to the cloud immediately. */
  urgent: boolean;
}

/** Does this event batch (measurement/change) or go now? Matches events.ts isTelemetry exactly. */
export function isTelemetryEvent(event: string): boolean {
  return /\.(measurement|change)$/.test(event);
}

/**
 * Parse the query of an incoming webhook request. Returns null when it carries no usable device+event
 * — the caller answers 200 regardless (a sleepy sensor is awake on borrowed battery; never make it
 * wait or retry), it just spools nothing.
 */
export function parseWebhook(searchParams: URLSearchParams): ParsedWebhook | null {
  const device = cleanId(searchParams.get('device'));
  const event = cleanEvent(searchParams.get('event'));
  if (!device || !event) return null;

  const params: Record<string, string> = {};
  let n = 0;
  for (const [k, v] of searchParams) {
    if (k === 'device' || k === 'event') continue;
    if (n >= MAX_PARAMS) break;
    const key = cleanEvent(k);
    if (!key || v === '' || v.length > VALUE_MAX) continue;
    params[key] = v;
    n += 1;
  }
  return { item: { device, event, params }, urgent: !isTelemetryEvent(event) };
}
