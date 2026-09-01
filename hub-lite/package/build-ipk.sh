#!/bin/sh
# Build an OpenWrt .ipk of the phone-home hub-lite.
#
# WHY this exists (ONSITE.md "Router phone-home"): the app installs the hub-lite over SSH today, which works
# but is desktop-only — the SSH client is deliberately kept out of the Android build. GL.iNet's 4.x
# RPC surface has `plugins.install_package`, so a package installs with **no SSH at all**, from any
# platform including a phone. It is also the natural home for the signed-artifact bar: opkg feeds
# are signed (usign), so the signature check happens on-device, by the package manager, rather than
# being something we invent.
#
# This script produces the package. SIGNING and PUBLISHING it — the signed opkg feed a router's
# self_update pulls from — is done by .github/workflows/hub-lite-feed.yml on a hub-lite-v* tag; see
# hub-lite/package/README.md for the feed URL and the one owner step (the signing secret).
#
# ipk format: an ar archive of debian-binary + control.tar.gz + data.tar.gz. No OpenWrt SDK, no
# cross-compiler: the hub-lite is POSIX shell, so the "build" is packaging.

set -eu

SRC="$(cd "$(dirname "$0")/.." && pwd)"
# The version comes from the hub-lite itself. Two sources of truth would mean the feed advertising a
# version the running hub-lite doesn't report — which is precisely the signal a staged rollout uses
# to decide who still needs updating.
VERSION="${VERSION:-$(sed -n 's/^HUB_LITE_VERSION="\([^"]*\)".*/\1/p' "$SRC/brvg-hub-lite.sh")}"
[ -n "$VERSION" ] || { echo "could not read HUB_LITE_VERSION from brvg-hub-lite.sh" >&2; exit 1; }
OUT="${OUT:-$(pwd)/dist}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

PKG="brvg-hub-lite"
ARCH="all"                 # pure shell — architecture-independent

mkdir -p "$WORK/data/usr/bin" "$WORK/data/usr/libexec/brvg-hub-lite" "$WORK/data/etc/init.d" "$WORK/data/www/brvg/cgi-bin" "$WORK/data/www/brvg/api" "$WORK/control" "$OUT"

install -m 0755 "$SRC/brvg-hub-lite.sh" "$WORK/data/usr/bin/brvg-hub-lite"
# The signed-feed provisioner: writes the trust anchor and the customfeeds line self_update needs.
# Shipped as a payload file (not just baked into postinst) so a re-run — or the app's over-SSH
# installer, which embeds the same content — has one canonical script to call.
install -m 0755 "$SRC/package/feed-setup.sh" "$WORK/data/usr/libexec/brvg-hub-lite/feed-setup"
install -m 0755 "$SRC/openwrt/etc/init.d/brvg-hub-lite" "$WORK/data/etc/init.d/brvg-hub-lite"
# USB GPS → TCP NMEA setup. SHIPPED but not run at install: the dongle is normally plugged in later,
# and it needs working internet for opkg. `brvg-setup-usb-gps` on the router does the whole job.
install -m 0755 "$SRC/setup-usb-gps.sh" "$WORK/data/usr/bin/brvg-setup-usb-gps"
# Relay tier: the CGI webhook receiver (inert until HUB_LITE_ENABLED=1 in the config).
install -m 0755 "$SRC/hub-lite-cgi.sh" "$WORK/data/www/brvg/cgi-bin/report"
install -m 0755 "$SRC/hub-lite-mgmt.sh" "$WORK/data/www/brvg/cgi-bin/mgmt"
# The /api/hub/* door. Installed WITHOUT a .sh suffix and directly at api/hub, because uhttpd
# resolves the longest existing file path and hands the rest over as PATH_INFO — so this one file
# answers /api/hub/ping, /api/hub/status and /api/hub/linktap/state.
install -m 0755 "$SRC/hub-lite-api.sh" "$WORK/data/www/brvg/api/hub"

# NOTE: no config file ships in the package. The config carries this device's token and is written
# by the app at enrollment; packaging a placeholder would risk overwriting a live one on upgrade.
# uhttpd serves the hub-lite CGI receiver and is NOT stock on GL.iNet firmware (bench GL-X750
# 4.3.28 ships nginx only) — opkg pulls it at install, which needs router internet.
cat > "$WORK/control/control" <<EOF
Package: $PKG
Version: $VERSION
Depends: libc, curl, uhttpd
Section: net
Architecture: $ARCH
Maintainer: Boat & RV Guardian
Description: Reports GPS and modem telemetry to Boat & RV Guardian, so the vehicle
 keeps reporting when nobody has the app open aboard.
EOF

# Keep an existing /etc/brvg-hub-lite.conf across upgrades — it holds the device's credential.
cat > "$WORK/control/conffiles" <<'EOF'
/etc/brvg-hub-lite.conf
EOF

cat > "$WORK/control/postinst" <<'EOF'
#!/bin/sh
[ -f /etc/brvg-hub-lite.conf ] && chmod 600 /etc/brvg-hub-lite.conf
/etc/init.d/brvg-hub-lite enable 2>/dev/null || true
# Point opkg at the signed feed so the NEXT self_update has somewhere to pull from. Best-effort:
# feed-setup runs `set -e`, but a router with a read-only or unusual /etc must still finish
# installing the hub-lite, so a failure here is logged and swallowed rather than failing the
# package.
[ -x /usr/libexec/brvg-hub-lite/feed-setup ] && { /usr/libexec/brvg-hub-lite/feed-setup || echo "BRVG_FEED_SETUP_SKIPPED"; }
# Deliberately NOT started here: without a config it would loop on a fatal error. The app starts it
# after writing the configuration.
exit 0
EOF
chmod 0755 "$WORK/control/postinst"

cat > "$WORK/control/prerm" <<'EOF'
#!/bin/sh
/etc/init.d/brvg-hub-lite stop 2>/dev/null || true
/etc/init.d/brvg-hub-lite disable 2>/dev/null || true
exit 0
EOF
chmod 0755 "$WORK/control/prerm"

echo "2.0" > "$WORK/debian-binary"

( cd "$WORK/data" && tar czf "$WORK/data.tar.gz" . )
( cd "$WORK/control" && tar czf "$WORK/control.tar.gz" . )

# Verify the payload before sealing it: an empty or short package installs "successfully" and
# leaves the router with no hub-lite, which is the failure mode worth catching here rather than on
# somebody's boat.
tar tzf "$WORK/data.tar.gz" | grep -q './usr/bin/brvg-hub-lite' || { echo "payload missing the hub-lite" >&2; exit 1; }
tar tzf "$WORK/data.tar.gz" | grep -q './usr/libexec/brvg-hub-lite/feed-setup' || { echo "payload missing the feed provisioner" >&2; exit 1; }
tar tzf "$WORK/data.tar.gz" | grep -q './etc/init.d/brvg-hub-lite' || { echo "payload missing the init script" >&2; exit 1; }
tar tzf "$WORK/data.tar.gz" | grep -q './www/brvg/cgi-bin/report' || { echo "payload missing the relay CGI" >&2; exit 1; }
tar tzf "$WORK/data.tar.gz" | grep -q './www/brvg/cgi-bin/mgmt' || { echo "payload missing the management CGI" >&2; exit 1; }
tar tzf "$WORK/data.tar.gz" | grep -q './www/brvg/api/hub' || { echo "payload missing the /api/hub CGI" >&2; exit 1; }
tar tzf "$WORK/control.tar.gz" | grep -q './control' || { echo "control archive incomplete" >&2; exit 1; }

IPK="$OUT/${PKG}_${VERSION}_${ARCH}.ipk"
rm -f "$IPK"

# The ar archive is written BY HAND rather than shelling out to `ar`, for two reasons found the
# hard way: CI runners don't necessarily ship binutils (`ar: not found`), and macOS `ar` needs
# `-S` or it treats the members as object files and emits a 96-byte archive containing only a
# symbol table — a "package" that installs nothing. The format is trivial and this keeps the
# script's promise of needing no toolchain at all.
#   header: "!<arch>\n", then per member a 60-byte record
#   (name16 mtime12 uid6 gid6 mode8 size10 0x60 0x0A) followed by data padded to even length.
ar_add() {
  _f="$2"
  _name=$(basename "$_f")
  _size=$(wc -c < "$_f" | tr -d ' ')
  printf '%-16s%-12s%-6s%-6s%-8s%-10s\140\n' "$_name" 0 0 0 100644 "$_size" >> "$1"
  cat "$_f" >> "$1"
  [ $(( _size % 2 )) -eq 1 ] && printf '\n' >> "$1"
  return 0
}

printf '!<arch>\n' > "$IPK"
( cd "$WORK" && ar_add "$IPK" debian-binary && ar_add "$IPK" control.tar.gz && ar_add "$IPK" data.tar.gz )

# Sanity-check the sealed archive with no external tools.
[ "$(head -c 8 "$IPK")" = "!<arch>" ] || { echo "not an ar archive" >&2; exit 1; }
[ "$(wc -c < "$IPK" | tr -d ' ')" -gt 2000 ] || { echo "package suspiciously small" >&2; exit 1; }

echo "built $IPK"
