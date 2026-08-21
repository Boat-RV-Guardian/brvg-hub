#!/bin/sh
# BRVG hub-lite — LAN MANAGEMENT door. Installed at /www/brvg/cgi-bin/mgmt and served by the same
# dedicated uhttpd instance as the webhook receiver (openwrt/etc/init.d/brvg-hub-lite).
#
# WHY THIS EXISTS (owner, 2026-08-21). The app had two ways to reach a router and neither was this
# one: it drove the VENDOR's API at 192.168.8.1 — which works only aboard, only on GL.iNet, and
# says nothing about the hub-lite — or it queued a verb in the cloud and waited for the next
# check-in. Aboard, with the box on the same LAN, the honest thing is to ask the hub-lite directly
# and get an answer now. That is this file. The cloud queue remains the door from anywhere.
#
# A hub does this with hub_server.rs and a per-member key set. A hub-lite is POSIX shell on a box
# with 416 KB of free overlay, so it gets the smaller version of the same idea:
#
#   GET  ?action=status              → the last telemetry this hub-lite composed, verbatim
#   GET  ?action=lockdown            → `uci show firewall` for our rules, for the app's parser
#   POST ?action=command&cmd=<verb>  → one ALLOWLISTED verb, run now (the cloud verb set exactly)
#   POST ?action=lockdown            → apply, with the per-MAC allow list the cloud verb cannot carry
#
# AUTH is the router's own management key (hubLiteKey.ts), presented as `x-brvg-key`. One secret,
# one router, one privilege level — the app fetches it from the worker over its normal
# Firebase-authed channel and the user never sees it.
#
# ⚠️ NOT A TUNNEL, for the same reason the cloud queue is not one: `command` takes a verb off a
# fixed list and nothing else, and `lockdown` takes booleans and hardware addresses that are
# validated before they reach a uci argument. There is no path here by which a caller's string
# becomes a command. This REPLACES the app SSH-ing a generated script in as root, which is why the
# validation is the interesting part of the file rather than an afterthought.

CONF="${BRVG_HUB_LITE_CONF:-/etc/brvg-hub-lite.conf}"
BIN="${BRVG_HUB_LITE_BIN:-/usr/bin/brvg-hub-lite}"

reply() { printf 'Status: %s\r\nContent-Type: application/json\r\n\r\n%s\r\n' "$1" "$2"; }
die()   { reply "$1" "{\"error\":\"$2\"}"; exit 0; }

# shellcheck disable=SC1090
[ -f "$CONF" ] && . "$CONF"

# No key on the box yet ⇒ the door is SHUT, not open. A hub-lite that has never reached the worker
# has no way to tell a member from a stranger on the marina Wi-Fi, and "fail open" is the wrong
# answer to that question.
[ -n "${MGMT_KEY:-}" ] || die 503 "this hub-lite has no management key yet"
[ "${HTTP_X_BRVG_KEY:-}" = "$MGMT_KEY" ] || die 401 "unauthorized"

# ---- request parsing -------------------------------------------------------------------------
# Only the three names below are ever read out of the query string, and each is filtered to the
# characters its own grammar allows before it is used for anything.
action=""; cmd=""; catch=""
IFS='&'
for kv in ${QUERY_STRING:-}; do
  case "$kv" in
    action=*) action="${kv#action=}" ;;
    cmd=*)    cmd="${kv#cmd=}" ;;
    catch=*)  catch="${kv#catch=}" ;;
  esac
done
unset IFS
action=$(printf '%s' "$action" | tr -cd 'a-z' | cut -c1-16)
cmd=$(printf '%s' "$cmd" | tr -cd 'a-z_' | cut -c1-32)
catch=$(printf '%s' "$catch" | tr -cd '01' | cut -c1-1)

method="${REQUEST_METHOD:-GET}"

# Source the hub-lite for its functions ONLY — BRVG_HUB_LITE_TEST is what stops main() from
# running. The same one-shot pattern the webhook receiver uses for the flood close.
load_lib() {
  # shellcheck disable=SC1090
  BRVG_HUB_LITE_TEST=1 . "$BIN" 2>/dev/null || die 500 "hub-lite not installed"
}

case "$method:$action" in
  # ---- status ---------------------------------------------------------------------------------
  # Served from the state file the reporting path writes, so this answers instantly, never touches
  # the AT port, and can never disagree with what the cloud was told.
  GET:status)
    STATE="${BRVG_HUB_LITE_STATE:-/tmp/brvg-hub-lite.state}"
    if [ -r "$STATE" ]; then
      printf 'Status: 200\r\nContent-Type: application/json\r\n\r\n'
      cat "$STATE"
    else
      # Reachable and honest: the hub-lite is up, it just has not composed a report yet (a fresh
      # boot). Saying so beats a 404 the app would have to guess the meaning of.
      reply 200 "{\"v\":1,\"event\":\"none\",\"pending\":true}"
    fi
    ;;

  # ---- lockdown state -------------------------------------------------------------------------
  GET:lockdown)
    load_lib
    _raw=$(lockdown_show | sed 's/\\/\\\\/g; s/"/\\"/g' | awk '{ printf "%s%s", (NR>1 ? "\\n" : ""), $0 }')
    reply 200 "{\"raw\":\"$_raw\"}"
    ;;

  # ---- run one verb ---------------------------------------------------------------------------
  # The verb set is NOT re-declared here. run_commands owns the allowlist, and anything it does not
  # recognise it drops — so this endpoint and the cloud queue can never diverge about what a hub-lite
  # will do. A verb this hub-lite does not know is still a 200: it was accepted and dropped, exactly
  # as the cloud path behaves.
  POST:command)
    [ -n "$cmd" ] || die 400 "cmd required"
    load_lib
    run_commands "lan:$cmd" >/dev/null 2>&1
    reply 200 "{\"status\":\"ok\",\"ran\":\"$cmd\",\"door\":\"lan\"}"
    ;;

  # ---- apply lockdown, with the allow list -----------------------------------------------------
  POST:lockdown)
    [ -n "$catch" ] || die 400 "catch=0|1 required"
    # MACs arrive in the BODY, one per line — not the query string: a query string lands in server
    # logs, and a device inventory is not something to leave lying in one.
    _macs=$(head -c 4096 | tr -cd '0-9A-Fa-f:\n' | tr '\n' ' ')
    load_lib
    # shellcheck disable=SC2086
    lockdown_apply_rules "$catch" $_macs
    case $? in
      0) : ;;
      2) die 400 "an approved-device entry is not a hardware address" ;;
      *) die 500 "uci is unavailable on this router" ;;
    esac
    _raw=$(lockdown_show | sed 's/\\/\\\\/g; s/"/\\"/g' | awk '{ printf "%s%s", (NR>1 ? "\\n" : ""), $0 }')
    reply 200 "{\"status\":\"ok\",\"raw\":\"$_raw\"}"
    ;;

  *)
    die 404 "no such action"
    ;;
esac
exit 0
