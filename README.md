# Boat & RV Guardian — on-site collectors

The on-site half of [Boat & RV Guardian](https://boatrvguardian.com): small collectors that run on
the vessel or RV, gather local sensor and GPS telemetry on the LAN, and report it to the Guardian
cloud in one batched roll-up per interval — alarms immediately, never batched.

Two implementations of **one wire contract**:

| | `hub/` | `agent/` (hub-lite) |
| --- | --- | --- |
| Language | TypeScript (Node 18+, zero runtime deps) | POSIX shell + uhttpd CGI |
| Hosts | Raspberry Pi, Docker, desktop | GL.iNet / OpenWrt-class routers (KBs footprint) |
| Does | webhook receiver, roll-up aggregation, NMEA 0183 TCP GPS source, local growth path (control, local API/UI) | webhook relay, roll-up, modem + GPS telemetry, immediate alarm passthrough |

A hub-lite is a **subset of a hub, never a variant**: both speak the same batch report format, and
the shared fixtures in each side's tests are what keep them from drifting. If you change the
contract in one place, a test goes red somewhere else — keep it that way.

## Integrating

The collectors report over outbound HTTPS only (nothing here listens on the WAN; CGNAT-friendly).
A collector is enrolled from the Guardian app, which mints the per-device token it authenticates
with. To feed your own hardware's telemetry through a collector:

- **GPS**: anything that serves NMEA 0183 over TCP on the LAN (chartplotter, AIS, gpsd, a cellular
  router's GNSS) — point the hub's `NMEA_HOST` at it.
- **Sensors**: HTTP GET/POST webhooks to the collector's LAN port
  (`/cgi-bin/report?device=<id>&event=<name>&<values>`) — events named `*.measurement` / `*.change`
  batch as telemetry; anything else is treated as an alarm and forwarded at once.

See `hub/README.md` and `agent/README.md` for running each tier.

## License

Apache-2.0 (see `LICENSE` and `NOTICE`). The Guardian **cloud service and apps are separate,
proprietary products** — this repository is the on-site integration surface, published so the
community can run, package, and extend the collectors and connect their own on-board hardware.
"Boat & RV Guardian" branding is a trademark of SC4 Technologies LLC and is not covered by the
code license.
