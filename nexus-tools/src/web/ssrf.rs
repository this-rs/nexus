//! Which addresses a fetch may reach (the SSRF guard, N21).
//!
//! A fetch tool is a request forger in the user's network: pointed at `169.254.169.254` it
//! reads cloud credentials, pointed at `localhost` it reads the developer's own services.
//! The guard classifies every address a name resolves to — **all** of them, since an
//! attacker's DNS may return one public and one private record — and refuses the whole
//! fetch if any is private. The connection is then made to the address that was checked
//! (pinned), never to a second resolution.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Why an address is refused, or `None` when it is a public one.
pub fn blocked_reason(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => v4_reason(v4),
        IpAddr::V6(v6) => v6_reason(v6),
    }
}

fn v4_reason(ip: Ipv4Addr) -> Option<&'static str> {
    let [a, b, c, _] = ip.octets();
    Some(match (a, b, c) {
        (0, ..) => "this-network address (0.0.0.0/8)",
        (10, ..) => "private network (10.0.0.0/8)",
        (100, 64..=127, _) => "shared address space / carrier-grade NAT (100.64.0.0/10)",
        (127, ..) => "loopback (127.0.0.0/8)",
        (169, 254, _) => "link-local (169.254.0.0/16, includes cloud metadata services)",
        (172, 16..=31, _) => "private network (172.16.0.0/12)",
        (192, 0, 0) => "IETF protocol assignments (192.0.0.0/24)",
        (192, 0, 2) | (198, 51, 100) | (203, 0, 113) => "documentation range",
        (192, 168, _) => "private network (192.168.0.0/16)",
        (198, 18..=19, _) => "benchmarking (198.18.0.0/15)",
        (224..=239, ..) => "multicast",
        (240..=255, ..) => "reserved / broadcast (240.0.0.0/4)",
        _ => return None,
    })
}

fn v6_reason(ip: Ipv6Addr) -> Option<&'static str> {
    // An IPv4 address wearing an IPv6 coat is judged as the IPv4 address it is.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return v4_reason(v4).map(|_| "IPv4-mapped IPv6 address of a non-public IPv4 address");
    }
    let segments = ip.segments();
    let octets = ip.octets();
    if ip.is_unspecified() {
        return Some("unspecified address (::)");
    }
    if ip.is_loopback() {
        return Some("loopback (::1)");
    }
    // ::a.b.c.d (deprecated IPv4-compatible) and NAT64 64:ff9b::/96 embed an IPv4 address.
    if segments[..6] == [0; 6] || segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        let v4 = Ipv4Addr::new(octets[12], octets[13], octets[14], octets[15]);
        return Some(if v4_reason(v4).is_some() {
            "IPv6 form of a non-public IPv4 address"
        } else {
            "IPv4-compatible / NAT64 address (not routable for a fetch)"
        });
    }
    // 6to4 (2002::/16) embeds the IPv4 address in bits 16..48.
    if segments[0] == 0x2002 {
        let v4 = Ipv4Addr::new(octets[2], octets[3], octets[4], octets[5]);
        return Some(if v4_reason(v4).is_some() {
            "6to4 address of a non-public IPv4 address"
        } else {
            "6to4 tunnel address"
        });
    }
    Some(match segments[0] {
        0xfe80..=0xfebf => "link-local (fe80::/10)",
        0xfec0..=0xfeff => "site-local (fec0::/10, deprecated)",
        0xfc00..=0xfdff => "unique local address (fc00::/7)",
        0xff00..=0xffff => "multicast",
        0x2001 if segments[1] == 0 => "Teredo tunnel (2001::/32)",
        0x2001 if segments[1] == 0x0db8 => "documentation range (2001:db8::/32)",
        0x0100 if segments[1..4] == [0, 0, 0] => "discard-only (100::/64)",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(text: &str) -> bool {
        blocked_reason(text.parse().unwrap()).is_some()
    }

    #[test]
    fn private_loopback_link_local_and_metadata_ipv4_are_refused() {
        for ip in [
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.1",
            "127.0.0.1",
            "127.255.255.254",
            "169.254.169.254",
            "169.254.0.1",
            "0.0.0.0",
            "100.64.0.1",
            "100.127.255.255",
            "192.0.0.1",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "240.0.0.1",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
        ] {
            assert!(blocked(ip), "{ip} should be refused");
        }
    }

    #[test]
    fn ordinary_public_ipv4_is_allowed_and_the_edges_of_ranges_are_exact() {
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "172.15.255.255",
            "172.32.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "169.253.255.255",
            "169.255.0.1",
            "192.167.255.255",
            "192.169.0.1",
            "198.17.255.255",
            "198.20.0.1",
            "223.255.255.255",
        ] {
            assert!(!blocked(ip), "{ip} should be allowed");
        }
    }

    #[test]
    fn ipv6_private_forms_are_refused() {
        for ip in [
            "::1",
            "::",
            "fe80::1",
            "febf::1",
            "fc00::1",
            "fd12:3456::1",
            "ff02::1",
            "fec0::1",
            "2001:db8::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            "100::1",
        ] {
            assert!(blocked(ip), "{ip} should be refused");
        }
        assert!(!blocked("2606:4700:4700::1111"));
        assert!(!blocked("2001:4860:4860::8888"));
    }

    #[test]
    fn an_ipv4_address_in_an_ipv6_coat_is_judged_as_the_ipv4_address() {
        // IPv4-mapped.
        for ip in [
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:7f00:1",
        ] {
            assert!(blocked(ip), "{ip} should be refused");
        }
        // Compatible, NAT64, 6to4: refused whatever they embed.
        for ip in [
            "::7f00:1",
            "::a00:1",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::1",
            "2002:a9fe:a9fe::1",
            "2002:0808:0808::1",
        ] {
            assert!(blocked(ip), "{ip} should be refused");
        }
    }

    #[test]
    fn the_reason_names_the_range() {
        assert!(
            blocked_reason("169.254.169.254".parse().unwrap())
                .unwrap()
                .contains("metadata")
        );
        assert!(
            blocked_reason("::ffff:10.0.0.1".parse().unwrap())
                .unwrap()
                .contains("IPv4-mapped")
        );
    }
}
