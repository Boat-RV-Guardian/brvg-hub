# Hub-lite package (.ipk) — the SSH-free install path

`sh hub-lite/package/build-ipk.sh` produces `dist/brvg-hub-lite_<version>_all.ipk`. The hub-lite is POSIX
shell, so the package is architecture-independent (`all`) and needs no OpenWrt SDK or
cross-compiler — "building" is packaging.

## Why this exists

The app installs the hub-lite **over SSH** today (`install_router_agent`). That works and needs no
infrastructure, but it is **desktop-only**: the SSH client is deliberately kept out of the Android
build. GL.iNet's 4.x RPC surface exposes `plugins.install_package`, so a package can be installed
**with no SSH at all, from any platform including a phone**.

It is also where the signed-artifact bar naturally lives: opkg feeds are signed (usign) and the
signature is verified **on-device by the package manager**, rather than by something we invent.

## What the package does and doesn't do

- Installs `/usr/bin/brvg-hub-lite` and `/etc/init.d/brvg-hub-lite`, and **enables** the service.
- Deliberately does **not** start it: without a configuration the hub-lite exits with a fatal error,
  so the app starts it after writing `/etc/brvg-hub-lite.conf`.
- Deliberately ships **no config file**. That file holds the device's token; packaging a
  placeholder would risk overwriting a live credential on upgrade. It is listed in `conffiles` so
  opkg preserves an existing one.

## The feed, and how it is published

The feed now exists. `.github/workflows/hub-lite-feed.yml` builds the `.ipk`, signs the `Packages`
index, and publishes to a single **rolling GitHub release tagged `hub-lite-feed`** (marked *not*
latest, so it never touches the `latest` link the daemon release claims). Routers point opkg at:

```
src/gz brvg_hublite https://github.com/DockNeighbor/DockNeighbor-Hub/releases/download/hub-lite-feed
```

a stable URL whose `Packages.gz` / `Packages.sig` the workflow overwrites each release. Both install
paths — the `.ipk` postinst and the app's over-SSH installer — run
[`feed-setup.sh`](feed-setup.sh), which installs the public key at `/etc/opkg/keys/<fingerprint>`
and writes that customfeeds line. So a freshly installed router is already pointed at the feed, and
`self_update` (or `plugins.update_repository` + `plugins.install_package` over GL.iNet RPC, no SSH,
works on Android) has somewhere to pull from.

### Cutting a feed release

Bump `HUB_LITE_VERSION` in `hub-lite/brvg-hub-lite.sh`, then push a matching tag:

```sh
git tag hub-lite-v0.14.4 && git push origin hub-lite-v0.14.4
```

The workflow refuses to publish if the tag and `HUB_LITE_VERSION` disagree. A `workflow_dispatch`
run builds and signs the feed as an **artifact** without publishing — the rehearsal valve.

### The one owner step: the signing secret

The keypair is generated **once** (already done — public key committed as
[`brvg-feed.pub`](brvg-feed.pub), fingerprint `b0ff2bec314c57d3`). The SECRET half is not in the
repo and must be added as the `HUB_LITE_USIGN_KEY` **repository secret** — that is the only thing
gating a live publish, and only a `hub-lite-v*` tag (which only a maintainer can push) triggers it.
Regenerating the key changes the fingerprint and forces every router to be re-provisioned, so do
not regenerate it casually.

## Updating an installed hub-lite — the signed path

`sh hub-lite/package/build-feed.sh` builds the `Packages` index over `dist/*.ipk` and **signs it**.
It **refuses to emit an unsigned feed** unless you pass `ALLOW_UNSIGNED=1`, because an unsigned
feed is not a weaker signed feed — it is a remote-code-execution channel with the lock off, and it
fails silently (opkg installs from it happily if signature checking was never turned on).

```sh
sh hub-lite/package/build-ipk.sh                       # version comes from HUB_LITE_VERSION in the hub-lite
USIGN_KEY=~/keys/brvg-feed.key sh hub-lite/package/build-feed.sh
```

Generate the key pair once, and keep the SECRET half off CI and out of the repo:

```sh
usign -G -s brvg-feed.key -p brvg-feed.pub -c "Boat & RV Guardian hub-lite feed"
```

### The chain of trust

```
Packages.sig  →  Packages (signed index)  →  SHA256Sum per .ipk  →  the .ipk
```

Verification happens **on the router**, by opkg, against a public key in `/etc/opkg/keys/` — not by
the hub-lite, and not by anything we wrote.

### Why the cloud cannot choose what gets installed

The command channel's `self_update` verb takes **no argument**. The cloud decides *who* updates and
*when* (`brvg-cloud-server/src/agentRollout.ts`); the signed feed decides *what*. Compromising the
command queue therefore changes the timing of a vendor-signed install, not its contents. **Never
give that verb a version or URL parameter** — that single change would turn a fixed allowlist back
into a code-execution channel.

### Rollback

Before upgrading, the hub-lite copies itself to `/etc/brvg-hub-lite.prev`. If the newly installed hub-lite
cannot even print its own version, the old one is restored automatically. `rollback_agent` does the
same thing on demand. On a boat behind CGNAT there is no remote undo, so the undo has to be local.

Both of these are now automated — the feed is published by CI (above) and the router is pointed at
it by `feed-setup.sh` at install. A router that predates this (installed before `feed-setup` shipped)
has no feed configured: re-running the app's installer, or the `.ipk` postinst, provisions it. Until
a router is provisioned, `self_update` finds no feed and logs that it did nothing — it does not
invent a fallback download path.
