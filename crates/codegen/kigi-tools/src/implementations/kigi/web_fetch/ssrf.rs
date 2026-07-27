//! SSRF protection for `web_fetch`.
//!
//! Non-public targets are blocked: loopback, RFC 1918, link-local,
//! CGNAT, TEST-NET, reserved, ULA. Loopback is opt-in via
//! `[toolset.web_fetch] allow_local` or `KIGI_WEB_FETCH_ALLOW_LOCAL`,
//! and even then only for a literal local host.
//!
//! Reference: [IANA IPv4 Special-Purpose Address Registry](https://www.iana.org/assignments/iana-ipv4-special-registry/)

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::Url;

use super::error::WebFetchError;

/// Hosts allowed to reach loopback when local access is on.
///
/// Names that merely RESOLVE to loopback are excluded: DNS rebinding.
pub fn is_explicit_local_host(host: &str) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(&host);
    // Drop an IPv6 zone id such as `fe80::1%lo0`.
    let host = host.split('%').next().unwrap_or(host);

    if host == "localhost" {
        return true;
    }
    host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Whether an IP is not globally routable.
pub fn is_non_public_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_non_public_ipv4(*v4),
        IpAddr::V6(v6) => is_non_public_ipv6(*v6),
    }
}

fn is_non_public_ipv4(ip: Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_broadcast()
        // "This network" (RFC 1122) 0.0.0.0/8
        || ipv4_in_cidr(ip, [0, 0, 0, 0], 8)
        // CGNAT (RFC 6598) — some clouds serve metadata here
        || ipv4_in_cidr(ip, [100, 64, 0, 0], 10)
        // IETF Protocol Assignments (RFC 6890)
        || ipv4_in_cidr(ip, [192, 0, 0, 0], 24)
        // TEST-NET-1 (RFC 5737)
        || ipv4_in_cidr(ip, [192, 0, 2, 0], 24)
        // Benchmarking (RFC 2544)
        || ipv4_in_cidr(ip, [198, 18, 0, 0], 15)
        // TEST-NET-2 / TEST-NET-3
        || ipv4_in_cidr(ip, [198, 51, 100, 0], 24)
        || ipv4_in_cidr(ip, [203, 0, 113, 0], 24)
        // Reserved (RFC 6890)
        || ipv4_in_cidr(ip, [240, 0, 0, 0], 4)
}

fn ipv4_in_cidr(ip: Ipv4Addr, base: [u8; 4], prefix: u8) -> bool {
    debug_assert!(prefix <= 32, "IPv4 prefix out of range");
    let ip = u32::from(ip);
    let base = u32::from(Ipv4Addr::from(base));
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    (ip & mask) == (base & mask)
}

fn is_non_public_ipv6(ip: Ipv6Addr) -> bool {
    // Identity wins: `::1` is not judged as `0.0.0.1`.
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    if let Some(v4) = embedded_ipv4(ip) {
        return is_non_public_ipv4(v4);
    }
    let seg = ip.segments();
    ip.is_unique_local()
        || ip.is_unicast_link_local()
        // Deprecated site-local (RFC 3879) fec0::/10
        || (seg[0] & 0xffc0) == 0xfec0
        // Documentation (RFC 3849) 2001:db8::/32
        || (seg[0] == 0x2001 && seg[1] == 0x0db8)
}

/// IPv4 reachable through a known IPv6 wrapper, if any.
///
/// Covers mapped, compatible, well-known NAT64, and 6to4. Not complete:
/// network-specific NAT64 prefixes (RFC 6052) cannot be enumerated.
fn embedded_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let seg = ip.segments();
    let embedded = |hi: u16, lo: u16| Ipv4Addr::from(u32::from(hi) << 16 | u32::from(lo));

    if seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2..6] == [0, 0, 0, 0] {
        return Some(embedded(seg[6], seg[7]));
    }
    if seg[0] == 0x2002 {
        return Some(embedded(seg[1], seg[2]));
    }
    // Covers `::ffff:a.b.c.d` and the deprecated `::a.b.c.d`.
    ip.to_ipv4()
}

/// Loopback including IPv4-mapped forms like `::ffff:127.0.0.1`.
///
/// `IpAddr::is_loopback` is false for mapped addresses, so the opt-in path
/// cannot use it directly.
fn is_loopback_addr(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// Dual gate: loopback opens only for an explicit local host.
///
/// Private and link-local never open through this flag.
/// Shared with the hook runner: one policy, every outbound URL.
pub fn is_blocked_for_host(ip: &IpAddr, host: &str, allow_local: bool) -> bool {
    if !is_non_public_ip(ip) {
        return false;
    }
    !(allow_local && is_loopback_addr(ip) && is_explicit_local_host(host))
}

/// Verifies no resolved address is blocked by the SSRF policy.
///
/// `allow_local` is config-only so the model cannot flip it.
pub(crate) async fn check_ssrf(url: &Url, allow_local: bool) -> Result<(), WebFetchError> {
    let host = url
        .host_str()
        .ok_or_else(|| WebFetchError::SingleLabelHost {
            host: String::new(),
        })?;

    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_blocked_for_host(&ip, host, allow_local) {
            return Err(WebFetchError::SsrfBlocked {
                host: host.to_string(),
                ip,
            });
        }
        return Ok(());
    }

    let port = url.port_or_known_default().unwrap_or(443);
    let addr_str = format!("{host}:{port}");
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host(&addr_str)
        .await
        .map_err(|e| WebFetchError::DnsResolution {
            host: host.to_string(),
            source: e,
        })?
        .collect();

    if addrs.is_empty() {
        return Err(WebFetchError::DnsEmpty(host.to_string()));
    }

    addrs
        .iter()
        .find(|addr| is_blocked_for_host(&addr.ip(), host, allow_local))
        .map_or(Ok(()), |addr| {
            Err(WebFetchError::SsrfBlocked {
                host: host.to_string(),
                ip: addr.ip(),
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_rfc1918_10x() {
        assert!(is_non_public_ip(&"10.0.0.1".parse().unwrap()));
        assert!(is_non_public_ip(&"10.255.255.255".parse().unwrap()));
    }

    #[test]
    fn blocks_rfc1918_172x() {
        assert!(is_non_public_ip(&"172.16.0.1".parse().unwrap()));
        assert!(is_non_public_ip(&"172.31.255.255".parse().unwrap()));
        assert!(!is_non_public_ip(&"172.15.0.1".parse().unwrap()));
        assert!(!is_non_public_ip(&"172.32.0.1".parse().unwrap()));
    }

    #[test]
    fn blocks_rfc1918_192168() {
        assert!(is_non_public_ip(&"192.168.0.1".parse().unwrap()));
        assert!(is_non_public_ip(&"192.168.255.255".parse().unwrap()));
    }

    #[test]
    fn blocks_link_local() {
        assert!(is_non_public_ip(&"169.254.0.1".parse().unwrap()));
        assert!(is_non_public_ip(&"169.254.169.254".parse().unwrap()));
    }

    #[test]
    fn blocks_cgnat_cloud_metadata() {
        assert!(is_non_public_ip(&"100.64.0.1".parse().unwrap()));
        assert!(is_non_public_ip(&"100.127.255.255".parse().unwrap()));
        assert!(!is_non_public_ip(&"100.63.0.1".parse().unwrap()));
        assert!(!is_non_public_ip(&"100.128.0.1".parse().unwrap()));
    }

    #[test]
    fn blocks_unspecified() {
        assert!(is_non_public_ip(&"0.0.0.0".parse().unwrap()));
        assert!(is_non_public_ip(&"::".parse().unwrap()));
    }

    #[test]
    fn blocks_loopback_by_default() {
        for ip in ["127.0.0.1", "127.0.0.2", "::1", "::ffff:127.0.0.1"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(is_non_public_ip(&ip), "{ip} must not be public");
            assert!(
                is_blocked_for_host(&ip, "localhost", false),
                "{ip} must be blocked without allow_local"
            );
        }
    }

    #[test]
    fn allow_local_opens_loopback_only_for_an_explicit_local_host() {
        for (ip, host) in [
            ("127.0.0.1", "localhost"),
            ("127.0.0.1", "127.0.0.1"),
            ("::1", "::1"),
            ("::ffff:127.0.0.1", "localhost"),
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(!is_blocked_for_host(&ip, host, true), "{ip} via {host}");
        }
        assert!(
            is_blocked_for_host(&"127.0.0.1".parse().unwrap(), "evil.example.com", true),
            "a public name resolving to loopback is DNS rebinding"
        );
    }

    #[test]
    fn allow_local_never_opens_private_or_link_local() {
        for ip in ["10.0.0.1", "169.254.169.254", "192.168.1.1"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(
                is_blocked_for_host(&ip, "localhost", true),
                "{ip} must stay blocked even with allow_local"
            );
        }
    }

    #[test]
    fn blocks_test_net_and_reserved_ranges() {
        for ip in [
            "0.0.0.1",
            "192.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.1",
            "203.0.113.1",
            "240.0.0.1",
        ] {
            assert!(is_non_public_ip(&ip.parse().unwrap()), "{ip}");
        }
        // Neighbours of every range above must stay reachable.
        for ip in [
            "1.0.0.1",
            "192.0.1.1",
            "192.0.3.1",
            "198.17.255.255",
            "198.20.0.1",
            "198.51.101.1",
            "203.0.114.1",
            "223.255.255.255",
        ] {
            assert!(!is_non_public_ip(&ip.parse().unwrap()), "{ip}");
        }
    }

    /// A v6 record can smuggle v4 through four wrapper prefixes.
    #[test]
    fn blocks_ipv4_smuggled_through_ipv6_wrappers() {
        for ip in [
            "64:ff9b::a9fe:a9fe",
            "64:ff9b::7f00:1",
            "2002:7f00:1::",
            "::7f00:1",
            "::a00:1",
            "fec0::1",
            "2001:db8::1",
        ] {
            assert!(is_non_public_ip(&ip.parse().unwrap()), "{ip}");
        }
        for ip in ["64:ff9b::808:808", "2002:808:808::", "2001:db9::1"] {
            assert!(!is_non_public_ip(&ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn explicit_local_host_tolerates_brackets_dots_and_zone_ids() {
        for host in [
            "localhost",
            "LOCALHOST.",
            "127.0.0.1",
            "127.1.2.3",
            "::1",
            "[::1]",
            "::1%lo0",
        ] {
            assert!(is_explicit_local_host(host), "{host}");
        }
        for host in [
            "example.com",
            "notlocalhost",
            "localhost.evil.com",
            "10.0.0.1",
        ] {
            assert!(!is_explicit_local_host(host), "{host}");
        }
    }

    #[test]
    fn allows_public_ips() {
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "142.250.80.46",
            // Global unicast v6: guards the new masks against over-matching.
            "2606:4700::1111",
            "2001:4860:4860::8888",
        ] {
            assert!(!is_non_public_ip(&ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn blocks_ipv6_link_local() {
        assert!(is_non_public_ip(&"fe80::1".parse().unwrap()));
    }

    #[test]
    fn blocks_ipv6_unique_local() {
        assert!(is_non_public_ip(&"fc00::1".parse().unwrap()));
        assert!(is_non_public_ip(&"fd00::1".parse().unwrap()));
    }

    #[test]
    fn blocks_ipv4_mapped_ipv6_private() {
        assert!(is_non_public_ip(
            &"::ffff:10.0.0.1".parse::<IpAddr>().unwrap()
        ));
        assert!(is_non_public_ip(
            &"::ffff:192.168.1.1".parse::<IpAddr>().unwrap()
        ));
    }

    #[test]
    fn allows_ipv4_mapped_ipv6_public() {
        assert!(!is_non_public_ip(
            &"::ffff:8.8.8.8".parse::<IpAddr>().unwrap()
        ));
    }

    #[tokio::test]
    async fn ssrf_blocks_ip_literal_private() {
        let url = Url::parse("https://10.0.0.1/secret").unwrap();
        let result = check_ssrf(&url, false).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("private"));
    }

    #[tokio::test]
    async fn ssrf_allows_ip_literal_public() {
        let url = Url::parse("https://1.1.1.1/").unwrap();
        let result = check_ssrf(&url, false).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn ssrf_blocks_loopback_literal_by_default() {
        let url = Url::parse("http://127.0.0.1:8080/").unwrap();
        assert!(check_ssrf(&url, false).await.is_err());
    }

    #[tokio::test]
    async fn ssrf_allows_loopback_literal_when_opted_in() {
        let url = Url::parse("http://127.0.0.1:8080/").unwrap();
        assert!(check_ssrf(&url, true).await.is_ok());
    }
}
