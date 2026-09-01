#!/bin/sh
# Point this router's opkg at the SIGNED Boat & RV Guardian hub-lite feed, and install the public
# key that verifies it. Idempotent — safe to re-run on every (re)install.
#
# WHY THIS IS SEPARATE, AND WHY IT MATTERS. `self_update` (brvg-hub-lite.sh) asks opkg to upgrade
# from "the SIGNED feed the router is already configured with". Until this snippet has run, there
# IS no such feed: self_update finds nothing and logs that it did nothing. So this is the one step
# that turns the whole cloud→signed-update path on. It ships in the .ipk's postinst AND in the
# app's over-SSH installer, because a router can arrive by either route and both must end up
# trusting the same key.
#
# The key is trusted OUT OF BAND: it is baked into the installer, never fetched from the feed it
# secures (that would be circular). opkg verifies every `Packages` index against it on-device — the
# cloud decides WHO updates and WHEN, never WHAT.
#
# THE FEED URL AND FINGERPRINT ARE THE CONTRACT. The fingerprint is the public key's own
# usign -F output; the file under /etc/opkg/keys MUST be named by it, because that is the name opkg
# looks up when it sees a signature. Regenerating the key changes the fingerprint and every router
# must be re-provisioned — so the key is generated ONCE (see hub-lite/package/README.md).

set -e

FEED_NAME="brvg_hublite"
FEED_URL="https://github.com/Boat-RV-Guardian/brvg-hub/releases/download/hub-lite-feed"
KEY_FINGERPRINT="b0ff2bec314c57d3"

# The public key, verbatim from hub-lite/package/brvg-feed.pub. Two lines: the comment and the
# base64 blob. usign reads both.
KEY_BODY='untrusted comment: Boat & RV Guardian hub-lite feed
RWSw/yvsMUxX0+mbunbU/mH8ZNwomavKQQM4C4dE7qeK1blQRQ2SGUFp'

# 1. Install the trust anchor. /etc/opkg/keys/<fingerprint> is where usign (via opkg) looks.
mkdir -p /etc/opkg/keys
printf '%s\n' "$KEY_BODY" > "/etc/opkg/keys/$KEY_FINGERPRINT"
chmod 644 "/etc/opkg/keys/$KEY_FINGERPRINT"

# 2. Point opkg at the feed, replacing any prior line for THIS feed name so re-runs don't stack up.
# Done with a temp file rather than `sed -i` so it behaves identically on busybox, GNU and BSD
# (macOS `sed -i` needs an argument the others reject) — which also lets the test exercise it.
_feeds=/etc/opkg/customfeeds.conf
_tmp="$_feeds.brvg.$$"
if [ -f "$_feeds" ]; then
  grep -v "^src/gz[[:space:]][[:space:]]*${FEED_NAME}[[:space:]]" "$_feeds" > "$_tmp" || true
else
  : > "$_tmp"
fi
printf 'src/gz %s %s\n' "$FEED_NAME" "$FEED_URL" >> "$_tmp"
mv "$_tmp" "$_feeds"

echo "BRVG_FEED_CONFIGURED $KEY_FINGERPRINT"
