// LinkTap gateway client — LOCAL HTTP ONLY, no MQTT broker, no LinkTap cloud.
//
// Owner direction 2026-08-19: "I would like to only use a hub to link tap gateway model. I do not
// wanna support using the linktap cloud anymore ... That includes adding linktap valves to the
// gateway without the linktap app ... need to confirm by HTTP not MQTT." This module is that path.
//
// ── WHY HTTP IS ENOUGH, from the vendor's own document ────────────────────────────────────────
// `LinkTap_Gateway_MQTT_Client_Integration.pdf` v2.1 §4.2: commands are plain HTTP POSTs of JSON to
// `http://<gateway-ip>/api.shtml`, and the reachable set is defined BY DIRECTION rather than by an
// allowlist — "refer to message definitions ... where 'Message direction' is 'App->Broker->GW'".
// §4 is explicit that this is a supported mode, not a workaround: "When neither Internet access nor
// MQTT broker is available, third-party application can interact with the gateway through HTTP
// commands." So HTTP and MQTT carry THE SAME MESSAGES, and an MQTT implementation of a command is
// equally the HTTP body. That equivalence is what makes CMD 1 below usable.
//
// ── THE ONE CONTRACT ──────────────────────────────────────────────────────────────────────────
// The app already speaks this protocol (dashboard/src/utils/linktapStatus.ts, linktapHttp.ts,
// hooks/useLinkTapCommands.ts). This module is deliberately the SAME dialect, not a second one —
// per HUB-PROXY.md's "a relay is a subset of a hub, never a variant of one". Where a value below
// was measured on hardware, the measurement is recorded so nobody re-derives it wrongly; this
// protocol has already cost two inverted unit bugs.

export const GATEWAY_TIMEOUT_MS = 15_000;

/**
 * Commands, with the direction that decides HTTP reachability. Everything here is App->GW, so
 * everything here is POSTable to api.shtml. (Directions cross-checked against openHAB's
 * `TLGatewayFrame` javadoc, which documents one per command.)
 */
export const LT_CMD = {
  /** App->GW. Add / register an end device. ⚠️ Takes `end_dev` as an ARRAY — see buildAddValve. */
  ADD_END_DEVICE: 1,
  /** App->GW. Delete / de-register an end device. Same `end_dev` array shape. */
  REMOVE_END_DEVICE: 2,
  /** App->GW. Ask for a water timer's status. The reply is the same shape the gateway pushes. */
  STATUS: 3,
  /** App->GW. Write a watering plan INTO the gateway. Deliberately unused — see deleteWaterPlan. */
  SETUP_WATER_PLAN: 4,
  /** App->GW. Delete a watering plan from the gateway. */
  REMOVE_WATER_PLAN: 5,
  /** App->GW. Start watering now, with a duration and (nominally) a volume cap. */
  START: 6,
  /** App->GW. Stop watering now. */
  STOP: 7,
  /** App->GW. Enable/disable a specific alert. */
  ALERT_ENABLEMENT: 10,
  /** App->GW. Dismiss a raised alert. */
  ALERT_DISMISS: 11,
  /** App->GW. Child-lock state. */
  LOCKOUT_STATE: 12,
  /** App->GW. Read gateway configuration — this is where `vol_unit` comes from. */
  GET_CONFIGURATION: 16,
  /** App->GW. Write gateway configuration. */
  SET_CONFIGURATION: 17,
} as const;

/** The gateway's configured volume unit (`cmd 16` → `vol_unit`). MVP's gateway is `gal`. */
export type GatewayVolUnit = 'gal' | 'L';

export const LITRES_PER_GALLON = 3.785411784;

/**
 * The idle-latch guard, carried over from the app verbatim. A CLOSED valve reports GARBAGE in
 * `volume` — the live GW-02 sat at 15,886,307.00 while idle. Anything past this bound is that
 * latch, not water, and must never reach a limit comparison or the usage history.
 */
export const MAX_PLAUSIBLE_CYCLE_VOLUME = 100_000;

export interface GatewayTarget {
  host: string;
  gatewayId: string;
}

/**
 * The gateway wraps its reply in HTML unless you turn that off in its admin page
 * (`<!--#RET-->{"cmd":x,"gw_id":"...","ret":y}` inside a <body>). Since we cannot assume an
 * operator has changed that setting, unwrap unconditionally. Byte-for-byte the app's
 * `extractJsonFromMaybeHtml` — the shared-fixture rule applies, change them together.
 */
export function extractJsonFromMaybeHtml(rawText: string): string {
  if (rawText.includes('<html') || rawText.includes('<body')) {
    const match = rawText.match(/\{[\s\S]*\}/);
    if (match) return match[0];
  }
  return rawText;
}

/** LinkTap's "is watering" flag arrives as true/'true'/1/'1' depending on firmware and source. */
export function coerceWateringBool(v: unknown): boolean {
  return v === true || v === 'true' || v === 1 || v === '1';
}

/**
 * Does this valve actually METER flow? `is_flm_plugin` is the authoritative answer when present;
 * otherwise fall back to whether a numeric volume field exists at all. LinkTap meters on the G2 and
 * G2S only, so a non-metering valve can only ever be bounded by TIME.
 */
export function reportsVolume(data: any): boolean {
  if (!data) return false;
  if (typeof data.is_flm_plugin === 'boolean') return data.is_flm_plugin;
  const raw = data.volume ?? data.vol;
  return raw != null && Number.isFinite(Number(raw));
}

/**
 * Litres delivered THIS CYCLE.
 *
 * ⚠️ MEASURED ON HARDWARE 2026-08-18, and this field has been read wrong TWICE in opposite
 * directions — do not re-derive it from intuition:
 *   * `volume` is the CYCLE TOTAL in the gateway's configured unit (`vol_unit`), and it RESETS at
 *     the start of each cycle. It is NOT LinkTap's cloud `vol` (millilitres, a field the LAN
 *     payload does not even carry) and NOT a lifetime counter in thousandths.
 *   * Reading it as a counter in thousandths under-reported by ~1000x (0.63 gal read as 0.0024 L).
 *     That is the DANGEROUS direction: a software cutoff compares against this number, so it could
 *     never arm.
 * The full evidence table lives in the app's `utils/linktapStatus.ts`.
 */
export function cycleVolumeLitres(data: any, volUnit: GatewayVolUnit): number {
  const raw = Number(data?.volume ?? NaN);
  if (!Number.isFinite(raw) || raw <= 0 || raw > MAX_PLAUSIBLE_CYCLE_VOLUME) return 0;
  return volUnit === 'gal' ? raw * LITRES_PER_GALLON : raw;
}

// ── command builders (pure) ────────────────────────────────────────────────────────────────────

export function buildStatus(t: GatewayTarget, devId: string) {
  return { cmd: LT_CMD.STATUS, gw_id: t.gatewayId, dev_id: devId };
}

export function buildGetConfiguration(t: GatewayTarget) {
  return { cmd: LT_CMD.GET_CONFIGURATION, gw_id: t.gatewayId };
}

/**
 * Start a cycle. `duration` is SECONDS (not minutes — another field that has been guessed wrong).
 *
 * ⚠️ `volume_limit` IS SENT BUT MUST NOT BE TRUSTED. Measured behaviour, carried over from the app:
 * "LinkTap hardware often ignores volume limits passed to cmd: 6, so we must enforce it here."
 * The hub therefore watches `volume` on every status and issues its own STOP — sending the cap is
 * belt-and-braces, the software cutoff is the actual enforcement. A non-metering valve
 * (`is_flm_plugin` false) can only be bounded by duration at all; say so rather than implying a
 * volume limit is holding.
 */
export function buildStart(t: GatewayTarget, devId: string, durationSecs: number, volumeLimit?: number) {
  const body: Record<string, unknown> = {
    cmd: LT_CMD.START,
    gw_id: t.gatewayId,
    dev_id: devId,
    duration: Math.max(1, Math.floor(durationSecs)),
  };
  if (volumeLimit != null && volumeLimit > 0) body.volume_limit = volumeLimit;
  return body;
}

export function buildStop(t: GatewayTarget, devId: string) {
  return { cmd: LT_CMD.STOP, gw_id: t.gatewayId, dev_id: devId };
}

/**
 * Delete any watering plan the gateway is holding for this valve.
 *
 * This exists because the HUB owns the schedule (owner ruling 2026-08-19: a cycle cut short by its
 * VOLUME cap must not restart, and "that logic does not exist on the valve alone"). A plan left
 * behind in the gateway by the LinkTap app would keep firing on its own clock, independently of the
 * hub, and nothing would reconcile the two. So adopting a gateway means clearing its plans, not
 * merely ignoring them. CMD 4 (write plan) is deliberately NOT offered here.
 */
export function buildDeleteWaterPlan(t: GatewayTarget, devId: string) {
  return { cmd: LT_CMD.REMOVE_WATER_PLAN, gw_id: t.gatewayId, dev_id: devId };
}

/**
 * Register a valve to the gateway WITHOUT the LinkTap app — the whole point of the hub-only model.
 *
 * ⚠️ TWO THINGS TO KNOW BEFORE TRUSTING THIS.
 *
 * 1. THE FIELD IS `end_dev`, AND IT IS AN ARRAY. Every other command in this protocol takes a
 *    singular `dev_id`; this one does not. Source: the Hubitat MQTT driver
 *    (pedroandrade1977/Hubitat-LinktapMQTT), which implements the full message set —
 *    `'register device': ["cmd": 1, "gw_id": ..., "end_dev": [...]]`. It is valid over HTTP because
 *    §4.2 defines the HTTP and MQTT message sets as the same messages (see the header).
 *
 * 2. NOBODY HAS EVER RUN THIS OVER HTTP, so it is SPECIFIED, not PROVEN. openHAB declares
 *    `CMD_ADD_END_DEVICE = 1` but ships no add-device frame at all, and the Home Assistant
 *    local-HTTP component's own FAQ tells you to add the valve in the mobile app first. What the
 *    documents cannot tell us is the RF side: whether the gateway opens a listen window, whether
 *    the valve needs its pairing button held, and what the `ret` codes mean. Bench-verify on the
 *    lab gateway before this becomes a product claim — and never probe it against MVP, which is
 *    read-only production kit.
 */
export function buildAddValve(t: GatewayTarget, devIds: string[]) {
  return { cmd: LT_CMD.ADD_END_DEVICE, gw_id: t.gatewayId, end_dev: devIds };
}

/** Remove valves from the gateway. Same `end_dev` array shape as buildAddValve. */
export function buildRemoveValve(t: GatewayTarget, devIds: string[]) {
  return { cmd: LT_CMD.REMOVE_END_DEVICE, gw_id: t.gatewayId, end_dev: devIds };
}

// ── transport ──────────────────────────────────────────────────────────────────────────────────

export interface GatewayReply {
  ok: boolean;
  /** The gateway's own `ret` code when it sent one. 0 is success on every command we have seen. */
  ret?: number;
  data?: any;
  error?: string;
}

/**
 * POST one command and parse the reply. Never throws — a gateway that is unplugged, rebooting or
 * mid-RF-retry must degrade to `ok:false`, because the caller's poll loop has nothing better to do
 * than try again. Same reasoning as the Cradlepoint poller.
 */
export async function postCommand(
  t: GatewayTarget,
  body: Record<string, unknown>,
  fetchImpl: typeof fetch = fetch,
): Promise<GatewayReply> {
  try {
    const res = await fetchImpl(`http://${t.host}/api.shtml`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(GATEWAY_TIMEOUT_MS),
    });
    if (!res.ok) return { ok: false, error: `gateway returned HTTP ${res.status}` };
    const text = extractJsonFromMaybeHtml(await res.text());
    let parsed: any;
    try {
      parsed = JSON.parse(text);
    } catch {
      return { ok: false, error: 'gateway reply was not JSON' };
    }
    const ret = typeof parsed?.ret === 'number' ? parsed.ret : undefined;
    // `ret` is absent on the status replies (cmd 3), which carry the payload instead — so absence
    // is not failure. Only an explicitly non-zero ret is.
    if (ret != null && ret !== 0) return { ok: false, ret, data: parsed, error: `gateway ret=${ret}` };
    return { ok: true, ret, data: parsed };
  } catch (e: any) {
    return { ok: false, error: e?.name === 'TimeoutError' ? 'gateway timed out' : String(e?.message || e) };
  }
}

/**
 * Read the gateway's configuration and pull out the volume unit, which every volume number in this
 * protocol is expressed in. Defaults to gallons on an unreadable/absent value because that is what
 * the shipped hardware reports and guessing litres would under-report by 3.79x.
 */
export async function readVolUnit(
  t: GatewayTarget,
  fetchImpl: typeof fetch = fetch,
): Promise<GatewayVolUnit> {
  const reply = await postCommand(t, buildGetConfiguration(t), fetchImpl);
  return reply.data?.vol_unit === 'L' ? 'L' : 'gal';
}
