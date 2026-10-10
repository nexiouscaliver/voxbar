//! Pairing trust: token generation and constant-time comparison, subnet
//! binding, per-IP failure rate limiting. All pure logic; unit-tested
//! without a socket in sight.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};

/// A fresh 128-bit pairing token, hex-encoded (32 characters).
pub fn generate_pairing_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("system entropy is available");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time token comparison. Both sides are fixed-length hex strings,
/// so only the content folds into the comparison; the result is decided
/// from a single accumulated diff, never an early return per byte.
pub fn token_matches(expected: &str, provided: &str) -> bool {
    let (e, p) = (expected.as_bytes(), provided.as_bytes());
    if e.len() != p.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..e.len() {
        diff |= e[i] ^ p[i];
    }
    diff == 0
}

/// The /24 a token is issued for, as "a.b.c.0/24".
pub fn subnet_of(ip: Ipv4Addr) -> String {
    let o = ip.octets();
    format!("{}.{}.{}.0/24", o[0], o[1], o[2])
}

/// True when `peer` falls inside `subnet` ("a.b.c.0/24"). Anything
/// unparseable refuses (fail closed).
pub fn same_subnet(peer: IpAddr, subnet: &str) -> bool {
    let IpAddr::V4(peer) = peer else {
        return false;
    };
    let Some((net, prefix)) = subnet.split_once('/') else {
        return false;
    };
    let Ok(prefix) = prefix.parse::<u8>() else {
        return false;
    };
    if prefix > 32 {
        return false;
    }
    let Ok(net) = net.parse::<Ipv4Addr>() else {
        return false;
    };
    if prefix == 0 {
        return true;
    }
    let mask: u32 = if prefix == 32 {
        u32::MAX
    } else {
        u32::MAX << (32 - prefix)
    };
    u32::from(net) & mask == u32::from(peer) & mask
}

/// Per-IP auth-failure limiter: after `max` failures inside `window`, the IP
/// is refused for `cooldown` before any further attempt is even parsed.
pub struct FailureLimiter {
    max: u32,
    window: Duration,
    cooldown: Duration,
    failures: HashMap<IpAddr, Vec<Instant>>,
}

impl FailureLimiter {
    pub fn new(max: u32, window: Duration, cooldown: Duration) -> Self {
        FailureLimiter {
            max,
            window,
            cooldown,
            failures: HashMap::new(),
        }
    }

    /// Whether this IP is currently blocked. Prunes expired entries.
    pub fn is_blocked(&mut self, ip: IpAddr, now: Instant) -> bool {
        let Some(events) = self.failures.get_mut(&ip) else {
            return false;
        };
        events.retain(|t| now.duration_since(*t) < self.window.max(self.cooldown));
        // Blocked when the failures inside the blocking window (cooldown,
        // the longer of the two) reached the max.
        let recent = events
            .iter()
            .filter(|t| now.duration_since(**t) < self.cooldown)
            .count();
        recent >= self.max as usize
    }

    pub fn register_failure(&mut self, ip: IpAddr, now: Instant) {
        let events = self.failures.entry(ip).or_default();
        events.retain(|t| now.duration_since(*t) < self.window.max(self.cooldown));
        events.push(now);
    }

    pub fn clear(&mut self, ip: IpAddr) {
        self.failures.remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn tokens_are_128_bit_hex() {
        let token = generate_pairing_token();
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(token, generate_pairing_token());
    }

    #[test]
    fn token_comparison_matches_and_rejects() {
        let token = generate_pairing_token();
        assert!(token_matches(&token, &token));
        assert!(!token_matches(&token, &token[..31]));
        assert!(!token_matches(&token, &format!("{token}x")));
        let mut flipped = token.clone();
        // Flip one hex digit.
        flipped.replace_range(0..1, if &flipped[0..1] == "0" { "1" } else { "0" });
        assert_ne!(token, flipped);
        assert!(!token_matches(&token, &flipped));
    }

    #[test]
    fn subnet_of_masks_the_host_octet() {
        assert_eq!(subnet_of(Ipv4Addr::new(192, 168, 1, 77)), "192.168.1.0/24");
        assert_eq!(subnet_of(Ipv4Addr::new(10, 0, 0, 1)), "10.0.0.0/24");
    }

    #[test]
    fn same_subnet_accepts_peers_in_the_bound_range_only() {
        let subnet = subnet_of(Ipv4Addr::new(192, 168, 1, 5));
        assert!(same_subnet(ip(192, 168, 1, 1), &subnet));
        assert!(same_subnet(ip(192, 168, 1, 254), &subnet));
        assert!(!same_subnet(ip(192, 168, 2, 1), &subnet));
        assert!(!same_subnet(ip(10, 0, 0, 1), &subnet));
        // Fail closed on garbage.
        assert!(!same_subnet(ip(192, 168, 1, 1), "not a subnet"));
        assert!(!same_subnet(ip(192, 168, 1, 1), "192.168.1.0/33"));
        assert!(!same_subnet(ip(192, 168, 1, 1), "192.168.1/24"));
    }

    #[test]
    fn failure_limiter_blocks_after_burst_and_recovers() {
        let mut limiter = FailureLimiter::new(3, Duration::from_secs(60), Duration::from_secs(30));
        let peer = ip(192, 168, 1, 50);
        let t0 = Instant::now();

        assert!(!limiter.is_blocked(peer, t0));
        limiter.register_failure(peer, t0);
        limiter.register_failure(peer, t0 + Duration::from_secs(1));
        assert!(!limiter.is_blocked(peer, t0 + Duration::from_secs(2)));
        limiter.register_failure(peer, t0 + Duration::from_secs(2));
        assert!(limiter.is_blocked(peer, t0 + Duration::from_secs(2)));

        // A different IP is unaffected.
        assert!(!limiter.is_blocked(ip(192, 168, 1, 51), t0 + Duration::from_secs(2)));

        // The cooldown elapses and the IP may try again.
        assert!(!limiter.is_blocked(peer, t0 + Duration::from_secs(35)));

        // A success clears the slate entirely.
        limiter.register_failure(peer, t0 + Duration::from_secs(40));
        limiter.register_failure(peer, t0 + Duration::from_secs(41));
        limiter.clear(peer);
        assert!(!limiter.is_blocked(peer, t0 + Duration::from_secs(41)));
    }
}
