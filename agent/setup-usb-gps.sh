#!/bin/sh
# Make a USB GPS dongle visible to the BRVG agent on an OpenWrt router (GL.iNet and friends).
#
# ⚠️ REWRITTEN 2026-08-16. This script used to install ser2net and publish the receiver as a TCP
# NMEA stream for the app to poll. That was the wrong design and is gone. A USB GPS is a SERIAL
# device on the router, and the agent already reads it: `GPS_SOURCE=auto` tries the modem's GNSS,
# then a USB NMEA device, then gpsd, and pushes fixes outbound. The TCP path added a service to
# install, a port to open, a desktop-app dependency and a second reader competing with the agent
# for the same device — to arrive at a position the agent was already sending, and only while
# somebody was aboard with the app open.
#
# So all that is actually needed is the KERNEL MODULES that make the dongle appear as /dev/ttyACM0.
# That is what this does, plus telling you whether the agent can now see it.
#
# Not run from the package's postinst: the dongle is usually plugged in after the agent is
# installed, and opkg needs working internet at that moment. Idempotent — plug it in, run it, done.
#
# Usage:  sh setup-usb-gps.sh [--device /dev/ttyACM0] [--port 10110] [--baud 9600]
#         sh setup-usb-gps.sh --status
#         sh setup-usb-gps.sh --remove

set -e

PORT=10110
BAUD=9600
DEVICE=""
ACTION="install"

while [ $# -gt 0 ]; do
  case "$1" in
    --device) DEVICE="$2"; shift 2 ;;
    --port)   PORT="$2"; shift 2 ;;
    --baud)   BAUD="$2"; shift 2 ;;
    --status) ACTION="status"; shift ;;
    --remove) ACTION="remove"; shift ;;
    -h|--help) sed -n '2,22p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

log()  { echo "[usb-gps] $*"; }
die()  { echo "[usb-gps] ERROR: $*" >&2; exit 1; }

# The agent's AT port must never be mistaken for the receiver — reading it steals the modem's
# control channel. Mirrors brvg-agent.sh's find_nmea_device().
AT_PORT=$(sed -n 's/^[[:space:]]*AT_PORT=["'\'']\{0,1\}\([^"'\'' ]*\).*/\1/p' /etc/brvg-agent.conf 2>/dev/null | tail -1)
[ -n "$AT_PORT" ] || AT_PORT=/dev/ttyUSB2

find_device() {
  for d in /dev/ttyACM0 /dev/ttyACM1 /dev/ttyUSB3 /dev/ttyUSB4; do
    [ "$d" = "$AT_PORT" ] && continue
    [ -c "$d" ] && { echo "$d"; return 0; }
  done
  return 1
}

lan_ip() {
  uci get network.lan.ipaddr 2>/dev/null && return 0
  ip -4 addr show br-lan 2>/dev/null | sed -n 's|.*inet \([0-9.]*\)/.*|\1|p' | head -1
}

# ── status ─────────────────────────────────────────────────────────────────────────────────────
if [ "$ACTION" = "status" ]; then
  d=$(find_device) && log "receiver: $d" || log "receiver: NONE FOUND"
  grep -sE '^[[:space:]]*GPS_SOURCE=' /etc/brvg-agent.conf 2>/dev/null || log "agent GPS_SOURCE: unset (defaults to auto)"
  exit 0
fi

# ── install ────────────────────────────────────────────────────────────────────────────────────
[ "$(id -u)" = "0" ] || die "run as root"
command -v opkg >/dev/null 2>&1 || die "this script targets OpenWrt (no opkg found)"

# Kernel modules, only if no receiver is visible yet. A dongle that already enumerated needs
# nothing installed, and opkg update on a metered link is not free.
if [ -z "$DEVICE" ] && ! find_device >/dev/null 2>&1; then
  log "no serial receiver visible — installing USB serial kernel modules"
  opkg update >/dev/null 2>&1 || die "opkg update failed — is the router online?"
  # CDC-ACM covers u-blox (VK-162 / BU-353S4 class); the others cover FTDI/Prolific receivers.
  for m in kmod-usb-acm kmod-usb-serial-ftdi kmod-usb-serial-pl2303; do
    opkg install "$m" >/dev/null 2>&1 || log "note: $m unavailable or already present"
  done
  sleep 2   # give the kernel a moment to enumerate the device node
fi

[ -n "$DEVICE" ] || DEVICE=$(find_device) || die "no USB GPS receiver found. Plug it in, then re-run. Checked /dev/ttyACM0-1 and /dev/ttyUSB3-4 (skipping the modem's AT port $AT_PORT)."
[ -c "$DEVICE" ] || die "$DEVICE is not a character device"
log "receiver: $DEVICE"

# Prove it is actually a GPS and not just a serial port that enumerated. A config that "works" but
# yields no sentences is the failure people waste an evening on.
if command -v timeout >/dev/null 2>&1; then
  sample=$(timeout 8 head -c 2048 "$DEVICE" 2>/dev/null | tr -dc '\11\12\15\40-\176' | grep -m1 -E '^\$G[PNLA]' || true)
else
  sample=$(head -c 2048 "$DEVICE" 2>/dev/null | tr -dc '\11\12\15\40-\176' | grep -m1 -E '^\$G[PNLA]' || true)
fi
if [ -n "$sample" ]; then
  log "OK — receiving NMEA: $sample"
else
  log "WARNING: the device is there but sent no NMEA in 8s. It may still be acquiring (a cold"
  log "         start can take minutes with a clear sky view), or it is not a GPS receiver."
fi

# The agent reads it on its own. Nothing to serve, nothing to open.
if [ -f /etc/brvg-agent.conf ] && grep -qE '^[[:space:]]*GPS_SOURCE=["'\'']?(at|off)' /etc/brvg-agent.conf 2>/dev/null; then
  log ""
  log "NOTE: the agent is set to GPS_SOURCE=at, so it will use the modem's GNSS and ignore this"
  log "      receiver. Set GPS_SOURCE=auto (or nmea) in /etc/brvg-agent.conf to use the dongle."
fi

log ""
log "Done. The agent picks the receiver up on its next GPS tick — GPS_SOURCE=auto tries the modem"
log "first, then this device. Nothing else to install: position reaches the app through the agent's"
log "normal reports, so it works with nobody aboard."
