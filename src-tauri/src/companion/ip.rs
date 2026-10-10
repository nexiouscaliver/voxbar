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

/// The interface the QR code advertises: the first private LAN IPv4. The
/// pairing token is bound to this interface's /24.
pub fn advertised_lan_ipv4() -> Option<Ipv4Addr> {
    private_ipv4_addresses()
        .into_iter()
        .find(|ip| !ip.is_loopback())
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
}
