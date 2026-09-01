#!/bin/sh
# BRVG hub-lite — the /api/hub/* door. Installed at /www/brvg/api/hub and served by the same uhttpd
# instance as the webhook receiver, on port 8722.
#
# ONE CONTRACT (owner ruling 2026-08-31): "the hub-lite should move to 8722, keep one contract."
# The app used to speak two dialects — /api/hub/<verb> to the Rust daemon on 8722, and
# ?action=<verb> to a hub-lite on 8181. Same questions, two shapes, two sets of bugs. This file is
# the hub-lite answering the DAEMON's contract, so the app has one hub client and one mental model.
#
# uhttpd is started with `-x /api`, and resolves the longest existing file path before handing the
# remainder over as PATH_INFO. So this ONE script at /www/brvg/api/hub receives every verb beneath
# it: /api/hub/ping → PATH_INFO=/ping.
#
# Scope is deliberately the READ side plus liveness. Valve COMMANDS stay on the existing management
# door with its own auth; this file opens no valve and changes no configuration, which is what lets
# ping stay unauthenticated.

CONF="${BRVG_HUB_LITE_CONF:-/etc/brvg-hub-lite.conf}"
BIN="${BRVG_HUB_LITE_BIN:-/usr/bin/brvg-hub-lite}"
LT_STATE="${BRVG_LT_STATE_DIR:-/tmp}"

# shellcheck disable=SC1090
[ -r "$CONF" ] && . "$CONF"

reply() { printf 'Content-Type: application/json\r\nStatus: %s\r\n\r\n%s\r\n' "$1" "$2"; exit 0; }

# JSON string escaping, such as shell can manage: the fields below are ids, versions and numbers
# that our own code wrote, but a quote or backslash reaching a client as malformed JSON is a bug
# the client cannot recover from, so it is cheaper to escape than to trust.
esc() { printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e 's/[[:cntrl:]]//g'; }

verb="${PATH_INFO:-/ping}"

case "$verb" in
  # ---- ping -----------------------------------------------------------------------------------
  # UNAUTHENTICATED, exactly like the daemon's: it is how the app discovers that the thing at this
  # address is a BRVG hub at all, before it has any key to present. It reveals liveness and a
  # version, and nothing a scanner could not learn by looking at the open port.
  #
  # `lite: true` is the honest part. A hub-lite is not a full hub — no washdown, no tank fill, no
  # relay socket, no ledger — and a client that assumed otherwise would offer controls that cannot
  # work. Advertise what is here, not what the contract allows.
  /ping)
    _ver=$(sed -n 's/^VERSION=//p' "$BIN" 2>/dev/null | head -1 | tr -cd '0-9.')
    reply 200 "{\"ok\":true,\"lite\":true,\"version\":\"$(esc "${_ver:-0}")\",\"registered\":$([ -n "${VEHICLE_ID:-}" ] && echo true || echo false)}"
    ;;

  # ---- status ---------------------------------------------------------------------------------
  /status)
    _caps='[]'
    [ -n "${LINKTAP_HOST:-}" ] && [ -n "${LINKTAP_DEV_IDS:-}" ] && _caps='["linktap"]'
    reply 200 "{\"lite\":true,\"vid\":\"$(esc "${VEHICLE_ID:-}")\",\"capabilities\":${_caps},\"shellyIngestArmed\":$([ -n "${VEHICLE_KEY:-}${DEVICE_TOKEN:-}" ] && echo true || echo false)}"
    ;;

  # ---- linktap/state --------------------------------------------------------------------------
  # The read the app uses instead of polling the gateway itself. Same field names the daemon's
  # measurement carries, so mapHubValveReading needs no hub-lite special case.
  #
  # NO LONG POLL HERE, deliberately. The daemon holds the request on a watch channel; this is CGI,
  # where holding a request holds a uhttpd worker AND a shell process on a box with 416 KB of free
  # overlay. `wait` is accepted and ignored: answering immediately is the honest degradation, and
  # the client's own loop still works — it simply polls at its own cadence instead of being pushed.
  # A hub-lite is allowed to be slower; it is not allowed to be a different shape.
  /linktap/state)
    _valves=''
    for _d in $(printf '%s' "${LINKTAP_DEV_IDS:-}" | tr ',' ' '); do
      _d=$(printf '%s' "$_d" | tr -cd 'A-Za-z0-9' | cut -c1-16)
      [ -n "$_d" ] || continue
      _f="${LT_STATE}/brvg-lt-${_d}.state"
      # state=idle|watering started=<epoch> stop=... — written by linktap_tick.
      _w=0; _vol=0; _remain=0
      if [ -r "$_f" ]; then
        # shellcheck disable=SC2046
        set -- $(cat "$_f" 2>/dev/null)
        for _kv in "$@"; do
          case "$_kv" in
            state=watering) _w=1 ;;
            volL=*) _vol="${_kv#volL=}" ;;
            remain=*) _remain="${_kv#remain=}" ;;
          esac
        done
      fi
      _valves="${_valves:+$_valves,}{\"devId\":\"$(esc "$_d")\",\"watering\":\"${_w}\",\"vol_l\":\"$(esc "$_vol")\",\"remain_s\":\"$(esc "$_remain")\"}"
    done
    # `rev` is a timestamp rather than a change counter: a caller comparing it still learns whether
    # anything is newer, and CGI has nowhere to keep a counter across invocations.
    reply 200 "{\"rev\":$(date +%s),\"lite\":true,\"valves\":[${_valves}]}"
    ;;

  *)
    reply 404 '{"error":"no such hub endpoint"}'
    ;;
esac
