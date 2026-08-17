# BRVG Hub — the onsite proxy box

Owner direction, 2026-08-07: *"a small box that can act as a hub and run the same proxy/polling
that the GL.iNet could run… it should have Z-Wave and Bluetooth capabilities… a raspberry pi like
device."*

The strategic point is that this box **decouples what we can do from which router the customer
owns**. The agent is the same one the routers run — one codebase, two homes — so everything
already built (telemetry, per-device tokens, the command channel, plan tracking) works here on day
one.

```sh
sudo sh agent/hub/install.sh      # idempotent; re-run to upgrade
```

## Why a hub, not just the router agent

It collapses five separate threads (recorded in brvg-internal open-tasks 📡):

1. **Vendor independence.** Peplink and Cradlepoint have no on-device app platform, and the CBA850
   is NetCloud-tethered and end-of-support in 2027. A hub runs behind *any* internet — dock Wi-Fi,
   a customer's existing router, anything.
2. **Radios Wi-Fi can't give us.** Z-Wave via USB stick reaches sensors (including water-shutoff
   valves) that Wi-Fi can't, and every Pi has Bluetooth for provisioning and presence.
3. **Possibly the range story without a vendor cloud** — Z-Wave Long Range claims ~1 mile
   line-of-sight. **Field-verify before it retires the YoLink track**; a datasheet is not a boat.
4. **Camera snapshot ingest** wanted "the first always-on non-serverless component". The hub *is*
   that component — at the boat, keeping the cloud serverless.
5. **GPS** for routers with no antenna port: the same USB dongle path the agent already supports.

## Hardware notes (learned, not guessed)

- **Prototype**: any Pi + a Zooz ZST39 LR Z-Wave stick + a u-blox USB GPS dongle (~$60 on top of
  the Pi).
- **Product**: CM4/CM5 with **eMMC**, or an HA-Yellow-class carrier. SD-card corruption is the
  classic Pi-product killer; a read-only rootfs and eMMC is the difference between a gadget and an
  appliance.
- 12 V buck converter, ~2–4 W draw. Marine environment: conformal coating or a sealed enclosure.
- Shipping our own OS image puts the **full signed-update bar** in scope from day one — the same
  bar the .ipk path exists to satisfy on routers.

## What this scaffolding does and doesn't do

**Does**: installs the agent + systemd unit, pulls curl/gpsd/bluez, reports which serial devices
and Bluetooth adapters are actually present (so the bench spike starts from facts, not assumptions),
enables the service, and refuses to start it without a configuration — an agent with no config
exits fatally, and a service that flaps looks like a bug.

**Doesn't**: install a Z-Wave stack. That is the next increment and it wants Node (Z-Wave JS); the
right time is when a stick is in hand and the range test can settle the YoLink question in the same
sitting.
