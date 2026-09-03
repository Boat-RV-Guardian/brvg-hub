#!/bin/sh
# Build (and SIGN) an opkg feed index from the packages in dist/.
#
# WHY: pushing code to customer routers is the single largest security surface this product would
# ever ship, and the owner's requirement is explicit — signed artifacts, verified on-device.
# opkg already does exactly that: it verifies a usign (Ed25519) signature over the `Packages`
# index, and the index pins each package by SHA256. So the chain is:
#
#     Packages.sig  →  Packages (signed index)  →  SHA256Sum per .ipk  →  the .ipk itself
#
# Verification happens ON THE ROUTER, by the package manager, using a public key installed under
# /etc/opkg/keys — not by anything we invent, and not by the hub-lite itself.
#
# THIS SCRIPT REFUSES TO PRODUCE AN UNSIGNED FEED unless you explicitly ask for one. An unsigned
# feed is not a smaller version of a signed feed; it is a remote-code-execution channel with the
# lock left off, and the failure mode is silent (opkg installs it happily if signature checking
# was never turned on). Making that state require an obvious, ugly flag is the point.
#
# Usage:
#   USIGN_KEY=/path/to/secret.key sh hub-lite/package/build-feed.sh
#   ALLOW_UNSIGNED=1 sh hub-lite/package/build-feed.sh      # local testing ONLY — never publish this
#
# Key handling (owner infrastructure — see README.md):
#   usign -G -s brvg-feed.key -p brvg-feed.pub -c "Boat & RV Guardian hub-lite feed"
# The SECRET key never leaves the machine that signs, is never committed, and never goes in CI
# without a review of who can trigger a publish. The PUBLIC key ships to routers.

set -eu

DIST="${DIST:-$(pwd)/dist}"
USIGN_KEY="${USIGN_KEY:-}"
ALLOW_UNSIGNED="${ALLOW_UNSIGNED:-}"

[ -d "$DIST" ] || { echo "no dist/ directory — run build-ipk.sh first" >&2; exit 1; }

set -- "$DIST"/*.ipk
[ -e "$1" ] || { echo "no .ipk files in $DIST — run build-ipk.sh first" >&2; exit 1; }

INDEX="$DIST/Packages"
: > "$INDEX"

# Portable SHA-256: coreutils, busybox and macOS all spell it differently.
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
  else echo "no sha256 tool available" >&2; exit 1
  fi
}

# Pull the control fields back out of each package so the index cannot drift from what shipped.
# (The control member is the second ar member; extracting it without `ar` keeps this script's
# no-toolchain promise, same as build-ipk.sh.)
# An .ipk is a GZIPPED TAR (see build-ipk.sh — .deb is ar, .ipk is not), so pulling the control
# file out is two tar calls. This used to hand-walk 60-byte `ar` member headers, which matched the
# packages this repo built and NOTHING opkg could install; against a real .ipk it read gzip bytes as
# a decimal size and died with "Illegal number".
extract_control() {
  _ipk="$1"
  _tmp="$2"
  tar xzf "$_ipk" -C "$_tmp" ./control.tar.gz 2>/dev/null \
    || tar xzf "$_ipk" -C "$_tmp" control.tar.gz 2>/dev/null \
    || return 1
  ( cd "$_tmp" && { tar xzf control.tar.gz ./control 2>/dev/null || tar xzf control.tar.gz control 2>/dev/null; } )
  [ -f "$_tmp/control" ] || [ -f "$_tmp/./control" ] || return 1
  return 0
}

for ipk in "$DIST"/*.ipk; do
  work=$(mktemp -d)
  if ! extract_control "$ipk" "$work"; then
    rm -rf "$work"
    echo "could not read control data from $ipk" >&2
    exit 1
  fi
  ctrl="$work/control"
  [ -f "$ctrl" ] || ctrl="$work/./control"
  {
    # Control fields first, verbatim, then the fields opkg needs to fetch and verify the file.
    sed -e '/^$/d' "$ctrl"
    echo "Filename: $(basename "$ipk")"
    echo "Size: $(wc -c < "$ipk" | tr -d ' ')"
    echo "SHA256sum: $(sha256_of "$ipk")"
    echo
  } >> "$INDEX"
  rm -rf "$work"
done

gzip -9 -c "$INDEX" > "$DIST/Packages.gz"
echo "indexed $(grep -c '^Package:' "$INDEX") package(s) → $INDEX"

# --- Signing ------------------------------------------------------------------------------------

if [ -n "$USIGN_KEY" ]; then
  command -v usign >/dev/null 2>&1 || { echo "usign not installed — cannot sign" >&2; exit 1; }
  [ -f "$USIGN_KEY" ] || { echo "USIGN_KEY does not exist: $USIGN_KEY" >&2; exit 1; }
  usign -S -m "$INDEX" -s "$USIGN_KEY" -x "$DIST/Packages.sig"
  # Prove the signature verifies before anything is published. A signature nobody checked is a
  # signature that isn't there.
  if command -v usign >/dev/null 2>&1 && [ -n "${USIGN_PUB:-}" ]; then
    usign -V -m "$INDEX" -p "$USIGN_PUB" -x "$DIST/Packages.sig" \
      || { echo "the signature did NOT verify against $USIGN_PUB" >&2; exit 1; }
    echo "signature verified against $USIGN_PUB"
  fi
  echo "signed → $DIST/Packages.sig"
  exit 0
fi

if [ -n "$ALLOW_UNSIGNED" ]; then
  echo "" >&2
  echo "*** UNSIGNED FEED — FOR LOCAL TESTING ONLY. DO NOT PUBLISH THIS. ***" >&2
  echo "*** Routers configured to check signatures will refuse it; routers that   ***" >&2
  echo "*** are not are accepting arbitrary code from whoever serves this URL.    ***" >&2
  echo "" >&2
  exit 0
fi

echo "refusing to emit an unsigned feed. Set USIGN_KEY=<secret key> to sign it," >&2
echo "or ALLOW_UNSIGNED=1 if you are testing locally and will not publish it." >&2
exit 1
