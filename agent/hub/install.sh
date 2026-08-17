#!/bin/sh
# BRVG Hub bootstrap — turns a fresh Raspberry Pi OS install into the onsite proxy box
# (owner direction 2026-08-07: "a small box that can act as a hub and run the same proxy/polling
# that the GL.iNet could run… Z-Wave and Bluetooth capabilities").
#
# It installs the SAME agent the routers run — one codebase, two homes — plus the optional radio
# bits the router can't provide. Deliberately idempotent: re-running it upgrades in place.
#
#   sudo sh agent/hub/install.sh
#
# What it does NOT do: write /etc/brvg-agent.conf. That file carries this device's token and is
# written by the app at enrollment (or by hand from the panel's manual steps).

set -eu

BIN=/usr/local/bin/brvg-agent
UNIT=/etc/systemd/system/brvg-agent.service
SRC="$(cd "$(dirname "$0")/.." && pwd)"

[ "$(id -u)" = "0" ] || { echo "run me with sudo" >&2; exit 1; }

echo "==> agent"
install -m 0755 "$SRC/brvg-agent.sh" "$BIN"
install -m 0644 "$SRC/systemd/brvg-agent.service" "$UNIT"

echo "==> dependencies"
# gpsd covers a USB GPS receiver; the agent falls back to reading the NMEA device directly.
# bluez is preinstalled on Raspberry Pi OS; named here so a bare Debian image works too.
if command -v apt-get >/dev/null 2>&1; then
  DEBIAN_FRONTEND=noninteractive apt-get update -qq
  DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends curl gpsd gpsd-clients bluez >/dev/null
fi

echo "==> radios"
# Z-Wave: a USB stick (Zooz ZST39 / Aeotec) appears as /dev/ttyACM* or /dev/ttyUSB*. We do NOT
# install a Z-Wave stack here — that is the next increment, and Z-Wave JS wants Node. This just
# reports what is plugged in so the bench spike starts from facts.
for dev in /dev/ttyACM* /dev/ttyUSB*; do
  [ -e "$dev" ] || continue
  echo "    serial device present: $dev"
done
if command -v hciconfig >/dev/null 2>&1 && hciconfig 2>/dev/null | grep -q hci; then
  echo "    bluetooth adapter present"
fi

echo "==> service"
systemctl daemon-reload
systemctl enable brvg-agent >/dev/null 2>&1 || true
if [ -f /etc/brvg-agent.conf ]; then
  chmod 600 /etc/brvg-agent.conf
  systemctl restart brvg-agent
  echo "    started"
else
  echo "    NOT started — /etc/brvg-agent.conf is missing."
  echo "    Enroll this hub from the app (Router / Connectivity → Connect to cloud), or copy a"
  echo "    configuration to /etc/brvg-agent.conf and run: systemctl start brvg-agent"
fi

echo "==> done"
