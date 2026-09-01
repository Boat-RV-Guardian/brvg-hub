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
# Two kinds of verb live here, with different auth:
#   * READ + liveness (ping, status, linktap/state) — unauthenticated, like the daemon's. They are
#     how the app discovers what this box is before it holds any key.
#   * VALVE COMMANDS (linktap/valve) — the router's management key, same Bearer the mgmt door takes.
#     Opening water is not something an unauthenticated LAN peer may do.

CONF="${BRVG_HUB_LITE_CONF:-/etc/brvg-hub-lite.conf}"
BIN="${BRVG_HUB_LITE_BIN:-/usr/bin/brvg-hub-lite}"
LT_STATE="${BRVG_LT_STATE_DIR:-/tmp}"

# shellcheck disable=SC1090
[ -r "$CONF" ] && . "$CONF"

reply() { printf 'Content-Type: application/json\r\nStatus: %s\r\n\r\n%s\r\n' "$1" "$2"; exit 0; }

# The management key, presented as `Authorization: Bearer` — the same secret and the same header the
# mgmt door takes, because one router has one privilege level.
require_key() {
  _want="${HUB_LITE_KEY:-}"
  [ -n "$_want" ] || reply 503 '{"error":"this hub-lite has no management key yet"}'
  case "${HTTP_AUTHORIZATION:-}" in
    "Bearer $_want") : ;;
    *) reply 401 '{"error":"a management key is required"}' ;;
  esac
}

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

  # ---- linktap/valve --------------------------------------------------------------------------
  # The command door. Without it #85's `capabilities: ["linktap"]` would be a lie the app acts on:
  # it would route an open here, get a 404, and — since the app no longer falls back to the gateway
  # (owner ruling 2026-08-31) — simply refuse. A router vessel would lose valve control entirely.
  #
  # Body is the daemon's ValveReq: {devId, action, durationSecs?, volumeCapL?, mode?}. Parsed with
  # grep rather than a JSON library because there is no JSON library here; the fields are all
  # numbers and short enums, and each is validated before use.
  /linktap/valve)
    require_key
    _body=$(dd bs=1 count="${CONTENT_LENGTH:-0}" 2>/dev/null)
    _dev=$(printf '%s' "$_body" | grep -o '"devId"[[:space:]]*:[[:space:]]*"[^"]*"' | sed 's/.*"\([^"]*\)"$/\1/' | tr -cd 'A-Za-z0-9' | cut -c1-16)
    _action=$(printf '%s' "$_body" | grep -o '"action"[[:space:]]*:[[:space:]]*"[^"]*"' | sed 's/.*"\([^"]*\)"$/\1/')
    _mode=$(printf '%s' "$_body" | grep -o '"mode"[[:space:]]*:[[:space:]]*"[^"]*"' | sed 's/.*"\([^"]*\)"$/\1/')
    _secs=$(printf '%s' "$_body" | grep -o '"durationSecs"[[:space:]]*:[[:space:]]*[0-9]*' | grep -o '[0-9]*$')
    _capl=$(printf '%s' "$_body" | grep -o '"volumeCapL"[[:space:]]*:[[:space:]]*[0-9.]*' | grep -o '[0-9.]*$')
    [ -n "$_dev" ] || reply 422 '{"error":"devId is required"}'
    case " $(printf '%s' "${LINKTAP_DEV_IDS:-}" | tr ',' ' ') " in
      *" $_dev "*) : ;;
      *) reply 404 '{"error":"that valve is not configured on this hub"}' ;;
    esac
    [ -n "${LINKTAP_HOST:-}" ] && [ -n "${LINKTAP_GW_ID:-}" ] || reply 409 '{"error":"no LinkTap gateway is configured"}'

    _mode="${_mode:-normal}"
    case "$_action" in
      close)
        BRVG_HUB_LITE_TEST=1 . "$BIN" 2>/dev/null || reply 500 '{"error":"hub-lite not installed"}'
        curl -fsS --max-time 5 -X POST -H 'Content-Type: application/json' \
          -d "$(linktap_stop_body "$LINKTAP_GW_ID" "$_dev")" "http://${LINKTAP_HOST}/api.shtml" >/dev/null 2>&1 \
          || reply 502 '{"error":"the gateway did not accept that command"}'
        printf 'state=watering\nstarted=%s\nstop=manual\nmode=%s\n' "$(date +%s)" "$_mode" > "${LT_STATE}/${_dev}"
        reply 200 '{"ok":true}'
        ;;
      open)
        [ -n "$_secs" ] && [ "$_secs" -gt 0 ] 2>/dev/null || reply 422 '{"error":"durationSecs is required to open a valve"}'
        # ⚠️ A WASHDOWN IS TIME-ONLY (owner spec 2026-07-30, re-ratified twice). A volumeCapL sent
        # alongside mode=washdown is a caller bug; honouring it would re-create the external cap
        # that cut two-hour hose runs at ~26 gal. Refuse rather than silently drop either side —
        # the daemon's do_valve refuses the identical shape.
        if [ "$_mode" = "washdown" ] && [ -n "$_capl" ]; then
          reply 422 '{"error":"a washdown is time-limited only - do not send volumeCapL with mode=washdown"}'
        fi
        [ "$_mode" = "washdown" ] && _capl=0
        _capl="${_capl:-0}"
        BRVG_HUB_LITE_TEST=1 . "$BIN" 2>/dev/null || reply 500 '{"error":"hub-lite not installed"}'
        # The cap must be expressed in the GATEWAY's unit, exactly as the daemon does: guessing
        # litres under-reports a gallon-configured cap by 3.79x, and the software cutoff compares
        # against that number.
        _unit=$(curl -fsS --max-time 5 -X POST -H 'Content-Type: application/json' \
          -d "{\"cmd\":16,\"gw_id\":\"${LINKTAP_GW_ID}\"}" "http://${LINKTAP_HOST}/api.shtml" 2>/dev/null \
          | grep -o '"vol_unit":"[^"]*"' | cut -d'"' -f4)
        _capgw=$(awk -v c="$_capl" -v u="${_unit:-L}" 'BEGIN{printf "%.2f", (u=="gal") ? c/3.785411784 : c}')
        curl -fsS --max-time 5 -X POST -H 'Content-Type: application/json' \
          -d "$(lt_start_body "$LINKTAP_GW_ID" "$_dev" "$_secs" "$_capgw")" "http://${LINKTAP_HOST}/api.shtml" >/dev/null 2>&1 \
          || reply 502 '{"error":"the gateway did not accept that command"}'
        # Record it as OURS, with THIS RUN's targets — or the next tick would meet a running valve
        # and adopt it into a Normal Run on the profile's cap, which for a washdown is exactly the
        # cap that must not exist.
        printf 'state=watering\nstarted=%s\nstop=\nmode=%s\ndur=%s\ncap=%s\n' \
          "$(date +%s)" "$_mode" "$_secs" "$_capl" > "${LT_STATE}/${_dev}"
        reply 200 '{"ok":true}'
        ;;
      *) reply 422 '{"error":"unknown action - expected open or close"}' ;;
    esac
    ;;

  *)
    reply 404 '{"error":"no such hub endpoint"}'
    ;;
esac
