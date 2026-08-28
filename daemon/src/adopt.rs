//! WHO MAY SIGN AN UNCLAIMED HUB — the "headless" door.
//!
//! WHY THIS EXISTS (owner, 2026-08-28): *"the app is running on a phone on the LAN and the hub is
//! running on a headless raspberry pi, how is it provisioned? central does not have a UI installed,
//! you install the hub service on it, how do you pair it?"*
//!
//! Until now setup was LOOPBACK ONLY, which quietly required a desktop app ON the hub machine. That
//! is impossible on a Pi, a NAS, a container, or any box you reach over SSH — the exact machines a
//! boat's always-on hub actually is. The loopback rule was not protecting anything a determined LAN
//! peer could not already reach; it was preventing the normal case.
//!
//! ⚠️ THIS IS A REAL, BOUNDED WEAKENING AND IT IS STATED HONESTLY. On a shared marina network,
//! opening a claim door means a stranger on that network could sign your unclaimed Pi to THEIR
//! vehicle. Three things bound it, and none of them is "nobody will try":
//!
//!   1. **Unregistered only.** A hub that has been signed refuses adoption outright, forever. The
//!      window is not a recurring vulnerability — it is one interval in a hub's whole life.
//!   2. **A window from service start.** Adoption closes ADOPTION_WINDOW after the process started.
//!      Missing it is not a lockout: restarting the service opens a fresh one, deliberately, so the
//!      recovery is a documented action rather than a permanent door. This is the same "pairing
//!      mode" model as Bluetooth, HomeKit and a Shelly's setup AP, for the same reason.
//!   3. **Same /24 as one of this hub's own addresses.** Not merely RFC1918 — a private address on
//!      some OTHER subnet reached through a router is not the "my phone is on this boat" case the
//!      door exists for, and refusing it costs the honest user nothing.
//!
//! And the claim is not silent: hub_server logs every attempt with the peer address, so a hub that
//! was taken can say by whom.
//!
//! Note what this door does NOT grant. Adoption still requires a cloud token the claimer had to
//! mint as an authenticated admin of some vehicle — the hub is not handing itself to an anonymous
//! packet. The exposure is "signed to the wrong account", not "signed to nobody".
//!
//! The other half of the answer — a hub with no app anywhere near it — is a PAIRING CODE the hub
//! redeems outbound, which needs no inbound door at all. That path is separate work; this module is
//! only about the LAN case.

use std::net::IpAddr;
use std::time::Duration;

/// How long after the service starts an unclaimed hub will answer the setup endpoints from the LAN.
///
/// Long enough to install the service, open the app, find the hub and name it without racing a
/// clock; short enough that an unattended box does not sit claimable for a week. Restarting the
/// service reopens it.
pub const ADOPTION_WINDOW: Duration = Duration::from_secs(15 * 60);

/// Why a setup call was refused — each maps to a different thing the person should DO, so they are
/// not collapsed into one "forbidden".
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum AdoptRefusal {
    /// Reachable, but not on any network this hub is itself on.
    OffNetwork,
    /// Same network, but the claim window has closed. Restarting the service reopens it.
    WindowClosed,
}

impl AdoptRefusal {
    pub fn message(self) -> &'static str {
        match self {
            AdoptRefusal::OffNetwork => {
                "this hub can only be set up from the computer it runs on, or from a device on the \
                 same local network"
            }
            AdoptRefusal::WindowClosed => {
                "this hub's setup window has closed — restart the hub service to open it again, \
                 then set the hub up within 15 minutes"
            }
        }
    }
}

/// PURE: may this peer set up this (still unclaimed) hub?
///
/// `local_ips` are the hub's own IPv4 addresses; `uptime` is how long the process has been running.
/// Both are passed in rather than read, so every branch below is testable without a network.
pub fn may_set_up(
    peer: IpAddr,
    local_ips: &[String],
    uptime: Duration,
    window: Duration,
) -> Result<(), AdoptRefusal> {
    // Loopback is the original door and is NOT time-boxed. Someone with a shell on the box can
    // restart the service at will, so a window would inconvenience them and stop nobody.
    if peer.is_loopback() {
        return Ok(());
    }
    if !same_slash24(peer, local_ips) {
        return Err(AdoptRefusal::OffNetwork);
    }
    if uptime > window {
        return Err(AdoptRefusal::WindowClosed);
    }
    Ok(())
}

/// Is this peer on the same /24 as one of the hub's own addresses?
///
/// A /24 rather than the interface's real mask because that is what the hub can know without
/// parsing platform-specific interface tables — and it errs on the side of REFUSING, which is the
/// safe direction: a wider real subnet means an honest user on `10.0.5.x` talking to a hub on
/// `10.0.4.x` falls back to the pairing code, rather than a stranger being let in.
fn same_slash24(peer: IpAddr, local_ips: &[String]) -> bool {
    let v4 = match peer {
        IpAddr::V4(v4) => v4,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            // A dual-stack listener reports LAN peers as ::ffff:a.b.c.d; that is the same host.
            Some(m) => m,
            None => return false,
        },
    };
    let peer_prefix = match crate::linktap_discover::slash24_prefix(&v4.to_string()) {
        Some(p) => p,
        None => return false,
    };
    local_ips
        .iter()
        .filter_map(|ip| crate::linktap_discover::slash24_prefix(ip))
        .any(|p| p == peer_prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }
    const LOCAL: [&str; 1] = ["192.168.8.40"];
    fn local() -> Vec<String> {
        LOCAL.iter().map(|s| s.to_string()).collect()
    }
    const FRESH: Duration = Duration::from_secs(60);
    const STALE: Duration = Duration::from_secs(20 * 60);

    #[test]
    fn loopback_is_always_allowed_and_is_never_time_boxed() {
        // The original door. Someone with a shell can restart the service whenever they like, so a
        // window here would inconvenience the honest case and stop no one.
        assert_eq!(may_set_up(ip("127.0.0.1"), &local(), STALE, ADOPTION_WINDOW), Ok(()));
        assert_eq!(may_set_up(ip("::1"), &[], Duration::from_secs(999_999), ADOPTION_WINDOW), Ok(()));
    }

    #[test]
    fn a_phone_on_the_same_lan_may_adopt_a_fresh_hub() {
        // THE CASE THIS WHOLE MODULE EXISTS FOR: a headless Pi and a phone on the boat's network.
        assert_eq!(may_set_up(ip("192.168.8.77"), &local(), FRESH, ADOPTION_WINDOW), Ok(()));
    }

    #[test]
    fn a_dual_stack_peer_is_the_same_host_not_a_different_one() {
        // An axum listener on `::` reports LAN peers as ::ffff:a.b.c.d. Reading that as "not IPv4"
        // would refuse every phone on a dual-stack network.
        assert_eq!(may_set_up(ip("::ffff:192.168.8.77"), &local(), FRESH, ADOPTION_WINDOW), Ok(()));
        assert_eq!(
            may_set_up(ip("::ffff:10.9.9.9"), &local(), FRESH, ADOPTION_WINDOW),
            Err(AdoptRefusal::OffNetwork)
        );
    }

    #[test]
    fn a_private_address_on_a_different_subnet_is_still_off_network() {
        // Deliberately STRICTER than the Shelly ingest's RFC1918 check. "Private" includes a guest
        // VLAN, a marina's whole 10/8, and anything reached through a router — none of which is the
        // "my phone is on this boat" case the door was opened for.
        for elsewhere in ["10.0.0.5", "172.16.4.9", "192.168.9.77", "100.100.1.1"] {
            assert_eq!(
                may_set_up(ip(elsewhere), &local(), FRESH, ADOPTION_WINDOW),
                Err(AdoptRefusal::OffNetwork),
                "{elsewhere} is not on the hub's own /24"
            );
        }
    }

    #[test]
    fn a_public_address_never_gets_near_the_door() {
        assert_eq!(may_set_up(ip("8.8.8.8"), &local(), FRESH, ADOPTION_WINDOW), Err(AdoptRefusal::OffNetwork));
        assert_eq!(
            may_set_up(ip("2606:4700::1111"), &local(), FRESH, ADOPTION_WINDOW),
            Err(AdoptRefusal::OffNetwork)
        );
    }

    #[test]
    fn the_window_closes_and_says_how_to_reopen_it() {
        // The bound that makes this acceptable on a marina network. Note the refusal is a DIFFERENT
        // one from off-network: the person on the LAN can fix theirs by restarting the service, and
        // the message has to tell them that rather than saying a flat "forbidden".
        assert_eq!(
            may_set_up(ip("192.168.8.77"), &local(), STALE, ADOPTION_WINDOW),
            Err(AdoptRefusal::WindowClosed)
        );
        assert!(AdoptRefusal::WindowClosed.message().contains("restart"));
        // Exactly at the boundary is still open — `>` not `>=`, so a hub is never refused for being
        // precisely on time.
        assert_eq!(may_set_up(ip("192.168.8.77"), &local(), ADOPTION_WINDOW, ADOPTION_WINDOW), Ok(()));
    }

    #[test]
    fn a_hub_that_knows_none_of_its_own_addresses_admits_only_loopback() {
        // `local_ipv4s()` returns empty on a box with no usable route. Failing CLOSED matters: the
        // alternative reading — "no addresses, so allow anything" — would open the door widest on
        // exactly the machine that understands its network least.
        assert_eq!(may_set_up(ip("192.168.8.77"), &[], FRESH, ADOPTION_WINDOW), Err(AdoptRefusal::OffNetwork));
        assert_eq!(may_set_up(ip("127.0.0.1"), &[], FRESH, ADOPTION_WINDOW), Ok(()));
    }

    #[test]
    fn multi_homed_hubs_honour_every_network_they_are_on() {
        // A boat PC is routinely on the vessel LAN and a starlink/cell subnet at once.
        let ips = vec!["192.168.8.40".to_string(), "172.31.0.105".to_string()];
        assert_eq!(may_set_up(ip("172.31.0.9"), &ips, FRESH, ADOPTION_WINDOW), Ok(()));
        assert_eq!(may_set_up(ip("192.168.8.9"), &ips, FRESH, ADOPTION_WINDOW), Ok(()));
        assert_eq!(may_set_up(ip("172.31.1.9"), &ips, FRESH, ADOPTION_WINDOW), Err(AdoptRefusal::OffNetwork));
    }
}
