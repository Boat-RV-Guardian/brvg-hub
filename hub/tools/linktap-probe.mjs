#!/usr/bin/env node
// Bench probe for the LinkTap gateway's LOCAL HTTP API — the tool that turns "specified" into
// "proven" for cloud-free operation (owner ask 2026-08-19: "need to confirm by HTTP not MQTT").
//
// READ-ONLY BY DEFAULT. It reads the gateway config (cmd 16) and a valve's status (cmd 3) and
// prints the RAW reply, because the raw reply is the evidence — every unit bug in this protocol
// came from someone summarising a payload instead of recording it.
//
// The interesting, UNPROVEN part is pairing: CMD 1 (add) and CMD 2 (remove) are documented as
// App->GW, and §4.2 of LinkTap's integration PDF says HTTP carries every App->GW message — but no
// third-party implementation has ever exercised CMD 1 over HTTP, so the RF side is unknown. Does
// the gateway open a listen window? Must the valve's button be held? What do the ret codes mean?
// Only hardware answers that, which is why --add and --remove exist and why they are opt-in.
//
//   node linktap-probe.mjs --host 192.168.1.107 --gw CCCCDDDDEEEEFFFF
//   node linktap-probe.mjs --host … --gw … --dev aaaabbbbccccdddd
//   node linktap-probe.mjs --host … --gw … --dev … --add
//   node linktap-probe.mjs --host … --gw … --dev … --remove
//
// ⚠️ NEVER point this at MVP (the owner's production vehicle). Lab gateway only — --add and
// --remove MUTATE the gateway's device registry.

const args = process.argv.slice(2);
const flag = (n) => args.includes(`--${n}`);
const val = (n) => { const i = args.indexOf(`--${n}`); return i >= 0 ? args[i + 1] : undefined; };

const host = val('host');
const gw = val('gw');
const dev = val('dev');

if (!host || !gw) {
  console.error('usage: linktap-probe.mjs --host <gateway-ip> --gw <gateway-id> [--dev <dev-id>] [--add|--remove]');
  process.exit(2);
}

/**
 * The gateway's full return-code table, recovered from the Hubitat MQTT driver — the only published
 * decoding of this field. Printing the meaning beats printing a bare number at 2am on a boat.
 */
const RET = {
  0: 'Success',
  1: 'Message format error',
  2: 'CMD message not supported',
  3: 'Gateway ID not matched',
  4: 'End device ID error',
  5: 'End device ID not found',
  6: 'Gateway internal error',
  7: 'Conflict with watering plan',
};

/** Unwrap the HTML the gateway wraps replies in unless that is disabled in its admin page. */
function unwrap(text) {
  if (text.includes('<html') || text.includes('<body')) {
    const m = text.match(/\{[\s\S]*\}/);
    if (m) return m[0];
  }
  return text;
}

async function send(label, body) {
  process.stdout.write(`\n── ${label}\n   → ${JSON.stringify(body)}\n`);
  const started = Date.now();
  try {
    const res = await fetch(`http://${host}/api.shtml`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(15_000),
    });
    const raw = await res.text();
    const ms = Date.now() - started;
    console.log(`   ← HTTP ${res.status} in ${ms}ms`);
    console.log(`   ← raw: ${raw.trim().replace(/\s+/g, ' ').slice(0, 400)}`);
    const text = unwrap(raw);
    try {
      const json = JSON.parse(text);
      console.log(`   ← parsed: ${JSON.stringify(json)}`);
      if (typeof json.ret === 'number') {
        console.log(`   ← ret=${json.ret} — ${RET[json.ret] ?? 'UNKNOWN CODE, record it'}`);
        if (json.ret === 5) console.log('     ⇒ the gateway could not reach that valve: powered? in RF range? in pairing mode?');
        if (json.ret === 7) console.log('     ⇒ a watering PLAN is in the way — clear it with cmd 5 and retry');
      }
      return json;
    } catch {
      console.log('   ⚠️ reply did not parse as JSON — record the raw form above');
      return null;
    }
  } catch (e) {
    console.log(`   ✗ ${e?.name === 'TimeoutError' ? 'timed out after 15s' : e?.message || e}`);
    return null;
  }
}

console.log(`LinkTap local HTTP probe → http://${host}/api.shtml  (gateway ${gw})`);

// cmd 16 — configuration. This is where vol_unit comes from, and vol_unit decides how every
// volume number in this protocol is read. Getting it wrong is a 3.79x error.
const cfg = await send('cmd 16  read gateway configuration', { cmd: 16, gw_id: gw });
if (cfg?.vol_unit) {
  console.log(`   ⇒ volume unit is ${cfg.vol_unit.toUpperCase()} — every volume/speed field is in this unit`);
}

if (dev) {
  // cmd 3 — status. Note `volume` here is the CYCLE TOTAL in vol_unit, and reads garbage
  // (~15.9M on the live GW-02) while the valve is CLOSED. That is expected, not a fault.
  const st = await send('cmd 3   read valve status', { cmd: 3, gw_id: gw, dev_id: dev });
  const d = st?.dev_stat?.[0] ?? st;
  if (d && typeof d === 'object') {
    console.log(`   ⇒ meters flow (is_flm_plugin): ${d.is_flm_plugin ?? '(not reported)'}`);
    console.log(`   ⇒ volume: ${d.volume ?? '(none)'} — if the valve is CLOSED, a huge number here is the known idle latch`);
  }
}

if (flag('add') || flag('remove')) {
  if (!dev) { console.error('\n--add/--remove need --dev <dev-id>'); process.exit(2); }
  const adding = flag('add');
  console.log(`\n⚠️  MUTATING the gateway's device registry (${adding ? 'CMD 1 add' : 'CMD 2 remove'}).`);
  console.log('    The MESSAGE is proven (a shipping Hubitat driver implements this exact command');
  console.log('    over MQTT, and the vendor defines HTTP as carrying the same messages). What is');
  console.log('    unproven is this TRANSPORT on this firmware. Expect ret=0 on success; ret=5 means');
  console.log('    the gateway could not reach the valve, so have it powered and in pairing mode.');
  // ⚠️ end_dev is an ARRAY here, unlike the singular dev_id every other command takes.
  const reply = await send(
    adding ? 'cmd 1   ADD end device' : 'cmd 2   REMOVE end device',
    { cmd: adding ? 1 : 2, gw_id: gw, end_dev: [dev] },
  );
  if (reply && reply.ret === 0) {
    console.log('\n   ⇒ ret=0. Re-run WITHOUT --add/--remove and check cmd 3 to confirm the');
    console.log('     registry actually changed — a 0 return is not the same as an RF join.');
  }
}

console.log('\nDone. Paste this whole output into the PR or open-tasks — the raw replies are the evidence.');
