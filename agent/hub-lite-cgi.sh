#!/bin/sh
# BRVG relay — CGI receiver (ONSITE.md "The one wire contract", relay tier). Installed at /www/brvg/cgi-bin/report and
# served by a dedicated uhttpd instance on the LAN. Pure POSIX shell: Lua is NOT stock firmware
# (owner correction 2026-08-13), and the X750's 416 KB of free overlay rules out everything else.
#
# The Shellys' webhooks are re-registered against this URL instead of the cloud:
#   http://<router-lan-ip>:8181/cgi-bin/report?device=<id>&event=<ev>&<values...>
# so a sensor's report never leaves the LAN — which is what lets lockdown's forward chain be
# deny-all with no allow rules, and means the sensors need neither DNS nor a correct clock.
#
# TWO PATHS, decided by the event class:
#   * ALARMS (anything that is not *.measurement / *.change — the same line events.ts draws) are
#     sent to the cloud IMMEDIATELY as a single-item batch. Aggregation never delays an alarm.
#     Only if that send fails is the alarm spooled, so the next drain retries it — never both.
#   * telemetry is appended to the spool and rides the next roll-up.
#
# The spool line format is the relay's internal contract with brvg-agent.sh:
#   <epoch>\t<device>\t<event>\t<raw-urlencoded-params>
# Appends of one short line to tmpfs are effectively atomic (< PIPE_BUF); there is no locking.

CONF="${BRVG_AGENT_CONF:-/etc/brvg-agent.conf}"
SPOOL="${BRVG_RELAY_SPOOL:-/tmp/brvg-relay.spool}"

# A sleepy flood sensor is awake on borrowed battery — answer first, work after.
printf 'Content-Type: text/plain\r\n\r\nok\r\n'

Q="${QUERY_STRING:-}"
device=""; event=""; rest=""
IFS='&'
for kv in $Q; do
  case "$kv" in
    device=*) device="${kv#device=}" ;;
    event=*)  event="${kv#event=}" ;;
    '') ;;
    *) rest="${rest:+$rest&}$kv" ;;
  esac
done
unset IFS

# Identity fields are constrained hard — they end up in storage keys and JSON. Values stay raw
# urlencoded here; the drain's parser owns decoding + escaping.
device=$(printf '%s' "$device" | tr -cd 'A-Za-z0-9_.:-' | cut -c1-64)
event=$(printf '%s' "$event" | tr -cd 'A-Za-z0-9_.-' | cut -c1-64)
[ -n "$device" ] && [ -n "$event" ] || exit 0

# Same telemetry line the worker draws (events.ts isTelemetry): measurements and changes batch;
# everything else — alarms, alarm-clears, button presses — goes NOW.
case "$event" in
  *.measurement|*.change) urgent=0 ;;
  *) urgent=1 ;;
esac

spool_line() {
  printf '%s\t%s\t%s\t%s\n' "$(date +%s)" "$device" "$event" "$rest" >> "$SPOOL"
}

if [ "$urgent" = "1" ] && [ -f "$CONF" ]; then
  # shellcheck disable=SC1090
  . "$CONF"
  # LOCAL FLOOD -> VALVE SHUTOFF, before the cloud send (hub-lite capability #1, owner
  # 2026-08-19): the close must not wait on the WAN — with the LinkTap cloud gone this is the
  # only automated close when the uplink is down. Deliberately independent of DEVICE_TOKEN:
  # closing a valve on the LAN needs no cloud credential. One-shot sourcing of the agent, same
  # pattern as spool_to_items below, so the classifier and the close live in ONE place.
  if [ -n "${LINKTAP_HOST:-}" ]; then
    LINKTAP_HOST="$LINKTAP_HOST" LINKTAP_GW_ID="${LINKTAP_GW_ID:-}" LINKTAP_DEV_IDS="${LINKTAP_DEV_IDS:-}" \
    BRVG_RELAY_SPOOL="$SPOOL" BRVG_AGENT_TEST=1 \
      sh -c ". \"${BRVG_AGENT_BIN:-/usr/bin/brvg-agent}\"; is_flood_shutoff \"$event\" && linktap_flood_close" 2>/dev/null || true
  fi
  if [ -n "${DEVICE_TOKEN:-}" ] && [ -n "${VID:-}" ] && [ -n "${DEVICE_ID:-}" ]; then
    # Single-item batch, NO seq: this path never retries (failure falls through to the spool,
    # which has its own seq), so idempotency isn't needed and must not be claimed.
    # ONE decoder: reuse the agent's own spool→items builder rather than carrying a copy of the
    # urldecode/escape awk here. Sourcing with BRVG_AGENT_TEST=1 defines functions only — no loop.
    _items=$(printf '0\t%s\t%s\t%s\n' "$device" "$event" "$rest" \
      | BRVG_AGENT_TEST=1 sh -c ". \"${BRVG_AGENT_BIN:-/usr/bin/brvg-agent}\"; spool_to_items" 2>/dev/null)
    case "$_items" in "["*"]") : ;; *) _items="" ;; esac
    _body='{"v":1,"kind":"delta","items":'${_items:-[]}',"ok":[]}'
    if [ -n "$_items" ] && curl -fsS --max-time 10 -X POST \
        -H 'Content-Type: application/json' -d "$_body" \
        "${WORKER_URL:-https://api.boatrvguardian.com}/api/agent/batch?vid=${VID}&device=${DEVICE_ID}&t=${DEVICE_TOKEN}" \
        >/dev/null 2>&1; then
      exit 0   # delivered — do NOT also spool, or the next drain double-reports the alarm
    fi
  fi
fi

spool_line
exit 0
