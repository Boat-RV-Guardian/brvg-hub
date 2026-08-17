# Agent package (.ipk) — the SSH-free install path

`sh agent/package/build-ipk.sh` produces `dist/brvg-agent_<version>_all.ipk`. The agent is POSIX
shell, so the package is architecture-independent (`all`) and needs no OpenWrt SDK or
cross-compiler — "building" is packaging.

## Why this exists

The app installs the agent **over SSH** today (`install_router_agent`). That works and needs no
infrastructure, but it is **desktop-only**: the SSH client is deliberately kept out of the Android
build. GL.iNet's 4.x RPC surface exposes `plugins.install_package`, so a package can be installed
**with no SSH at all, from any platform including a phone**.

It is also where the signed-artifact bar naturally lives: opkg feeds are signed (usign) and the
signature is verified **on-device by the package manager**, rather than by something we invent.

## What the package does and doesn't do

- Installs `/usr/bin/brvg-agent` and `/etc/init.d/brvg-agent`, and **enables** the service.
- Deliberately does **not** start it: without a configuration the agent exits with a fatal error,
  so the app starts it after writing `/etc/brvg-agent.conf`.
- Deliberately ships **no config file**. That file holds the device's token; packaging a
  placeholder would risk overwriting a live credential on upgrade. It is listed in `conffiles` so
  opkg preserves an existing one.

## Remaining, and it is owner infrastructure — not code

1. **Host a feed.** Publish `dist/*.ipk` plus a `Packages`/`Packages.gz` index to a public URL
   (R2 is the obvious home — it already serves app downloads).
2. **Sign it** (usign key), publish the public key, and add the feed + key to the router.
3. Point the router at the feed (`/etc/opkg/customfeeds.conf`), then the app can call
   `plugins.update_repository` + `plugins.install_package` over the RPC it already speaks —
   no SSH, works on Android.

Until then the SSH path remains the shipped install, and this package is built and verified in CI
so it cannot rot before it is needed.

## Updating an installed agent — the signed path

`sh agent/package/build-feed.sh` builds the `Packages` index over `dist/*.ipk` and **signs it**.
It **refuses to emit an unsigned feed** unless you pass `ALLOW_UNSIGNED=1`, because an unsigned
feed is not a weaker signed feed — it is a remote-code-execution channel with the lock off, and it
fails silently (opkg installs from it happily if signature checking was never turned on).

```sh
sh agent/package/build-ipk.sh                       # version comes from AGENT_VERSION in the agent
USIGN_KEY=~/keys/brvg-feed.key sh agent/package/build-feed.sh
```

Generate the key pair once, and keep the SECRET half off CI and out of the repo:

```sh
usign -G -s brvg-feed.key -p brvg-feed.pub -c "Boat & RV Guardian agent feed"
```

### The chain of trust

```
Packages.sig  →  Packages (signed index)  →  SHA256Sum per .ipk  →  the .ipk
```

Verification happens **on the router**, by opkg, against a public key in `/etc/opkg/keys/` — not by
the agent, and not by anything we wrote.

### Why the cloud cannot choose what gets installed

The command channel's `self_update` verb takes **no argument**. The cloud decides *who* updates and
*when* (`brvg-cloud-server/src/agentRollout.ts`); the signed feed decides *what*. Compromising the
command queue therefore changes the timing of a vendor-signed install, not its contents. **Never
give that verb a version or URL parameter** — that single change would turn a fixed allowlist back
into a code-execution channel.

### Rollback

Before upgrading, the agent copies itself to `/etc/brvg-agent.prev`. If the newly installed agent
cannot even print its own version, the old one is restored automatically. `rollback_agent` does the
same thing on demand. On a boat behind CGNAT there is no remote undo, so the undo has to be local.

### Still owner infrastructure — not code

1. Publish `dist/*.ipk`, `Packages`, `Packages.gz` and `Packages.sig` to a public URL (R2).
2. Install the public key on routers (`/etc/opkg/keys/<fingerprint>`) and add the feed to
   `/etc/opkg/customfeeds.conf`.

Until both exist, `self_update` finds no feed and logs that it did nothing — it does not invent a
fallback download path.
