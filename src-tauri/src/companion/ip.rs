//! LAN interface enumeration for the companion server.

use std::net::{IpAddr, Ipv4Addr};

/// True for RFC 1918 private ranges (the only ranges the server binds and
/// the only peers it accepts besides loopback, which is local-only and
/// useful for tests).
pub fn is_private_lan(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(*v4),
        IpAddr::V6(_) => false,
    }
}

fn is_private_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 127.0.0.0/8 (loopback,
    // same-machine only).
    o[0] == 10
        || (o[0] == 172 && (16..=31).contains(&o[1]))
        || (o[0] == 192 && o[1] == 168)
        || o[0] == 127
}

/// Enumerate this machine's private IPv4 addresses. Real getifaddds on
/// macOS/Linux (the supported companion platforms this cycle); an empty
/// list elsewhere, which reports the feature unavailable rather than
/// guessing.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn private_ipv4_addresses() -> Vec<Ipv4Addr> {
    use libc::{freeifaddrs, getifaddrs, ifaddrs, sockaddr_in, AF_INET};

    let mut ifap: *mut ifaddrs = std::ptr::null_mut();
    if unsafe { getifaddrs(&mut ifap) } != 0 {
        log::warn!("companion: getifaddrs failed");
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut cursor = ifap;
    while !cursor.is_null() {
        let ifa = unsafe { &*cursor };
        if !ifa.ifa_addr.is_null() {
            let family = unsafe { (*ifa.ifa_addr).sa_family } as i32;
            if family == AF_INET {
                let sin = unsafe { &*(ifa.ifa_addr as *const sockaddr_in) };
                // sin_addr.s_addr sits in memory in network byte order;
                // to_ne_bytes hands those bytes back in order.
                let b = sin.sin_addr.s_addr.to_ne_bytes();
                let ip = Ipv4Addr::new(b[0], b[1], b[2], b[3]);
                if is_private_v4(ip) && !ip.is_loopback() {
                    out.push(ip);
                }
            }
        }
        cursor = ifa.ifa_next;
    }
    unsafe { freeifaddrs(ifap) };
    out
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn private_ipv4_addresses() -> Vec<Ipv4Addr> {
    Vec::new()
}

/// The interface the QR code advertises: the best private LAN IPv4 by the
/// KB-225 ranking below. The pairing token is bound to this interface's /24.
pub fn advertised_lan_ipv4() -> Option<Ipv4Addr> {
    pick_lan_ipv4(&private_ipv4_addresses())
}

/// KB-225: rank candidate private IPv4s so physical LAN interfaces win over
/// VPN-ish ranges when both are up. With a corporate VPN (10/8) connected,
/// first-found advertising bound the TLS server to the VPN IP while the
/// user's phone sat on home Wi-Fi on a different subnet: reachable by every
/// corporate peer, refused by the phone.
///
/// Heuristic (no interface metadata on plain std, so no interface-name
/// sniffing): 10/8 is the classic corporate-VPN range while home routers
/// almost always hand out 192.168 (sometimes 172.16-31), so non-10/8
/// candidates outrank 10/8. Within the same tier, first found still wins.
///
/// Limitations, honestly: a home network that itself runs 10.x is only
/// selected when it is the sole candidate (with a 192.168 also present it
/// loses), and a VPN handing out 192.168.x cannot be distinguished from
/// real Wi-Fi by address alone - interface-name sniffing (utun/tun/tap) is
/// the future fix if this ever bites.
fn pick_lan_ipv4(candidates: &[Ipv4Addr]) -> Option<Ipv4Addr> {
    candidates
        .iter()
        .copied()
        .filter(|ip| is_private_v4(*ip) && !ip.is_loopback())
        // min_by_key keeps the FIRST of equal keys, preserving the
        // first-found order within a tier.
        .min_by_key(|ip| u8::from(is_vpn_ish(*ip)))
}

/// KB-225: true for the 10/8 range, the classic corporate-VPN giveaway.
fn is_vpn_ish(ip: Ipv4Addr) -> bool {
    ip.octets()[0] == 10
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn private_ranges_are_accepted() {
        assert!(is_private_lan(&v4(10, 1, 2, 3)));
        assert!(is_private_lan(&v4(172, 16, 0, 1)));
        assert!(is_private_lan(&v4(172, 31, 255, 254)));
        assert!(is_private_lan(&v4(192, 168, 1, 42)));
        assert!(is_private_lan(&v4(127, 0, 0, 1)));
    }

    #[test]
    fn public_and_shared_ranges_are_refused() {
        assert!(!is_private_lan(&v4(8, 8, 8, 8)));
        assert!(!is_private_lan(&v4(172, 32, 0, 1)));
        assert!(!is_private_lan(&v4(172, 15, 255, 255)));
        assert!(!is_private_lan(&v4(169, 254, 1, 1)));
        assert!(!is_private_lan(&IpAddr::from([
            0xfe80, 0, 0, 0, 0, 0, 0, 1
        ])));
    }

    #[test]
    fn enumerated_addresses_are_private_and_loopback_free() {
        for ip in private_ipv4_addresses() {
            assert!(is_private_v4(ip));
            assert!(!ip.is_loopback());
        }
    }

    // KB-225: physical LAN ranges outrank the corporate-VPN 10/8 range.

    #[test]
    fn vpn_range_loses_to_192_168_even_when_first() {
        let picked = pick_lan_ipv4(&[
            Ipv4Addr::new(10, 20, 0, 5),
            Ipv4Addr::new(192, 168, 1, 42),
        ]);
        assert_eq!(picked, Some(Ipv4Addr::new(192, 168, 1, 42)));
    }

    #[test]
    fn vpn_range_loses_to_172_16_31_even_when_first() {
        let picked = pick_lan_ipv4(&[
            Ipv4Addr::new(10, 0, 30, 2),
            Ipv4Addr::new(172, 20, 0, 9),
        ]);
        assert_eq!(picked, Some(Ipv4Addr::new(172, 20, 0, 9)));
    }

    #[test]
    fn vpn_range_is_chosen_when_sole_candidate() {
        // A home network that itself runs 10.x: still the only show in town.
        assert_eq!(
            pick_lan_ipv4(&[Ipv4Addr::new(10, 0, 0, 7)]),
            Some(Ipv4Addr::new(10, 0, 0, 7))
        );
    }

    #[test]
    fn same_tier_keeps_first_found_order() {
        // Tiebreak inside one tier: enumeration order, as before KB-225.
        assert_eq!(
            pick_lan_ipv4(&[
                Ipv4Addr::new(192, 168, 1, 10),
                Ipv4Addr::new(192, 168, 2, 20),
            ]),
            Some(Ipv4Addr::new(192, 168, 1, 10))
        );
    }

    #[test]
    fn loopback_and_public_are_never_picked() {
        assert_eq!(pick_lan_ipv4(&[Ipv4Addr::new(127, 0, 0, 1)]), None);
        assert_eq!(pick_lan_ipv4(&[Ipv4Addr::new(8, 8, 8, 8)]), None);
        assert_eq!(pick_lan_ipv4(&[]), None);
    }
}
