#!/bin/sh
# BRVG phone-home agent — Phase A skeleton (telemetry push; the command channel is Phase B).
#
# One POSIX-shell agent, two homes: a GL.iNet router (busybox ash, AT commands straight to the
# modem port — the agent runs as root on-device, so no RPC login is needed) and a Raspberry
# Pi-class hub (gpsd or a raw NMEA serial dongle). It pushes GPS and modem telemetry OUTBOUND
# over HTTPS to the hosted worker on a timer — no inbound path exists behind CGNAT, and none is
# needed. The command channel (Phase B) is deliberately absent from this skeleton.
#
# Auth: prefer the per-DEVICE revocable token (DEVICE_TOKEN → /api/agent, minted by
# /api/agent/enroll — worker increment 1), falling back to the per-vehicle webhook key
# (VEHICLE_KEY → /api/shelly) for configs written before tokens existed. A leaked token exposes
# one device's telemetry write path and is revoked individually; prefer it everywhere.
#
# Frugality: this rides the customer's own metered link. Cadences are configurable with floors,
# every request is a single small GET, and a failed send is dropped (next tick retries) rather
# than queued — Phase A is telemetry, not a store-and-forward system.
#
# Everything parseable is in awk functions at the top, exercised by agent/test.sh with strings
# captured from real hardware (GL-X750 bench session 2026-08-06).
#
# Self-update (owner requirement: "build the secure solution"). The agent NEVER downloads or
# evaluates code it was handed. `self_update` takes NO argument: it asks opkg to install from the
# SIGNED feed the router is already configured with, so WHAT gets installed is decided by the
# signed index and verified on-device by the package manager, while the cloud only decides WHO is
# told to update and WHEN (staged rollout). The previous agent is kept and automatically restored
# if the new one cannot even report its own version.

AGENT_VERSION="0.8.0"
AGENT_BACKUP="/etc/brvg-agent.prev"


# --- Pure parsers (stdin → stdout; empty output = no data) -------------------------------------

# AT+QGPSLOC=2 response → "lat lon acc" (acc = hdop*5, rough). Rejects 0/0 and out-of-range.
parse_qgpsloc() {
  awk -F'[:,]' '/\+QGPSLOC/ {
    lat = $3 + 0; lon = $4 + 0; hdop = $5 + 0
    if (lat == 0 && lon == 0) exit
    if (lat > 90 || lat < -90 || lon > 180 || lon < -180) exit
    # %.5f ≈ 1 m resolution; awk default %.6g would truncate 3-digit longitudes
    if (hdop > 0) printf "%.5f %.5f %.0f\n", lat, lon, hdop * 5
    else printf "%.5f %.5f\n", lat, lon
    exit
  }'
}

# AT+QCSQ response → "mode rssi rsrp sinr_db rsrq" (LTE only: raw sinr 0..250 → dB).
parse_qcsq() {
  awk '/\+QCSQ/ {
    line = $0; sub(/.*\+QCSQ:[ ]*/, "", line); gsub(/\r/, "", line)
    n = split(line, a, ",")
    gsub(/"/, "", a[1])
    if (a[1] == "LTE" && n >= 5) printf "%s %s %s %s %s\n", a[1], a[2], a[3], (a[4] / 5) - 20, a[5]
    else if (n >= 3) printf "%s %s %s\n", a[1], a[2], a[3]
    exit
  }'
}

# AT+COPS? response → carrier name (spaces preserved; caller URL-encodes).
parse_cops() {
  awk -F'"' '/\+COPS/ { print $2; exit }'
}

# AT+QGDCNT? response → "sent received" bytes since the counter was last reset.
parse_qgdcnt() {
  awk -F'[:,]' '/\+QGDCNT/ { gsub(/[^0-9]/, "", $2); gsub(/[^0-9]/, "", $3); if ($2 != "" && $3 != "") print $2, $3; exit }'
}

# AT+CPIN? response → ok | locked | missing
parse_cpin() {
  awk '/\+CPIN: READY/ { print "ok"; exit }
       /\+CPIN: SIM PIN|\+CPIN: SIM PUK/ { print "locked"; exit }
       /CME ERROR: 10|SIM not inserted/ { print "missing"; exit }'
}

# NMEA RMC sentence(s) → "lat lon" from the last valid fix (ddmm.mmmm → decimal degrees).
parse_nmea_rmc() {
  awk -F, '/RMC/ && $3 == "A" {
    v = $4 + 0; d = int(v / 100); lat = d + (v - d * 100) / 60; if ($5 == "S") lat = -lat
    v = $6 + 0; d = int(v / 100); lon = d + (v - d * 100) / 60; if ($7 == "W") lon = -lon
    if (lat == 0 && lon == 0) next
    # %.5f (≈1 m), matching the modem path — awk default OFMT is %.6g, which drops the USB dongle
    # to ~4 decimals (~11 m). Verified live on a u-blox 7 (bench 2026-08-13).
    out = sprintf("%.5f %.5f", lat, lon)
  } END { if (out != "") print out }'
}

# gpsd TPV JSON (gpspipe -w) → "lat lon [acc]" from the last 2D/3D fix.
parse_gpsd_tpv() {
  awk '/"class":"TPV"/ && /"mode":[23]/ {
    lat = ""; lon = ""; acc = ""
    if (match($0, /"lat":[-0-9.]+/))  lat = substr($0, RSTART + 6, RLENGTH - 6)
    if (match($0, /"lon":[-0-9.]+/))  lon = substr($0, RSTART + 6, RLENGTH - 6)
    if (match($0, /"eph":[0-9.]+/))   acc = substr($0, RSTART + 6, RLENGTH - 6)
    if (lat != "" && lon != "") out = lat " " lon (acc != "" ? " " acc : "")
  } END { if (out != "") print out }'
}

urlencode_spaces() { printf '%s' "$1" | sed 's/ /%20/g; s/&/%26/g'; }

# --- Relay: spool → batch report (HUB-PROXY.md, relay tier) ------------------------------------
# The CGI receiver (hub-lite-cgi.sh) appends webhook lines to a spool; these functions roll the spool
# up into ONE batch POST. Wire contract: brvg-cloud-server/src/agentBatch.ts (v1); the canonical
# fixture there is what agent/test.sh checks this output against.
#
# ⚠️ On the saving: an earlier version of this comment claimed the TLS handshake dominates, so
# collapsing connections saved most of the data. MEASURED IN PRODUCTION 2026-08-14 and that is
# wrong — a mains sensor reuses TLS sessions and costs ~551 B per report, not the ~5 KB a fresh
# handshake would. The roll-up saves tens of MB/month across a few sensors: worth having, not an
# order of magnitude. The relay's real justification is LOCKDOWN — sensors that hand off on the LAN
# need no WAN egress, so the forward chain can be deny-all with no allow rules.

# Spool lines (epoch	device	event	rawquery) → the JSON items array, deduped per device+event
# KEEPING THE NEWEST (a later line overwrites — the spool is append-ordered). stdout: one line,
# a JSON array. Decode + escape here must match hub-lite-cgi.sh byte for byte (tested).
spool_to_items() {
  awk -F'	' '
    function urldec(s,  out, i, c, h, hex) {
      hex = "0123456789abcdef"; gsub(/\+/, " ", s); out = ""
      for (i = 1; i <= length(s); i++) {
        c = substr(s, i, 1)
        if (c == "%" && i + 2 <= length(s)) {
          h = tolower(substr(s, i + 1, 2))
          if (h ~ /^[0-9a-f][0-9a-f]$/) {
            out = out sprintf("%c", (index(hex, substr(h, 1, 1)) - 1) * 16 + index(hex, substr(h, 2, 1)) - 1)
            i += 2; continue
          }
        }
        out = out c
      }
      return out
    }
    # gsub replacement escaping is its own trap: "\\\\" collapses to one backslash (a no-op).
    # "\\\\&" = a literal backslash, then the match — the portable way to double it.
    function jesc(s) { gsub(/\\/, "\\\\&", s); gsub(/"/, "\\\"", s); gsub(/[\001-\037]/, "", s); return s }
    function params_json(q,  n, parts, i, eq, k, v, out, first) {
      n = split(q, parts, "&"); out = "{"; first = 1
      for (i = 1; i <= n; i++) {
        eq = index(parts[i], "="); if (eq == 0) continue
        k = substr(parts[i], 1, eq - 1); v = urldec(substr(parts[i], eq + 1))
        gsub(/[^A-Za-z0-9_.-]/, "", k); if (k == "") continue
        if (!first) out = out ","; first = 0
        out = out "\"" k "\":\"" jesc(v) "\""
      }
      return out "}"
    }
    NF >= 3 {
      dev = $2; ev = $3; q = (NF >= 4 ? $4 : "")
      gsub(/[^A-Za-z0-9_.:-]/, "", dev); gsub(/[^A-Za-z0-9_.-]/, "", ev)
      if (dev == "" || ev == "") next
      key = dev SUBSEP ev
      if (!(key in seen)) { order[++count] = key; seen[key] = 1 }
      line[key] = "{\"device\":\"" dev "\",\"event\":\"" ev "\",\"params\":" params_json(q) "}"
    }
    END {
      printf "["
      for (i = 1; i <= count; i++) { if (i > 1) printf ","; printf "%s", line[order[i]] }
      printf "]"
    }'
}

# The devices present in a spool, one per line (for the ok/changed split).
spool_devices() {
  awk -F'	' 'NF >= 3 { gsub(/[^A-Za-z0-9_.:-]/, "", $2); if ($2 != "" && !(($2) in s)) { s[$2] = 1; print $2 } }'
}

# Assemble the envelope. $1=seq $2=kind $3=items-json-array $4=ok ids (space-separated) $5=boot id.
build_batch_json() {
  _ok="["
  _first=1
  for _d in $4; do
    [ "$_first" = 1 ] || _ok="$_ok,"
    _first=0
    _ok="$_ok\"$_d\""
  done
  _ok="$_ok]"
  printf '{"v":1,"seq":%s,"boot":"%s","kind":"%s","items":%s,"ok":%s,"agent":{"av":"%s","tier":"hub-lite"}}'     "$1" "$5" "$2" "${3:-[]}" "$_ok" "$AGENT_VERSION"
}

RELAY_SPOOL="${BRVG_RELAY_SPOOL:-/tmp/brvg-relay.spool}"
RELAY_SEQ_FILE="${BRVG_RELAY_SEQ:-/tmp/brvg-relay.seq}"
RELAY_STATE_DIR="${BRVG_RELAY_STATE:-/tmp/brvg-relay-state}"
RELAY_BOOT_FILE="${BRVG_RELAY_BOOT:-/tmp/brvg-relay.boot}"

# Per-boot id, so the cloud can tell "this router rebooted and its counter restarted" from "this is
# a replay". WITHOUT IT THE COUNTER RESET IS SILENT DATA LOSS: /tmp is tmpfs on OpenWrt, so a power
# cut — routine on a boat — wipes the spool, the counter and the state together; the counter goes
# back to 1, the cloud still holds the old high-water mark, and every batch comes back
# `200 {duplicate:true}`. The drain below reads that as success and deletes the spool. See the
# `isNewSeq` comment in brvg-cloud-server/src/agentBatch.ts.
#
# The file lives in the SAME tmpfs as the counter, which is exactly what makes this correct: the id
# and the counter can only ever disappear together, so a new id always accompanies a reset.
relay_boot_id() {
  if [ -s "$RELAY_BOOT_FILE" ]; then cat "$RELAY_BOOT_FILE"; return 0; fi
  # The kernel's own per-boot UUID where it exists (every Linux since 2.6, OpenWrt included);
  # urandom, then pid+uptime, only so this can never return empty and emit `"boot":""`.
  # Readability tested first: `< missing 2>/dev/null` silences the COMMAND, not the shell's own
  # redirection error, so without the guard this prints to stderr on any host without /proc.
  _b=$([ -r /proc/sys/kernel/random/boot_id ] && tr -d '-' < /proc/sys/kernel/random/boot_id | cut -c1-32)
  [ -n "$_b" ] || _b=$(od -An -N8 -tx1 /dev/urandom 2>/dev/null | tr -d ' \n')
  [ -n "$_b" ] || _b="p$$u$(cut -d. -f1 /proc/uptime 2>/dev/null)"
  printf '%s' "$_b" | tr -cd 'A-Za-z0-9' > "$RELAY_BOOT_FILE"
  cat "$RELAY_BOOT_FILE"
}

# One drain: move the spool aside (an append that races lands in the NEXT drain), split devices
# into changed (items) vs unchanged (ok) against the last-sent state, POST, and only on success
# advance the sequence and the state. On failure the batch is prepended back for retry UNDER THE
# SAME SEQ — that is what lets the server drop a replay whole instead of re-alerting.
drain_relay() {
  # BOTH sources matter: fresh spool lines, AND a .sending file a failed drain left behind. Found
  # on the bench 2026-08-13: checking only the live spool meant a failed batch was never retried
  # until NEW telemetry arrived — on a quiet vessel, never.
  _sending="$RELAY_SPOOL.sending"
  [ -s "$RELAY_SPOOL" ] || [ -s "$_sending" ] || return 0
  # A previous failed drain left a .sending file — retry it first, oldest data wins.
  if [ ! -s "$_sending" ]; then
    mv "$RELAY_SPOOL" "$_sending" 2>/dev/null || return 0
  fi
  mkdir -p "$RELAY_STATE_DIR"
  _seq=$( (cat "$RELAY_SEQ_FILE" 2>/dev/null || echo 0) | tr -cd '0-9' )
  _seq=$(( ${_seq:-0} + 1 ))

  # Split: a device whose newest spooled line matches its last-SENT line has nothing new — it goes
  # in `ok` (freshness only). Everything else ships as items. Keyframe every Nth drain resends all.
  _kind="delta"
  [ $(( _seq % ${KEYFRAME_EVERY:-6} )) -eq 0 ] && _kind="keyframe"
  _items_src="$_sending.items"
  : > "$_items_src"
  _ok_ids=""
  while IFS= read -r _dev; do
    _newest=$(awk -F'	' -v d="$_dev" '$2 == d' "$_sending" | tail -1)
    _sig=$(printf '%s' "$_newest" | cut -f3-)
    _state="$RELAY_STATE_DIR/$_dev.last"
    if [ "$_kind" = "delta" ] && [ -f "$_state" ] && [ "$(cat "$_state")" = "$_sig" ]; then
      _ok_ids="${_ok_ids:+$_ok_ids }$_dev"
    else
      awk -F'	' -v d="$_dev" '$2 == d' "$_sending" >> "$_items_src"
    fi
  done <<EOF_DEVS
$(spool_devices < "$_sending")
EOF_DEVS

  _items=$(spool_to_items < "$_items_src")
  _body=$(build_batch_json "$_seq" "$_kind" "$_items" "$_ok_ids" "$(relay_boot_id)")
  _url="${WORKER_URL}/api/agent/batch?vid=${VID}&device=${DEVICE_ID}&t=${DEVICE_TOKEN}"
  # Same command piggyback + ack as send_event: the batch reply carries pending verbs, and the
  # request that delivers acks is the next one out — whichever path (event or batch) goes first.
  [ -n "$PENDING_ACK" ] && _url="${_url}&ack=${PENDING_ACK}"
  if _resp=$(curl -fsS --max-time 20 -X POST -H 'Content-Type: application/json' -d "$_body" "$_url" 2>/dev/null); then
    PENDING_ACK=""
    echo "$_seq" > "$RELAY_SEQ_FILE"
    # Persist last-sent per device so the next delta knows what "unchanged" means.
    while IFS= read -r _dev; do
      awk -F'	' -v d="$_dev" '$2 == d' "$_sending" | tail -1 | cut -f3- > "$RELAY_STATE_DIR/$_dev.last"
    done <<EOF_DEVS2
$(spool_devices < "$_sending")
EOF_DEVS2
    rm -f "$_sending" "$_items_src"
    log "relay: drained batch seq=$_seq ($_kind)"
    _cmds=$(printf '%s' "$_resp" | parse_commands)
    [ -n "$_cmds" ] && run_commands "$_cmds"
  else
    rm -f "$_items_src"
    log "relay: batch seq=$_seq failed (will retry with the same seq)"
  fi
}

# --- Config ------------------------------------------------------------------------------------

CONF="${BRVG_AGENT_CONF:-/etc/brvg-agent.conf}"

load_config() {
  # shellcheck disable=SC1090
  [ -f "$CONF" ] && . "$CONF"
  WORKER_URL="${WORKER_URL:-https://api.boatrvguardian.com}"
  GPS_INTERVAL="${GPS_INTERVAL:-120}"
  MODEM_INTERVAL="${MODEM_INTERVAL:-600}"
  [ "$GPS_INTERVAL" -lt 30 ] && GPS_INTERVAL=30       # floors: a metered link is not a firehose
  [ "$MODEM_INTERVAL" -lt 60 ] && MODEM_INTERVAL=60
  AT_PORT="${AT_PORT:-/dev/ttyUSB2}"                  # GL-X750; X3000-class PCIe modems differ — see README
  GPS_SOURCE="${GPS_SOURCE:-auto}"                    # auto | at | gpsd | nmea
  GPS_DEVICE="${GPS_DEVICE:-}"                        # serial NMEA dongle for GPS_SOURCE=nmea
  if [ -z "$VID" ] || [ -z "$DEVICE_ID" ]; then
    echo "brvg-agent: VID and DEVICE_ID are required in $CONF" >&2
    exit 1
  fi
  if [ -z "${DEVICE_TOKEN:-}" ] && [ -z "${VEHICLE_KEY:-}" ]; then
    echo "brvg-agent: DEVICE_TOKEN (preferred) or VEHICLE_KEY is required in $CONF" >&2
    exit 1
  fi
  case "$WORKER_URL" in
    https://*) : ;;
    *) echo "brvg-agent: WORKER_URL must be https" >&2; exit 1 ;;
  esac
}

log() { echo "brvg-agent: $*" >&2; }

# --- AT transport (GL.iNet path — root on-device, straight to the modem port) ------------------

AT_BUF="${TMPDIR:-/tmp}/brvg-agent.at.$$"

at_cmd() {
  # $1 = command, $2 = read window seconds (send_at blocks modem-side; GNSS reads answer fast)
  [ -c "$AT_PORT" ] || return 1
  : > "$AT_BUF"
  cat "$AT_PORT" > "$AT_BUF" 2>/dev/null &
  _cat=$!
  printf '%s\r' "$1" > "$AT_PORT" 2>/dev/null || { kill "$_cat" 2>/dev/null; return 1; }
  sleep "${2:-3}"
  kill "$_cat" 2>/dev/null
  wait "$_cat" 2>/dev/null
  cat "$AT_BUF"
}

# --- Collectors --------------------------------------------------------------------------------

detect_platform() {
  if [ -n "$PLATFORM" ]; then echo "$PLATFORM"
  elif [ -f /etc/glversion ] || [ -d /etc/gl-metadata ]; then echo glinet
  else echo generic
  fi
}

# A plugged-in USB GPS receiver, if any. This is the answer for routers whose modem has no GPS
# antenna port (hardware-verified on a GL-X750, 2026-08-06): a ~$15 u-blox dongle appears as a
# serial device streaming NMEA, and costs nothing to check for. Requires the kernel modules
# (kmod-usb-acm / kmod-usb-serial-*) — see agent/README.md.
find_nmea_device() {
  [ -n "$GPS_DEVICE" ] && [ -c "$GPS_DEVICE" ] && { echo "$GPS_DEVICE"; return 0; }
  for _d in /dev/ttyACM0 /dev/ttyACM1 /dev/ttyUSB3 /dev/ttyUSB4; do
    [ "$_d" = "$AT_PORT" ] && continue          # never the modem's own AT port
    [ -c "$_d" ] || continue
    echo "$_d"; return 0
  done
  return 1
}

read_nmea_device() {
  _dev=$(find_nmea_device) || return 1
  # head -c bounds the read on a device that streams forever; timeout guards a silent one.
  timeout 6 head -c 4096 "$_dev" 2>/dev/null | parse_nmea_rmc
}

collect_gps() {
  case "$GPS_SOURCE" in
    at) at_cmd 'AT+QGPSLOC=2' 3 | parse_qgpsloc ;;
    gpsd) command -v gpspipe >/dev/null 2>&1 && gpspipe -w -n 8 2>/dev/null | parse_gpsd_tpv ;;
    nmea) read_nmea_device ;;
    auto)
      # Modem GNSS first (no extra hardware), then a USB dongle, then gpsd. The fallback ORDER is
      # the point: a router with no GPS antenna port answers the AT read forever with "no fix",
      # so the dongle has to be tried even when the modem is present and healthy.
      _fix=""
      if [ "$(detect_platform)" = "glinet" ]; then
        _fix=$(at_cmd 'AT+QGPSLOC=2' 3 | parse_qgpsloc)
      fi
      if [ -z "$_fix" ]; then _fix=$(read_nmea_device); fi
      if [ -z "$_fix" ] && command -v gpspipe >/dev/null 2>&1; then
        _fix=$(gpspipe -w -n 8 2>/dev/null | parse_gpsd_tpv)
      fi
      [ -n "$_fix" ] && echo "$_fix" ;;
  esac
}

collect_modem() {
  # Only meaningful where an AT port exists (router / cellular HAT). "" elsewhere is fine —
  # a Pi hub on shore Wi-Fi simply has no modem story to tell.
  [ -c "$AT_PORT" ] || return 0
  sig=$(at_cmd 'AT+QCSQ' 2 | parse_qcsq)
  carrier=$(at_cmd 'AT+COPS?' 2 | parse_cops)
  sim=$(at_cmd 'AT+CPIN?' 2 | parse_cpin)
  data=$(at_cmd 'AT+QGDCNT?' 2 | parse_qgdcnt)
  echo "${sig}|${carrier}|${sim}|${data}"
}

# --- Push --------------------------------------------------------------------------------------

# Pure: report URL for an event + pre-encoded params (exercised by test.sh). Token path wins.
build_report_url() {
  if [ -n "${DEVICE_TOKEN:-}" ]; then
    printf '%s/api/agent?vid=%s&device=%s&event=%s&t=%s&%s' "$WORKER_URL" "$VID" "$DEVICE_ID" "$1" "$DEVICE_TOKEN" "$2"
  else
    printf '%s/api/shelly?vid=%s&device=%s&event=%s&k=%s&%s' "$WORKER_URL" "$VID" "$DEVICE_ID" "$1" "$VEHICLE_KEY" "$2"
  fi
}

# Commands acknowledged on the NEXT report (Phase B — see the worker's agentCommands.ts).
PENDING_ACK=""

# Set by run_commands when a verb's EFFECT needs to reach the cloud now rather than at the next
# tick. Commands arrive as the reply to a report, so the report that delivered them was composed
# BEFORE they ran — without this, "reset the data counter" shows the old counter for up to
# MODEM_INTERVAL, and "report now" does nothing observable at all. The main loop consumes it (see
# the note there): doing the follow-up send from inside send_event would re-enter send_event from
# its own body, and one failed curl mid-recursion loses the ack list.
FOLLOWUP_REPORT=0

# Pure: extract "id:cmd" pairs from the worker's JSON reply. Deliberately tiny — busybox has no
# JSON parser, and the payload shape is fixed and small: {"commands":[{"id":"..","cmd":".."}]}.
parse_commands() {
  tr -d ' \n' | sed -n 's/.*"commands":\[\(.*\)\].*/\1/p' \
    | sed 's/},{/}\n{/g' \
    | sed -n 's/.*"id":"\([A-Za-z0-9_-]*\)".*"cmd":"\([a-z_]*\)".*/\1:\2/p'
}

send_event() {
  # $1 = event name, $2 = pre-encoded param string ("lat=..&lon=..")
  _url=$(build_report_url "$1" "$2")
  [ -n "$PENDING_ACK" ] && _url="${_url}&ack=${PENDING_ACK}"
  _resp=$(curl -fsS --max-time 15 "$_url" 2>/dev/null)
  if [ $? -ne 0 ]; then
    log "send $1 failed (will retry next tick)"
    return 1
  fi
  PENDING_ACK=""   # the worker saw our acks; anything still queued comes back below
  _cmds=$(printf '%s' "$_resp" | parse_commands)
  [ -n "$_cmds" ] && run_commands "$_cmds"
  return 0
}

# Apply a Cloud Update Schedule: $1 = gps seconds, $2 = modem seconds. Writes the config so a
# restart keeps it, and updates the running loop so it takes effect on the next tick rather than at
# the next reboot. The floors in load_config still apply — a metered link is not a firehose.
set_intervals() {
  log "command: interval -> gps ${1}s modem ${2}s"
  GPS_INTERVAL="$1"
  MODEM_INTERVAL="$2"
  [ "$GPS_INTERVAL" -lt 30 ] && GPS_INTERVAL=30
  [ "$MODEM_INTERVAL" -lt 60 ] && MODEM_INTERVAL=60
  if [ -f /etc/brvg-agent.conf ]; then
    _tmp="/etc/brvg-agent.conf.$$"
    grep -vE '^[[:space:]]*(GPS_INTERVAL|MODEM_INTERVAL)=' /etc/brvg-agent.conf > "$_tmp" 2>/dev/null || true
    printf 'GPS_INTERVAL=%s\nMODEM_INTERVAL=%s\n' "$GPS_INTERVAL" "$MODEM_INTERVAL" >> "$_tmp"
    chmod 600 "$_tmp" 2>/dev/null
    mv "$_tmp" /etc/brvg-agent.conf
  fi
  FOLLOWUP_REPORT=1
}

# Execute the allowlisted verbs the cloud queued. An unknown verb is acknowledged and DROPPED —
# never passed to a shell — so a queue this agent doesn't understand can't become code execution.
run_commands() {
  for _entry in $1; do
    _id=${_entry%%:*}
    _cmd=${_entry#*:}
    case "$_cmd" in
      # FOLLOWUP_REPORT=1 on the verbs whose result is worth seeing immediately AND that leave the
      # uplink intact. Deliberately NOT set for reboot/reboot_modem (the link is about to drop, so
      # the extra send would just fail) or the update verbs (the agent is being replaced).
      report_now)   log "command: report_now"; FOLLOWUP_REPORT=1 ;;
      self_update)  log "command: self_update"; self_update ;;
      rollback_agent) log "command: rollback_agent"; restore_agent "requested" ;;
      reboot)       log "command: reboot"; (sleep 5; reboot) >/dev/null 2>&1 & ;;
      reboot_modem) log "command: reboot_modem"; at_cmd 'AT+CFUN=1,1' 5 >/dev/null 2>&1 ;;
      reset_data)   log "command: reset_data"; at_cmd 'AT+QGDCNT=0' 3 >/dev/null 2>&1; FOLLOWUP_REPORT=1 ;;
      # HIGH SECURITY: local administration off. A router bolted to a marina pole is physically
      # reachable by anyone; with SSH and the vendor web UI down, a thief's only route in is a
      # factory reset, which yields a blank router rather than this vehicle's network.
      #
      # This is SAFE BY CONSTRUCTION: a command can only reach us as the reply to a report that
      # SUCCEEDED, so cloud reachability is proven at the moment we disable the local doors. There
      # is no path where an already-offline router locks itself out.
      #
      # Recovery is deliberate, not accidental: factory reset, then restore from the app.
      local_admin_off)
        log "command: local_admin_off — disabling ssh + vendor web UI"
        uci set dropbear.@dropbear[0].enable='0' 2>/dev/null && uci commit dropbear 2>/dev/null
        /etc/init.d/dropbear stop 2>/dev/null
        /etc/init.d/dropbear disable 2>/dev/null
        # The vendor UI is nginx on GL.iNet 4.x; uhttpd on stock OpenWrt. Stop whichever exists.
        for _svc in nginx uhttpd; do
          [ -x "/etc/init.d/$_svc" ] || continue
          "/etc/init.d/$_svc" stop 2>/dev/null
          "/etc/init.d/$_svc" disable 2>/dev/null
        done
        FOLLOWUP_REPORT=1 ;;
      local_admin_on)
        log "command: local_admin_on — restoring ssh + vendor web UI"
        uci set dropbear.@dropbear[0].enable='1' 2>/dev/null && uci commit dropbear 2>/dev/null
        /etc/init.d/dropbear enable 2>/dev/null
        /etc/init.d/dropbear start 2>/dev/null
        for _svc in nginx uhttpd; do
          [ -x "/etc/init.d/$_svc" ] || continue
          "/etc/init.d/$_svc" enable 2>/dev/null
          "/etc/init.d/$_svc" start 2>/dev/null
        done
        FOLLOWUP_REPORT=1 ;;
      # Cloud Update Schedule pushed from the app. ONE VERB PER SCHEDULE, not a verb with a value:
      # parse_commands matches [a-z_]+ only, and the whole allowlist property is that a command can
      # never carry an argument. Persisted to the config so it survives a restart, and applied to the
      # live loop without one. GPS keeps its 30 s floor — the drag-detection rule needs it.
      interval_saver)    set_intervals 1800 3600 ;;
      interval_regular)  set_intervals 900 1800 ;;
      interval_often)    set_intervals 300 600 ;;
      interval_constant) set_intervals 30 300 ;;
      # Traffic lockdown on/off (Phase B first verbs, owner sprint 2026-08-17). ON re-arms the
      # watchdog's released marker so a later hub death can release again; OFF is the same
      # release the watchdog performs. Both safe-by-construction: they arrive only as the reply
      # to a report that SUCCEEDED, so cloud reachability is proven at the moment they run.
      lockdown_on)
        log "command: lockdown_on"
        if apply_lockdown; then rm -f "$HUB_WATCH_RELEASED" 2>/dev/null; else log "lockdown_on: uci unavailable"; fi
        FOLLOWUP_REPORT=1 ;;
      lockdown_off)
        log "command: lockdown_off"
        release_lockdown || log "lockdown_off: nothing to release"
        FOLLOWUP_REPORT=1 ;;
      gps_on)       log "command: gps_on"
                    at_cmd 'AT+QGPSCFG="autogps",1' 3 >/dev/null 2>&1
                    at_cmd 'AT+QGPS=1' 3 >/dev/null 2>&1
                    FOLLOWUP_REPORT=1 ;;
      *)            log "command: ignoring unknown verb" ;;
    esac
    PENDING_ACK="${PENDING_ACK:+$PENDING_ACK,}$_id"
  done
}

# --- Self-update ------------------------------------------------------------------------------
# Deliberately argument-free. The command channel carries a verb and nothing else, so there is no
# attacker-controlled string anywhere in this path: no URL, no version, no filename.

agent_path() {
  # Where this script is installed. Falls back to the packaged path.
  command -v brvg-agent 2>/dev/null || echo /usr/bin/brvg-agent
}

# Restore the kept-back copy of the previous agent. Used both by the rollback verb and
# automatically when a freshly installed agent fails its smoke check.
restore_agent() {
  _self=$(agent_path)
  if [ ! -s "$AGENT_BACKUP" ]; then
    log "rollback: no previous version kept ($1)"
    return 1
  fi
  cp "$AGENT_BACKUP" "$_self" && chmod 0755 "$_self" || { log "rollback: copy failed"; return 1; }
  log "rolled back to the previous agent ($1); restarting"
  [ -x /etc/init.d/brvg-agent ] && (sleep 2; /etc/init.d/brvg-agent restart) >/dev/null 2>&1 &
  return 0
}

self_update() {
  if ! command -v opkg >/dev/null 2>&1; then
    # The Pi hub installs from git/systemd, not opkg. Refuse rather than inventing a second,
    # unsigned update path on the platform that has no package signing.
    log "self_update: no opkg on this platform — skipping"
    return 0
  fi
  _self=$(agent_path)
  # Keep the running agent so a bad release is one command (or one failed smoke check) from undone.
  cp "$_self" "$AGENT_BACKUP" 2>/dev/null && chmod 0644 "$AGENT_BACKUP" 2>/dev/null

  # opkg verifies the feed's usign signature itself; --no-check-certificate and friends are
  # deliberately NOT used. A feed that fails its signature check simply does not install.
  if ! opkg update >/dev/null 2>&1; then
    log "self_update: feed refresh failed (offline, or the feed signature did not verify)"
    return 1
  fi
  if ! opkg upgrade brvg-agent >/dev/null 2>&1; then
    log "self_update: no upgrade applied (already current, or the package failed verification)"
    return 1
  fi

  # Smoke-check the thing we just installed BEFORE trusting it to keep the vehicle reporting.
  if ! "$_self" --version >/dev/null 2>&1; then
    log "self_update: the new agent failed its version check"
    restore_agent "failed smoke check"
    return 1
  fi
  log "self_update: installed $("$_self" --version 2>/dev/null); restarting"
  [ -x /etc/init.d/brvg-agent ] && (sleep 2; /etc/init.d/brvg-agent restart) >/dev/null 2>&1 &
  return 0
}

# --- Hub watchdog: fail open rather than leave the vessel silent ------------------------------
# Owner decision 2026-08-13. Under lockdown deny-all the hub is the ONLY path to the cloud. When
# the hub runs ON this router the failure domains coincide (a dead router means a dead gateway
# anyway) — but when it runs on a Pi/Docker/desktop it can die while the router routes happily,
# and the vessel goes silent with no remote way back in.
#
# So the ROUTER watches the hub: it is the enforcement point, and it is independently alive. After
# HUB_WATCH_FAILS consecutive failed health checks it RELEASES lockdown (availability beats
# lockdown purity) and reports it.
#
# The event names are chosen, not invented. `hub.offline` / `hub.online` match the worker's
# existing /offline|online/ → health rule INTENTIONALLY, so this needs no notifyCategories change.
# Checked against the real rules table, and the near-miss is instructive: `lockdown.released` also
# classifies — but only because "lockdown" happens to contain the substring "down" from the
# /…|down|fall/ rule. That is an accident, and it would evaporate the day someone tightens that
# rule to \bdown\b, silently sending a "your firewall was released" alert to NOBODY
# (categoryForEvent returns null → delivered to no one). Match on purpose, not by coincidence.
#
# It deliberately does NOT re-arm on recovery: a hub that restarts every few minutes would flap the
# firewall (each apply is a ~10 s reload). The released state is announced; re-arming is one tap in
# the app.

HUB_WATCH_FAILS_FILE="${BRVG_HUB_FAILS:-/tmp/brvg-hub-watch.fails}"
HUB_WATCH_RELEASED="${BRVG_HUB_RELEASED:-/tmp/brvg-hub-watch.released}"

# PURE: given the consecutive-failure count, the threshold, whether we already released, and
# whether the last probe succeeded — what should happen? Echoes: release | recover | none
watch_decide() {
  _healthy="$1"; _fails="$2"; _threshold="$3"; _released="$4"
  if [ "$_healthy" = "1" ]; then
    [ "$_released" = "1" ] && { echo recover; return 0; }
    echo none; return 0
  fi
  if [ "$_released" = "1" ]; then echo none; return 0; fi
  if [ "$_fails" -ge "$_threshold" ]; then echo release; return 0; fi
  echo none
}

# Remove every lockdown rule. The `brvg_lk_` NAME PREFIX is the shared contract between this and
# the app's SSH enforcement (src-tauri/src/lockdown.rs) — matching on the prefix, not on shared
# code, is what lets two languages manage the same rules without drifting.
release_lockdown() {
  command -v uci >/dev/null 2>&1 || return 1
  _i=0; _del=""
  while uci -q get "firewall.@rule[$_i]" >/dev/null 2>&1; do
    case "$(uci -q get "firewall.@rule[$_i].name")" in brvg_lk_*) _del="$_i $_del";; esac
    _i=$((_i + 1))
  done
  [ -n "$_del" ] || return 1          # nothing of ours applied — nothing to release
  for _j in $_del; do uci delete "firewall.@rule[$_j]" 2>/dev/null; done
  uci commit firewall
  /etc/init.d/firewall reload >/dev/null 2>&1 || true
  return 0
}

# Argument-free traffic lockdown: ONE catch-all REJECT rule (lan->wan), named under the shared
# brvg_lk_ prefix so the app's SSH enforcement, the watchdog, and release_lockdown all manage the
# same set. fw3/fw4 consult rules before zone forwardings, so this closes the forward chain while
# the hub (OUTPUT, not FORWARD) keeps reporting. Per-MAC allow rules stay app-applied — a verb
# carries no arguments, so it can only express the no-allows shape.
apply_lockdown() {
  command -v uci >/dev/null 2>&1 || return 1
  release_lockdown >/dev/null 2>&1 || true   # idempotent re-apply; hand-written rules survive
  _n=$(uci add firewall rule) || return 1
  uci set "firewall.$_n.name=brvg_lk_deny_all"
  uci set "firewall.$_n.src=lan"
  uci set "firewall.$_n.dest=wan"
  uci set "firewall.$_n.proto=any"
  uci set "firewall.$_n.target=REJECT"
  uci commit firewall
  /etc/init.d/firewall reload >/dev/null 2>&1 || true
  return 0
}

watch_hub() {
  # BANDWIDTH SAVER MODE FAILS CLOSED (owner, 2026-08-17): when lockdown exists to control
  # metered-SIM spend, a dead hub must NOT release it — silence until the connectivity-offline
  # alert is the accepted failure mode. The gate also skips the probe itself: no curl per tick.
  [ "${BANDWIDTH_SAVER:-0}" = "1" ] && return 0
  [ -n "${HUB_WATCH_URL:-}" ] || return 0
  _threshold="${HUB_WATCH_FAILS:-5}"
  _fails=$( (cat "$HUB_WATCH_FAILS_FILE" 2>/dev/null || echo 0) | tr -cd '0-9' )
  _fails=${_fails:-0}
  _released=0; [ -f "$HUB_WATCH_RELEASED" ] && _released=1

  if curl -fsS --max-time 8 "$HUB_WATCH_URL" >/dev/null 2>&1; then
    _healthy=1; _fails=0
  else
    _healthy=0; _fails=$((_fails + 1))
  fi
  echo "$_fails" > "$HUB_WATCH_FAILS_FILE"

  case "$(watch_decide "$_healthy" "$_fails" "$_threshold" "$_released")" in
    release)
      if release_lockdown; then
        : > "$HUB_WATCH_RELEASED"
        log "hub unreachable ${_fails}x — RELEASED lockdown so the vehicle keeps reporting"
        send_event "hub.offline" "released=1" || true
      else
        log "hub unreachable ${_fails}x — no lockdown rules to release"
        : > "$HUB_WATCH_RELEASED"   # don't retry the release every tick
        send_event "hub.offline" "released=0" || true
      fi
      ;;
    recover)
      rm -f "$HUB_WATCH_RELEASED"
      log "hub is answering again (network restrictions stay OFF until re-applied in the app)"
      send_event "hub.online" "rearmed=0" || true
      ;;
  esac
}

push_gps() {
  set -- $(collect_gps)
  [ -z "$1" ] && { log "no GPS fix this tick"; return 0; }
  _p="lat=$1&lon=$2"
  [ -n "$3" ] && _p="$_p&acc=$3"
  send_event "gps.measurement" "$_p"
}

# --- WAN usage accounting -----------------------------------------------------------------------
# The modem's own counter (AT+QGDCNT) tells us CELLULAR bytes, which is the right basis for plan
# alerts. It cannot tell us anything about the other WANs — so a week on marina Wi-Fi reads as
# "the counter didn't move", and we have no way to show the uplink or the roll-up actually saved
# anything. These per-interface counters close that.
#
# THE HARD PART IS RESETS, not reading. /sys counters are since-boot, so:
#   * a reboot sends them to 0 — a naive delta would be hugely negative;
#   * an interface bounce (modem reconnect, repeater rejoin) resets that one alone;
#   * our own `reset_data` verb deliberately zeroes the modem counter.
# So every delta is computed against a stored previous value, and a value that went DOWN is
# treated as a reset: report the NEW value as the delta (bytes since the reset) and carry on,
# rather than emitting a negative or a wrap-around-sized spike.

WAN_STATE_DIR="${BRVG_WAN_STATE:-/tmp/brvg-wan-state}"

# PURE: previous, current -> bytes to report. Echoes the delta.
#
# ⚠️ FIRST SIGHT REPORTS ZERO, and that is deliberate. It used to report `$_cur` — "everything so
# far" — which is harmless after a REBOOT (the kernel's /sys counters reset with the state dir, so
# `$_cur` is near zero) but badly wrong on a FRESH INSTALL: the state dir doesn't exist yet while
# the interface counters hold the router's ENTIRE UPTIME, potentially weeks of traffic. That whole
# figure landed in the billing cycle as a single tick — under the worker's 50 GB sanity cap, so
# nothing rejected it — and could trip the 80%/100% plan alerts, pushing a false "you have used
# your data plan" the moment a customer onboarded a router that had been running a while.
#
# Those bytes moved before we were watching, and probably before the cycle began, so they are not
# ours to attribute. Record the baseline, report nothing, start counting from the next tick.
wan_delta() {
  _prev="$1"; _cur="$2"
  [ -n "$_prev" ] || { echo 0; return 0; }                # first sight: baseline only, report none
  if [ "$_cur" -lt "$_prev" ] 2>/dev/null; then
    echo "$_cur"                                          # counter reset — count from zero
  else
    echo $(( _cur - _prev ))
  fi
}

# Which interface is carrying the default route right now: cellular | wired | wifi | none.
# This is what lets the cloud attribute bytes to a SOURCE rather than just an interface name.
wan_kind() {
  case "$1" in
    wwan*|wwan0|rmnet*|usb*) echo cellular ;;
    eth0|wan|eth0.2)         echo wired ;;
    apcli*|sta*|wlan*)       echo wifi ;;
    *)                       echo other ;;
  esac
}

# Read one interface's total bytes (rx+tx), or nothing if it has no counters.
wan_bytes() {
  _rx="/sys/class/net/$1/statistics/rx_bytes"
  _tx="/sys/class/net/$1/statistics/tx_bytes"
  [ -r "$_rx" ] && [ -r "$_tx" ] || return 1
  echo $(( $(cat "$_rx") + $(cat "$_tx") ))
}

# The interface currently holding the default route (empty if offline).
wan_active_iface() {
  ip route show default 2>/dev/null | awk '/^default/ { for (i=1;i<=NF;i++) if ($i=="dev") { print $(i+1); exit } }'
}

# Emit "&wan_cellular=<mb>&wan_wifi=<mb>..." for the interfaces that moved since last tick, plus
# the active source. Only NON-ZERO deltas are sent — a boat on Wi-Fi shouldn't pay for a cellular
# field that says 0 on every report.
collect_wan_usage() {
  mkdir -p "$WAN_STATE_DIR" 2>/dev/null
  _out=""
  _active=$(wan_active_iface)
  [ -n "$_active" ] && _out="&wanSrc=$(wan_kind "$_active")"
  for _path in /sys/class/net/*; do
    [ -e "$_path" ] || continue                           # no match ⇒ the glob stayed literal
    _if=$(basename "$_path")
    case "$_if" in lo|br-*|eth1) continue ;; esac         # loopback, the LAN bridge, LAN ports
    _kind=$(wan_kind "$_if")
    [ "$_kind" = "other" ] && continue                    # only WAN-side media
    _cur=$(wan_bytes "$_if") || continue
    _f="$WAN_STATE_DIR/$_if"
    _prev=$(cat "$_f" 2>/dev/null | tr -cd '0-9')
    _d=$(wan_delta "$_prev" "$_cur")
    echo "$_cur" > "$_f"
    # Report in KB: MB loses a slow trickle entirely, raw bytes waste URL length every tick.
    _kb=$(( _d / 1024 ))
    [ "$_kb" -gt 0 ] && _out="$_out&wanKb_${_kind}=${_kb}"
  done
  printf '%s' "$_out"
}

push_modem() {
  _m=$(collect_modem)
  [ -z "$_m" ] && return 0
  _sig=${_m%%|*}; _rest=${_m#*|}
  _carrier=${_rest%%|*}; _rest=${_rest#*|}
  _sim=${_rest%%|*}; _data=${_rest#*|}
  set -- $_sig
  _p="up=1"
  [ -n "$1" ] && _p="$_p&mode=$(urlencode_spaces "$1")"
  [ -n "$2" ] && _p="$_p&rssi=$2"
  [ -n "$3" ] && _p="$_p&rsrp=$3"
  [ -n "$4" ] && _p="$_p&sinr=$4"
  [ -n "$5" ] && _p="$_p&rsrq=$5"
  [ -n "$_carrier" ] && _p="$_p&carrier=$(urlencode_spaces "$_carrier")"
  [ -n "$_sim" ] && _p="$_p&sim=$_sim"
  # Plan-burn: the modem counts bytes since its last reset; the cloud turns that into the
  # 80%/100%-of-plan alerts, so report MB rather than raw bytes.
  if [ -n "$_data" ]; then
    set -- $_data
    if [ -n "$1" ] && [ -n "$2" ]; then
      _mb=$(( ($1 + $2) / 1048576 ))
      _p="$_p&dataMb=$_mb"
    fi
  fi
  # Report which agent version is running, plus per-source WAN usage. Staged rollout and rollback
  # are unmanageable without the version: you cannot decide who to update next if you cannot see
  # what is deployed.
  _p="$_p&av=$AGENT_VERSION$(collect_wan_usage)"
  send_event "modem.measurement" "$_p"
}

# --- Main loop ---------------------------------------------------------------------------------

main() {
  load_config
  log "starting (platform=$(detect_platform), gps every ${GPS_INTERVAL}s, modem every ${MODEM_INTERVAL}s)"
  # Small random start offset so a fleet doesn't tick in lockstep after a regional power event.
  sleep $(( $$ % 20 ))
  # An urgent webhook (alarm) pokes the drain immediately — aggregation must never delay one that
  # the CGI failed to deliver directly.
  [ "${HUB_LITE_ENABLED:-0}" = "1" ] && trap drain_relay USR1
  _elapsed=$MODEM_INTERVAL   # first loop sends both
  while :; do
    push_gps
    if [ "$_elapsed" -ge "$MODEM_INTERVAL" ]; then
      push_modem
      [ "${HUB_LITE_ENABLED:-0}" = "1" ] && drain_relay
      watch_hub
      _elapsed=0
    fi
    # A command ran during the sends above. Its effect is NOT in the report that carried it — that
    # payload was built first — so report again before sleeping. Cleared before sending, so a
    # command arriving in the follow-up is handled by the next pass rather than looping here; at
    # most one extra pair of sends per iteration, whatever the queue does.
    if [ "$FOLLOWUP_REPORT" = "1" ]; then
      FOLLOWUP_REPORT=0
      log "command follow-up: reporting the new state"
      push_gps
      push_modem
      _elapsed=0
    fi
    sleep "$GPS_INTERVAL"
    _elapsed=$(( _elapsed + GPS_INTERVAL ))
  done
}

# `--version` must work without a config: the self-update smoke check runs it on a freshly
# installed agent before that agent has ever been configured.
case "${1:-}" in
  --version|-v) echo "$AGENT_VERSION"; exit 0 ;;
esac

# Sourced by agent/test.sh with BRVG_AGENT_TEST set — parsers only, no loop, no config.
if [ -z "$BRVG_AGENT_TEST" ]; then
  main "$@"
fi
