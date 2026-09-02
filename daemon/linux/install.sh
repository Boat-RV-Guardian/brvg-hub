#!/bin/sh
# Install the Rust hub as a systemd service on a Pi or any bare Linux box — the Linux counterpart of
# the macOS pkg and the Windows installer. Downloads the arch-correct binary from the SAME signed
# release the desktop app installs (releases/latest + SHA256SUMS), verifies it, and wires the unit
# with Restart=always so remote self-update (swap-and-exit) is relaunched by systemd.
#
#   sudo sh daemon/linux/install.sh
#
# Idempotent: re-running upgrades the binary in place and restarts. It does NOT write hub.json —
# that carries the device token and is written by the app at enrollment.

set -eu

REPO="DockNeighbor/DockNeighbor-Hub"
ROOT="/var/lib/DockNeighbor"
BIN="$ROOT/bin/brvg-hub"
UNIT="/etc/systemd/system/brvg-hub.service"
SRC="$(cd "$(dirname "$0")" && pwd)"

[ "$(id -u)" = "0" ] || { echo "run me with sudo" >&2; exit 1; }
command -v curl >/dev/null 2>&1 || { echo "curl is required" >&2; exit 1; }

# The daemon release ships one binary per arch; pick this box's. Only the two we build for.
case "$(uname -m)" in
  x86_64|amd64)   ASSET="brvg-hub-linux-x64" ;;
  aarch64|arm64)  ASSET="brvg-hub-linux-arm64" ;;
  *) echo "unsupported architecture: $(uname -m) (only x86_64 and aarch64 have a daemon build)" >&2; exit 1 ;;
esac

BASE="https://github.com/$REPO/releases/latest/download"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "==> downloading $ASSET"
curl -fSL -o "$TMP/$ASSET" "$BASE/$ASSET"
curl -fSL -o "$TMP/SHA256SUMS" "$BASE/SHA256SUMS"

echo "==> verifying checksum"
# The signed-artifact bar the whole update story rests on: never install a binary that does not match
# the release's published hash. Pull just this asset's line and check it.
EXPECT="$(grep " $ASSET\$" "$TMP/SHA256SUMS" | awk '{print $1}')"
[ -n "$EXPECT" ] || { echo "no checksum for $ASSET in SHA256SUMS" >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then
  GOT="$(sha256sum "$TMP/$ASSET" | awk '{print $1}')"
else
  GOT="$(shasum -a 256 "$TMP/$ASSET" | awk '{print $1}')"
fi
[ "$GOT" = "$EXPECT" ] || { echo "checksum mismatch: got $GOT, expected $EXPECT" >&2; exit 1; }

echo "==> proving the binary runs on this box"
chmod +x "$TMP/$ASSET"
# The same gate self-update uses: a correct hash on a binary that cannot exec here (wrong libc)
# would still not run. --version exits 0 and prints the version, or we refuse to install it.
VER="$("$TMP/$ASSET" --version)" || { echo "the downloaded binary would not run --version" >&2; exit 1; }

echo "==> installing $VER to $BIN"
mkdir -p "$ROOT/bin"
# Same-directory swap so an upgrade of a RUNNING binary works (the old inode stays mapped): keep the
# old one as .prev, move the new into place.
if [ -f "$BIN" ]; then cp -p "$BIN" "$BIN.prev" 2>/dev/null || true; fi
install -m 0755 "$TMP/$ASSET" "$BIN.new"
mv "$BIN.new" "$BIN"

echo "==> installing the service unit"
install -m 0644 "$SRC/brvg-hub.service" "$UNIT"
systemctl daemon-reload
systemctl enable brvg-hub >/dev/null 2>&1 || true
systemctl restart brvg-hub

sleep 2
if systemctl is-active --quiet brvg-hub; then
  echo "==> brvg-hub $VER is running (systemctl status brvg-hub)"
else
  echo "!! brvg-hub did not start — check: journalctl -u brvg-hub -n 50" >&2
  exit 1
fi
