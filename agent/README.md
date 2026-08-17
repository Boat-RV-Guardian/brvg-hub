# BRVG phone-home agent (Phase A)

One POSIX-shell agent, two homes: a **GL.iNet router** (busybox ash, procd) and a **Raspberry
Pi-class hub** (systemd). It pushes GPS and modem telemetry **outbound** over HTTPS to the hosted
worker on a timer, so the vehicle reports without the app being onsite. Phase A = telemetry push;
the Phase B command channel is deliberately not in this skeleton.

## How it reports

Standard telemetry webhooks — the exact path every Shelly on the vehicle already uses, so the
cloud side needs **zero changes**:

```
GET {WORKER_URL}/api/shelly?vid=…&device=…&event=gps.measurement&k=…&lat=…&lon=…&acc=…
GET {WORKER_URL}/api/shelly?vid=…&device=…&event=modem.measurement&k=…&rssi=…&rsrp=…&carrier=…&sim=…
```

Auth, preferred: a **per-device revocable token** (`DEVICE_TOKEN` → `GET /api/agent?...&t=`),
minted once by an admin via `POST /api/agent/enroll {vid, deviceId}` (Firebase ID token required;
re-enroll rotates, DELETE revokes). Legacy fallback when `DEVICE_TOKEN` is empty: the per-vehicle
webhook key (`VEHICLE_KEY` → `/api/shelly?...&k=`). Both land in the identical cloud pipeline.

## Install — from the app (the normal path)

Router panel → **☁️ Cloud reporting → Connect to cloud**. The desktop app mints this device's
token and installs the agent onto the router over SSH using the admin password it already holds:
script, init service and the token-bearing config are written, the service is enabled, started and
verified. **No files are edited by hand.**

Availability: desktop (Tauri) builds only — the SSH client is deliberately kept out of the Android
binary (see `src-tauri/Cargo.toml`). Where it isn't available the app shows the manual steps below
instead of a button that cannot work. The eventual all-platform path is an **opkg feed**
(`plugins.install_package` exists in the 4.x RPC surface, so a signed .ipk installs with no SSH at
all) — that lands with the packaged agent under the signed-artifact bar.

## Install — by hand (fallback / non-GL.iNet)

GL.iNet (tested platform: GL-X750, fw 4.3.28)

```sh
scp agent/brvg-agent.sh root@192.168.8.1:/usr/bin/brvg-agent
scp agent/brvg-agent.conf.example root@192.168.8.1:/etc/brvg-agent.conf
scp agent/openwrt/etc/init.d/brvg-agent root@192.168.8.1:/etc/init.d/brvg-agent
ssh root@192.168.8.1 'chmod 755 /usr/bin/brvg-agent /etc/init.d/brvg-agent; chmod 600 /etc/brvg-agent.conf'
# edit /etc/brvg-agent.conf (VID, DEVICE_ID, VEHICLE_KEY), then:
ssh root@192.168.8.1 '/etc/init.d/brvg-agent enable && /etc/init.d/brvg-agent start; logread -f | grep brvg'
```

GPS/modem are read with AT commands straight to the modem port — the agent runs as root
on-device, so no RPC login is involved. `AT_PORT` defaults to `/dev/ttyUSB2` (correct for the
X750's EC25). **X3000-class PCIe modems (RM520) expose a different device** — check
`ls /dev/mhi_* /dev/ttyUSB*` on the unit and set `AT_PORT`; expect `/dev/mhi_DUN`-style names
[unverified until an X3000 is on the bench].

## Install — Raspberry Pi hub

**One command**: `sudo sh agent/hub/install.sh` (see [hub/README.md](hub/README.md) for what the
box is for and the hardware notes). The manual steps below are the same thing, unpacked.

```sh
sudo cp agent/brvg-agent.sh /usr/local/bin/brvg-agent && sudo chmod 755 /usr/local/bin/brvg-agent
sudo cp agent/brvg-agent.conf.example /etc/brvg-agent.conf && sudo chmod 600 /etc/brvg-agent.conf
sudo cp agent/systemd/brvg-agent.service /etc/systemd/system/
# edit /etc/brvg-agent.conf, then:
sudo systemctl enable --now brvg-agent && journalctl -fu brvg-agent
```

### GPS on a router with no GPS antenna port

Some models (verified: **GL-X750** — two cellular SMA ports and nothing else) enable the modem's
GNSS happily and then see zero satellites forever, because the receiver has no antenna wired to it.
The fix is a **USB GPS dongle** (u-blox class, ~$15 — VK-162 / BU-353S4): plug it into the router's
USB port and the agent finds it automatically. `GPS_SOURCE=auto` tries the modem first, then a USB
NMEA device (`/dev/ttyACM*`, and `ttyUSB` ports that are not the modem's AT port), then gpsd — the
ORDER matters, because a router with no GPS antenna answers the modem read forever with "no fix",
so the dongle must be tried even when the modem is present and healthy.

OpenWrt needs kernel modules before the dongle appears as a device. **Don't run those by hand** —
the package ships a setup script that installs them, finds the receiver, and reads it back to prove
it is really sending NMEA:

```sh
brvg-setup-usb-gps            # plug the dongle in first; idempotent, safe to re-run
brvg-setup-usb-gps --status   # what it found, and which GPS_SOURCE the agent is set to
```

It installs `kmod-usb-acm` (u-blox and most CDC-ACM receivers) plus the FTDI/Prolific modules. That
is all that is needed — the agent reads the device directly. It is NOT run at install time: the
dongle is normally plugged in later, and opkg needs the router online at the moment it runs.

Pin it explicitly with `GPS_SOURCE=nmea` + `GPS_DEVICE=/dev/ttyACM0` if auto-detection picks wrong.

Two network sources join the chain (2026-08-17, GPS parity with the hub — explicit choices, never
part of `auto`):

- **`GPS_SOURCE=tcp`** + `GPS_HOST`/`GPS_PORT` — NMEA 0183 served on the LAN (chartplotter, AIS,
  gpsd). The agent dials in, reads a burst, and reuses the same RMC parser as the serial path.
  ⚠️ Uses busybox `nc`; verify it exists on FACTORY-STOCK firmware before shipping (the same trap
  as the manually-installed Lua on the bench box).
- **`GPS_SOURCE=cradlepoint`** + `CRADLEPOINT_HOST`/`_PORT`/`_USER`/`_PASSWORD` — poll a
  Cradlepoint NCOS router's local `/api/status/gps` over HTTP Basic auth (same env names as the
  TypeScript hub). The router is POLLED, never configured to push anywhere; the DMS payload shape
  is pinned to a bench capture (CBA850 fw 7.0.50).

### How the position actually reaches the app

**The agent reads the dongle itself.** `GPS_SOURCE=auto` (the default the app writes) tries the
modem's GNSS, then a USB NMEA device, then gpsd, and pushes fixes outbound on its normal GPS tick.
There is nothing to serve and no port to open, and it works with nobody aboard.

⚠️ An earlier version of this doc described publishing the receiver over TCP with ser2net so the
desktop app could poll it. **That was the wrong design and has been removed** (2026-08-16). A USB
GPS is a serial device on the router; turning it into a network service added a daemon to install,
a port to open, a desktop-only dependency, and a second reader competing with the agent for the same
device — to arrive at a position the agent was already sending, and only while somebody was aboard.

If the app shows no position from the router, the order to check is: does `/dev/ttyACM0` exist
(`brvg-setup-usb-gps --status`), is the agent running, and is `GPS_SOURCE` `auto` rather than `at`.

## Behavior

- GPS every `GPS_INTERVAL` (default 120 s, floor 30), modem every `MODEM_INTERVAL` (default
  600 s, floor 60). One small HTTPS GET per report — this rides the customer's metered link.
- No fix → nothing sent (the cloud keeps last-known; "no fix" is diagnosed by the app's
  Health Check, not by telemetry spam). Failed sends are dropped; the next tick retries.
  Phase A is telemetry, not store-and-forward.
- Random start offset so a fleet doesn't tick in lockstep after a regional power event.

## Tests

`sh agent/test.sh` — every parser is exercised with responses captured from the real GL-X750
bench session (2026-08-06) plus standard NMEA/gpsd shapes. Runs in CI (`agent` job).

## Security posture (Phase A)

- Outbound HTTPS only; `WORKER_URL` must be https and defaults to the pinned first-party worker.
- The config file holds the vehicle key → `chmod 600`, root-owned.
- No inbound listener, no command execution, no self-update. Anything that installs or updates
  code on customer devices carries the full signed-artifact bar (see ROUTER-PHONE-HOME.md) —
  this skeleton is installed manually, by us, on bench/dev hardware.


## Relay tier (X750-class routers) — roll up the Shellys' reports

`HUB_LITE_ENABLED=1` in `/etc/brvg-agent.conf` turns this agent into the **hub-lite tier** of the hub
architecture: a LAN-only uhttpd instance serves
`hub-lite-cgi.sh` at `http://<router>:8181/cgi-bin/report`, the Shellys' webhooks are re-registered
against it, and the agent drains the spool into ONE `/api/agent/batch` report per modem interval.

Why: the metered link pays per TLS handshake, not per byte — one roll-up connection replaces one
connection per device per event. And once no sensor talks to the internet directly, lockdown's
forward chain can be deny-all with no allow rules at all.

Rules that hold regardless of settings:

- **Alarms are never held.** Anything that isn't `*.measurement`/`*.change` is sent to the cloud
  immediately by the CGI itself, as its own single-item batch. Only if that send fails is it
  spooled for the next drain — never both, so an alarm is never double-reported.
- Devices whose newest values are UNCHANGED since the last successful report ride the `ok` list
  ("all my devices are good, except these") — a freshness touch, not data.
- Every `KEYFRAME_EVERY`th drain is a full keyframe, bounding how long a lost delta can
  leave the cloud's view stale.
- A failed drain retries with the SAME sequence number, so the cloud can drop a replay whole
  instead of re-firing its alerts.

Pure shell throughout — Lua is not stock GL.iNet firmware. ⏳ Before shipping to customers:
confirm `uhttpd` is present on a FACTORY-RESET X750 (it was present on the bench unit, which has
had packages added); if not stock, it becomes a `Depends:` in the .ipk.


## Hub watchdog — failing open, except in bandwidth saver mode

Owner decisions 2026-08-13 + 2026-08-17. **`BANDWIDTH_SAVER=1` disables the watchdog entirely and
lockdown FAILS CLOSED**: when lockdown exists to control metered-SIM spend, a dead hub must not
release it — a silent vessel until the connectivity-offline alert fires is the accepted trade.
Everything below applies only to normal (non-saver) installs.

Under a *traffic* lockdown the hub is the only route to the cloud. If
the hub runs on this router that is fine — a dead router is a dead gateway either way — but a hub
on a **separate** box (Pi, Docker, desktop) can die while the router routes happily, and then the
vessel is silent with no remote way back in.

Set `HUB_WATCH_URL` to the hub's `/healthz` and the router will watch it. After `HUB_WATCH_FAILS`
consecutive failures it **removes the lockdown rules** so sensors can reach the cloud directly
again, and sends `hub.offline` so you hear about it. When the hub returns it sends `hub.online`.

It deliberately does **not** re-arm the lockdown on recovery: a hub that restarts every few minutes
would flap the firewall, and every apply is a ~10 s reload. The released state is announced;
re-applying is one tap in the app.

The `brvg_lk_` rule-name prefix is the shared contract between this watchdog and the app's SSH
enforcement — both manage the same rules by matching that prefix, which is how two languages stay
in step without sharing code.


## WAN usage accounting

Every modem report also carries per-source usage: `wanSrc` (which WAN is carrying traffic right
now) and `wanKb_cellular` / `wanKb_wifi` / `wanKb_wired` deltas. No configuration — it reads
`/sys/class/net/*/statistics/*_bytes` and degrades silently where those don't exist (macOS, a
container without them).

**Why deltas and not totals.** Those counters are since-boot, and three separate things send them
backwards: a reboot, an interface bounce, and our own `reset_data` command. `wan_delta` treats any
DECREASE as a reset and reports the new value — otherwise unsigned arithmetic produces a
wrap-sized spike that the cloud would read as a runaway plan burn.

The modem's own `AT+QGDCNT` counter still drives the plan alerts, because that is what the carrier
meters. These interface counters answer the different question — *where* the bytes went — which is
what makes the Wi-Fi uplink and the roll-up measurable rather than merely plausible. Cross-checked
on the bench: QGDCNT and `wwan0` agree to ~1% over 14 h.

## Hub watchdog and hub health

`HUB_WATCH_URL` should point at the hub's `/healthz`. That endpoint reports **delivery**, not just
liveness: it answers 503 once the hub has failed several consecutive drains, so a hub that is
running but cannot reach the cloud still trips the watchdog and releases the lockdown. A hub whose
web server is up while nothing is being delivered is precisely the case fail-open exists for.
