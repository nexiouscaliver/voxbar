//! LAN interface enumeration for the companion server.
//!
//! v1.6.0: the picker surfaces EVERY candidate IPv4 with its interface
//! name instead of silently choosing, and Tailscale's CGNAT range
//! (100.64.0.0/10) is a first-class candidate so a phone on the same
//! tailnet can pair even with no shared Wi-Fi. The KB-225 LAN preference
//! stays the default ranking; Tailscale sits between LAN and the
//! corporate-VPN 10/8 tier and is always manually selectable.

use std::net::{IpAddr, Ipv4Addr};

/// The Tailscale CGNAT range as a CIDR, used both for classification and
/// as the pairing-token binding when a Tailscale interface is advertised
/// (tailnet peers sit anywhere in the /10, not in the Mac's /24).
pub const TAILSCALE_CGNAT_SUBNET: &str = "100.64.0.0/10";

/// How a candidate interface address is presented in the settings picker.
/// Drives the label next to the interface name and the default ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterfaceKind {
    /// RFC 1918 Wi-Fi/Ethernet (en0) and bridges over them (bridge100).
    Lan,
    /// The Tailscale CGNAT range 100.64.0.0/10 (usually a utun interface).
    Tailscale,
    /// The 10/8 corporate-VPN giveaway (KB-225).
    CorporateVpn,
}

impl InterfaceKind {
    /// Stable wire string for the settings panel (CompanionInterface.kind).
    pub fn as_str(self) -> &'static str {
        match self {
            InterfaceKind::Lan => "lan",
            InterfaceKind::Tailscale => "tailscale",
            InterfaceKind::CorporateVpn => "vpn",
        }
    }
}

/// One candidate the settings picker can surface: an interface name plus
/// the IPv4 it carries (an interface with several addresses yields one
/// candidate per address).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceCandidate {
    pub ip: Ipv4Addr,
    /// OS interface name (en0, utun9, bridge100).
    pub name: String,
    pub kind: InterfaceKind,
}

/// True for addresses the server accepts as peers: RFC 1918 private
/// ranges, loopback (same-machine only, useful for tests), and the
/// Tailscale CGNAT range (v1.6.0: a phone on the same tailnet).
pub fn is_private_lan(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(*v4),
        IpAddr::V6(_) => false,
    }
}

fn is_private_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 127.0.0.0/8 (loopback,
    // same-machine only), and 100.64.0.0/10 (Tailscale CGNAT; the peer
    // gate below is range-based, so a tailnet peer passes wherever the
    // listener is bound).
    o[0] == 10
        || (o[0] == 172 && (16..=31).contains(&o[1]))
        || (o[0] == 192 && o[1] == 168)
        || o[0] == 127
        || is_tailscale_cgnat(ip)
}

/// True inside the Tailscale CGNAT range 100.64.0.0/10 (RFC 6598 shared
/// address space; on a Mac with Tailscale installed this is the tailnet
/// address range).
pub fn is_tailscale_cgnat(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (64..=127).contains(&o[1])
}

/// Classify a candidate address for the picker. Loopback and public
/// addresses classify as None and never become candidates.
pub fn classify_v4(ip: Ipv4Addr) -> Option<InterfaceKind> {
    let o = ip.octets();
    if (o[0] == 192 && o[1] == 168) || (o[0] == 172 && (16..=31).contains(&o[1])) {
        Some(InterfaceKind::Lan)
    } else if is_tailscale_cgnat(ip) {
        Some(InterfaceKind::Tailscale)
    } else if o[0] == 10 {
        Some(InterfaceKind::CorporateVpn)
    } else {
        None
    }
}

/// Enumerate this machine's candidate IPv4 addresses with their interface
/// names. Real getifaddrs on macOS/Linux (the supported companion
/// platforms this cycle); an empty list elsewhere, which reports the
/// feature unavailable rather than guessing.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn interface_candidates() -> Vec<InterfaceCandidate> {
    use libc::{freeifaddrs, getifaddrs, ifaddrs, sockaddr_in, AF_INET};
    use std::ffi::CStr;

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
                let name = if ifa.ifa_name.is_null() {
                    String::new()
                } else {
                    unsafe { CStr::from_ptr(ifa.ifa_name) }
                        .to_string_lossy()
                        .into_owned()
                };
                if let Some(kind) = classify_v4(ip) {
                    out.push(InterfaceCandidate { ip, name, kind });
                }
            }
        }
        cursor = ifa.ifa_next;
    }
    unsafe { freeifaddrs(ifap) };
    out
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn interface_candidates() -> Vec<InterfaceCandidate> {
    Vec::new()
}

/// The KB-225 default ranking, extended for v1.6.0: physical LAN wins,
/// Tailscale outranks the corporate-VPN 10/8 tier, and within one tier
/// the first-found interface keeps winning (the pre-v1.6.0 tiebreak).
/// The ranking only picks the DEFAULT; every candidate, Tailscale
/// included, is selectable in the settings picker.
fn rank(kind: InterfaceKind) -> u8 {
    match kind {
        InterfaceKind::Lan => 0,
        InterfaceKind::Tailscale => 1,
        InterfaceKind::CorporateVpn => 2,
    }
}

/// The interface advertised when no explicit choice is stored: the best
/// candidate by the ranking above.
pub fn pick_interface(candidates: &[InterfaceCandidate]) -> Option<Ipv4Addr> {
    candidates
        .iter()
        // min_by_key keeps the FIRST of equal keys, preserving the
        // first-found order within a tier.
        .min_by_key(|c| rank(c.kind))
        .map(|c| c.ip)
}

/// Resolve the address the server binds and advertises. `chosen` is the
/// persisted `companion_bind_ip` setting: None/empty means the automatic
/// ranking (the pre-v1.6.0 behavior); an IPv4 must be one of the
/// currently enumerated candidates, so a choice whose interface went
/// away fails loudly instead of silently rebinding elsewhere.
pub fn resolve_bind_ip(
    candidates: &[InterfaceCandidate],
    chosen: Option<&str>,
) -> Result<Ipv4Addr, String> {
    match chosen.map(str::trim).filter(|s| !s.is_empty()) {
        None => pick_interface(candidates)
            .ok_or_else(|| "No private Wi-Fi interface found to advertise".to_string()),
        Some(s) => match s.parse::<Ipv4Addr>() {
            Ok(ip) => candidates
                .iter()
                .find(|c| c.ip == ip)
                .map(|c| c.ip)
                .ok_or_else(|| {
                    format!("Chosen companion interface {s} is not currently present on this Mac")
                }),
            Err(_) => Err(format!(
                "Chosen companion interface {s} is not a valid IPv4 address"
            )),
        },
    }
}

/// The subnet the pairing token is issued for when advertising `ip`: the
/// whole CGNAT /10 for a Tailscale interface (tailnet peers sit anywhere
/// in it), the /24 otherwise (unchanged pre-v1.6.0 binding).
pub fn pairing_subnet_for(ip: Ipv4Addr) -> String {
    if is_tailscale_cgnat(ip) {
        TAILSCALE_CGNAT_SUBNET.to_string()
    } else {
        super::auth::subnet_of(ip)
    }
}

/// Validate the manual advertised-host override: an IPv4 literal or a
/// plain hostname (dot-separated labels of letters/digits/hyphens). No
/// scheme, port, slash, or whitespace can pass, so the value can be
/// interpolated into the pairing URL without re-parsing.
pub fn is_valid_advertised_host(raw: &str) -> bool {
    let s = raw.trim();
    if s.is_empty() || s.len() > 253 {
        return false;
    }
    if s.parse::<Ipv4Addr>().is_ok() {
        return true;
    }
    s.split('.').all(|label| {
        (1..=63).contains(&label.len())
            && label
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphanumeric())
            && label
                .bytes()
                .last()
                .is_some_and(|b| b.is_ascii_alphanumeric())
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    fn cand(a: u8, b: u8, c: u8, d: u8, name: &str) -> InterfaceCandidate {
        InterfaceCandidate {
            ip: Ipv4Addr::new(a, b, c, d),
            name: name.to_string(),
            kind: classify_v4(Ipv4Addr::new(a, b, c, d)).expect("test candidates must classify"),
        }
    }

    #[test]
    fn private_ranges_are_accepted() {
        assert!(is_private_lan(&v4(10, 1, 2, 3)));
        assert!(is_private_lan(&v4(172, 16, 0, 1)));
        assert!(is_private_lan(&v4(172, 31, 255, 254)));
        assert!(is_private_lan(&v4(192, 168, 1, 42)));
        assert!(is_private_lan(&v4(127, 0, 0, 1)));
        // v1.6.0: a phone on the tailnet peers from the CGNAT range.
        assert!(is_private_lan(&v4(100, 101, 7, 24)));
    }

    #[test]
    fn public_and_shared_ranges_are_refused() {
        assert!(!is_private_lan(&v4(8, 8, 8, 8)));
        assert!(!is_private_lan(&v4(172, 32, 0, 1)));
        assert!(!is_private_lan(&v4(172, 15, 255, 255)));
        assert!(!is_private_lan(&v4(169, 254, 1, 1)));
        // The CGNAT range ends at 100.127.255.255; 100.128+ is public.
        assert!(!is_private_lan(&v4(100, 63, 255, 255)));
        assert!(!is_private_lan(&v4(100, 128, 0, 1)));
        assert!(!is_private_lan(&IpAddr::from([
            0xfe80, 0, 0, 0, 0, 0, 0, 1
        ])));
    }

    /// The v1.6.0 classification table, in the phone_visibility! mold:
    /// one list generates BOTH the exhaustive `wire_of` match (no
    /// wildcard arm, so extending InterfaceKind breaks compilation of
    /// this test until the new kind consciously joins the table) AND the
    /// sample array the agreement loop iterates. Each row pins
    /// representative (plus boundary) addresses per class, including
    /// both edges of the CGNAT /10.
    #[test]
    fn every_interface_kind_has_a_deliberate_classification_table() {
        macro_rules! classification_table {
            (
                // Each expr is followed by a comma inside the repetition
                // (an expr fragment may only be followed by => , ; in a
                // macro matcher), so the sample lists carry trailing
                // commas in the invocation below.
                none [ $( $none:expr, )+ ]
                $( $kind:ident as $str:expr => [ $( $ip:expr, )+ ] $(,)? )+
            ) => {
                // Exhaustive over InterfaceKind: no wildcard arm, so a
                // new variant fails compilation until it joins the table.
                fn wire_of(kind: InterfaceKind) -> &'static str {
                    match kind {
                        $( InterfaceKind::$kind => $str, )+
                    }
                }
                fn samples() -> Vec<(InterfaceKind, &'static str, Ipv4Addr)> {
                    let mut v = Vec::new();
                    $( v.extend([ $( $ip ),* ].into_iter()
                        .map(|ip| (InterfaceKind::$kind, $str, ip))); )+
                    v
                }
                fn none_samples() -> Vec<Ipv4Addr> {
                    vec![ $( $none ),* ]
                }
            };
        }

        classification_table! {
            none [
                Ipv4Addr::new(127, 0, 0, 1),      // loopback: never a candidate
                Ipv4Addr::new(8, 8, 8, 8),        // public
                Ipv4Addr::new(169, 254, 1, 1),    // link-local
                Ipv4Addr::new(100, 63, 255, 255), // one below the CGNAT /10
                Ipv4Addr::new(100, 128, 0, 1),    // one above the CGNAT /10
                Ipv4Addr::new(172, 32, 0, 1),     // one above 172.16/12
            ]
            Lan as "lan" => [
                Ipv4Addr::new(192, 168, 1, 42),
                Ipv4Addr::new(172, 16, 0, 1),
                Ipv4Addr::new(172, 31, 255, 254),
            ]
            Tailscale as "tailscale" => [
                Ipv4Addr::new(100, 64, 0, 1),      // first address of the /10
                Ipv4Addr::new(100, 101, 7, 24),    // typical tailnet address
                Ipv4Addr::new(100, 127, 255, 254), // last address of the /10
            ]
            CorporateVpn as "vpn" => [
                Ipv4Addr::new(10, 20, 0, 5),
                Ipv4Addr::new(10, 0, 30, 2),
            ]
        }

        for (kind, wire, ip) in samples() {
            assert_eq!(
                classify_v4(ip),
                Some(kind),
                "{ip}: classified wrong for {wire}"
            );
            assert_eq!(kind.as_str(), wire, "wire string must match the table");
            assert_eq!(wire_of(kind), wire);
        }
        for ip in none_samples() {
            assert_eq!(classify_v4(ip), None, "{ip}: must never be a candidate");
        }
    }

    #[test]
    fn enumerated_candidates_are_classified_and_named() {
        for c in interface_candidates() {
            assert_eq!(
                c.kind,
                classify_v4(c.ip).expect("enumerated candidates classify")
            );
            assert!(!c.ip.is_loopback());
            assert!(!c.name.is_empty(), "getifaddrs always carries a name");
        }
    }

    // KB-225 (kept) + v1.6.0 Tailscale tier: physical LAN ranges outrank
    // Tailscale, which outranks the corporate-VPN 10/8 range.

    #[test]
    fn lan_outranks_tailscale_even_when_tailscale_is_first() {
        let picked =
            pick_interface(&[cand(100, 101, 7, 24, "utun9"), cand(192, 168, 1, 42, "en0")]);
        assert_eq!(picked, Some(Ipv4Addr::new(192, 168, 1, 42)));
    }

    #[test]
    fn tailscale_outranks_the_corporate_vpn_range() {
        let picked = pick_interface(&[cand(10, 20, 0, 5, "utun3"), cand(100, 101, 7, 24, "utun9")]);
        assert_eq!(picked, Some(Ipv4Addr::new(100, 101, 7, 24)));
    }

    #[test]
    fn vpn_range_loses_to_192_168_even_when_first() {
        let picked = pick_interface(&[cand(10, 20, 0, 5, "utun3"), cand(192, 168, 1, 42, "en0")]);
        assert_eq!(picked, Some(Ipv4Addr::new(192, 168, 1, 42)));
    }

    #[test]
    fn vpn_range_is_chosen_when_sole_candidate() {
        // A home network that itself runs 10.x: still the only show in town.
        assert_eq!(
            pick_interface(&[cand(10, 0, 0, 7, "en0")]),
            Some(Ipv4Addr::new(10, 0, 0, 7))
        );
    }

    #[test]
    fn tailscale_is_chosen_when_sole_candidate() {
        assert_eq!(
            pick_interface(&[cand(100, 101, 7, 24, "utun9")]),
            Some(Ipv4Addr::new(100, 101, 7, 24))
        );
    }

    #[test]
    fn same_tier_keeps_first_found_order() {
        // Tiebreak inside one tier: enumeration order, as before KB-225.
        assert_eq!(
            pick_interface(&[cand(192, 168, 1, 10, "en0"), cand(192, 168, 2, 20, "en1"),]),
            Some(Ipv4Addr::new(192, 168, 1, 10))
        );
    }

    #[test]
    fn loopback_and_public_are_never_picked() {
        assert_eq!(pick_interface(&[]), None);
    }

    // resolve_bind_ip: the settings seam between the persisted choice and
    // the address the server binds.

    #[test]
    fn resolve_defaults_to_the_ranked_pick() {
        let candidates = [
            cand(10, 20, 0, 5, "utun3"),
            cand(192, 168, 1, 42, "en0"),
            cand(100, 101, 7, 24, "utun9"),
        ];
        assert_eq!(
            resolve_bind_ip(&candidates, None).unwrap(),
            Ipv4Addr::new(192, 168, 1, 42)
        );
        assert_eq!(
            resolve_bind_ip(&candidates, Some("")).unwrap(),
            Ipv4Addr::new(192, 168, 1, 42),
            "an empty stored choice means automatic"
        );
    }

    #[test]
    fn resolve_honors_a_present_chosen_interface_including_tailscale() {
        let candidates = [cand(192, 168, 1, 42, "en0"), cand(100, 101, 7, 24, "utun9")];
        assert_eq!(
            resolve_bind_ip(&candidates, Some("100.101.7.24")).unwrap(),
            Ipv4Addr::new(100, 101, 7, 24),
            "the Tailscale interface is selectable, not just rankable"
        );
        assert_eq!(
            resolve_bind_ip(&candidates, Some(" 192.168.1.42 ")).unwrap(),
            Ipv4Addr::new(192, 168, 1, 42),
            "a stored choice is trimmed before matching"
        );
    }

    #[test]
    fn resolve_fails_loudly_when_the_choice_is_missing_or_invalid() {
        let candidates = [cand(192, 168, 1, 42, "en0")];
        let missing = resolve_bind_ip(&candidates, Some("100.101.7.24")).unwrap_err();
        assert!(
            missing.contains("not currently present"),
            "missing interface error: {missing}"
        );
        let garbage = resolve_bind_ip(&candidates, Some("https://mac:4177")).unwrap_err();
        assert!(
            garbage.contains("not a valid IPv4 address"),
            "garbage error: {garbage}"
        );
        assert!(resolve_bind_ip(&[], None).is_err());
    }

    /// The pairing-token binding: a Tailscale bind issues the token for
    /// the whole CGNAT /10 (tailnet peers sit outside any one /24); a
    /// LAN bind keeps the pre-v1.6.0 /24 exactly.
    #[test]
    fn pairing_subnet_widens_to_the_cgnat_slash_10_for_tailscale() {
        assert_eq!(
            pairing_subnet_for(Ipv4Addr::new(100, 101, 7, 24)),
            "100.64.0.0/10"
        );
        assert_eq!(
            pairing_subnet_for(Ipv4Addr::new(192, 168, 1, 42)),
            "192.168.1.0/24"
        );
        assert_eq!(
            pairing_subnet_for(Ipv4Addr::new(10, 20, 0, 5)),
            "10.20.0.0/24"
        );
        // And the /10 binding actually validates a far-away tailnet peer
        // through the existing auth check.
        assert!(super::super::auth::same_subnet(
            IpAddr::V4(Ipv4Addr::new(100, 127, 200, 9)),
            &pairing_subnet_for(Ipv4Addr::new(100, 64, 1, 2))
        ));
        assert!(!super::super::auth::same_subnet(
            IpAddr::V4(Ipv4Addr::new(192, 168, 2, 9)),
            &pairing_subnet_for(Ipv4Addr::new(192, 168, 1, 2))
        ));
    }

    // Manual advertised-host validation.

    #[test]
    fn advertised_host_accepts_ips_and_plain_hostnames() {
        assert!(is_valid_advertised_host("192.168.1.42"));
        assert!(is_valid_advertised_host("100.101.7.24"));
        assert!(is_valid_advertised_host("localhost"));
        assert!(is_valid_advertised_host("mac.tail1234.ts.net"));
        assert!(is_valid_advertised_host("  padded.host  "), "trimmed first");
    }

    #[test]
    fn advertised_host_refuses_urls_ports_and_garbage() {
        assert!(!is_valid_advertised_host(""));
        assert!(!is_valid_advertised_host("   "));
        assert!(!is_valid_advertised_host("https://mac.tailnet.ts.net"));
        assert!(!is_valid_advertised_host("mac.tailnet.ts.net:4177"));
        assert!(!is_valid_advertised_host("mac tailnet"));
        assert!(!is_valid_advertised_host("-leading-hyphen.example"));
        assert!(!is_valid_advertised_host("trailing-.example"));
        assert!(!is_valid_advertised_host("bad_underscore.example"));
        assert!(!is_valid_advertised_host(&"a".repeat(254)));
    }
}
