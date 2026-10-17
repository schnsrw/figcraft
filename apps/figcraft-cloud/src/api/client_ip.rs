//! Client address resolution behind the gateway.
//!
//! The socket peer is the client unless it falls inside a configured trusted-proxy CIDR
//! (`FIGCRAFT_TRUSTED_PROXY_CIDRS`, default none). Only then is `X-Forwarded-For` read, from the
//! right: each trusted hop is skipped and the first untrusted address is the client. A malformed
//! entry stops the walk and the last trusted hop is used, so a forged header can only ever move a
//! client into the proxy's own bucket, never into another client's.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use axum::http::HeaderMap;

/// At most this many CIDRs may be configured.
const MAX_TRUSTED_PROXIES: usize = 64;
/// Forwarded headers longer than this (all values together) are ignored.
const MAX_FORWARDED_BYTES: usize = 2048;
/// At most this many hops are examined.
const MAX_FORWARDED_HOPS: usize = 32;

/// One IPv4 or IPv6 network.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// Parse `a.b.c.d/n`, `x::y/n` or a bare address (a single host).
    pub(crate) fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        let (address, prefix) = match value.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (value, None),
        };
        let address: IpAddr = address.parse().ok()?;
        let max = if address.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(prefix) if !prefix.is_empty() && prefix.len() <= 3 && prefix.bytes().all(|byte| byte.is_ascii_digit()) => prefix.parse().ok()?,
            Some(_) => return None,
            None => max,
        };
        if prefix > max {
            return None;
        }
        Some(Self { network: mask(address, prefix), prefix })
    }

    pub(crate) fn contains(&self, address: IpAddr) -> bool {
        let address = canonical(address);
        address.is_ipv4() == self.network.is_ipv4() && mask(address, self.prefix) == self.network
    }
}

/// Parse a comma-separated CIDR list (bounded).
pub(crate) fn parse_cidrs(value: &str) -> Result<Vec<Cidr>, &'static str> {
    let mut cidrs = Vec::new();
    for part in value.split(',').filter(|part| !part.trim().is_empty()) {
        if cidrs.len() == MAX_TRUSTED_PROXIES {
            return Err("FIGCRAFT_TRUSTED_PROXY_CIDRS lists too many networks");
        }
        cidrs.push(Cidr::parse(part).ok_or("FIGCRAFT_TRUSTED_PROXY_CIDRS must be a comma-separated list of CIDRs")?);
    }
    Ok(cidrs)
}

/// IPv4-mapped IPv6 addresses compare as IPv4.
pub(crate) fn canonical(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(address, IpAddr::V4),
        IpAddr::V4(_) => address,
    }
}

fn mask(address: IpAddr, prefix: u8) -> IpAddr {
    match canonical(address) {
        IpAddr::V4(v4) => {
            let bits = u32::from(v4);
            let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix.min(32))) };
            IpAddr::V4(Ipv4Addr::from(bits & mask))
        }
        IpAddr::V6(v6) => {
            let bits = u128::from(v6);
            let mask = if prefix == 0 { 0 } else { u128::MAX << (128 - u32::from(prefix.min(128))) };
            IpAddr::V6(Ipv6Addr::from(bits & mask))
        }
    }
}

/// The client address for `peer` given the request headers and the trusted proxies.
pub(crate) fn resolve(peer: Option<IpAddr>, headers: &HeaderMap, trusted: &[Cidr]) -> Option<IpAddr> {
    let peer = canonical(peer?);
    let is_trusted = |address: IpAddr| trusted.iter().any(|cidr| cidr.contains(address));
    if !is_trusted(peer) {
        return Some(peer);
    }
    let mut total = 0usize;
    let mut values = Vec::new();
    for value in headers.get_all("x-forwarded-for") {
        total = total.saturating_add(value.len());
        if total > MAX_FORWARDED_BYTES {
            return Some(peer);
        }
        let Ok(text) = value.to_str() else {
            return Some(peer);
        };
        values.push(text);
    }
    let mut client = peer;
    let hops = values.iter().flat_map(|value| value.split(',')).map(str::trim).collect::<Vec<_>>();
    for hop in hops.iter().rev().take(MAX_FORWARDED_HOPS) {
        let Ok(address) = hop.parse::<IpAddr>() else {
            return Some(client);
        };
        let address = canonical(address);
        if !is_trusted(address) {
            return Some(address);
        }
        client = address;
    }
    Some(client)
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::*;

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    fn forwarded(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append("x-forwarded-for", HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    #[test]
    fn cidrs_parse_and_match_both_families() {
        let v4 = Cidr::parse("10.1.0.0/16").unwrap();
        assert!(v4.contains(ip("10.1.200.3")));
        assert!(!v4.contains(ip("10.2.0.1")));
        assert!(v4.contains(ip("::ffff:10.1.0.9")));
        let v6 = Cidr::parse("fd00::/8").unwrap();
        assert!(v6.contains(ip("fd12::1")));
        assert!(!v6.contains(ip("10.1.0.1")));
        assert!(Cidr::parse("192.0.2.7").unwrap().contains(ip("192.0.2.7")));
        assert!(Cidr::parse("0.0.0.0/0").unwrap().contains(ip("203.0.113.1")));
        for bad in ["10.0.0.0/33", "::/129", "10.0.0.0/", "10.0.0.0/x", "nope", "10.0.0.0/0008", ""] {
            assert_eq!(Cidr::parse(bad), None, "{bad}");
        }
        assert_eq!(parse_cidrs("10.0.0.0/8, 172.16.0.0/12").unwrap().len(), 2);
        assert!(parse_cidrs("10.0.0.0/8,bad").is_err());
        assert!(parse_cidrs(&vec!["10.0.0.1"; 65].join(",")).is_err());
    }

    #[test]
    fn forwarded_for_is_ignored_unless_the_peer_is_trusted() {
        let headers = forwarded(&["198.51.100.9"]);
        assert_eq!(resolve(Some(ip("203.0.113.5")), &headers, &[]), Some(ip("203.0.113.5")));
        let trusted = parse_cidrs("10.0.0.0/8").unwrap();
        assert_eq!(resolve(Some(ip("203.0.113.5")), &headers, &trusted), Some(ip("203.0.113.5")));
        assert_eq!(resolve(Some(ip("10.0.0.2")), &headers, &trusted), Some(ip("198.51.100.9")));
        assert_eq!(resolve(None, &headers, &trusted), None);
    }

    #[test]
    fn forwarded_for_is_walked_from_the_right_and_stops_on_garbage() {
        let trusted = parse_cidrs("10.0.0.0/8").unwrap();
        // A client-supplied left entry cannot override the address the proxy appended.
        let headers = forwarded(&["1.1.1.1, 198.51.100.9", "10.0.0.3"]);
        assert_eq!(resolve(Some(ip("10.0.0.2")), &headers, &trusted), Some(ip("198.51.100.9")));
        let headers = forwarded(&["198.51.100.9, garbage, 10.0.0.4"]);
        assert_eq!(resolve(Some(ip("10.0.0.2")), &headers, &trusted), Some(ip("10.0.0.4")));
        let headers = forwarded(&[&"9".repeat(MAX_FORWARDED_BYTES + 1)]);
        assert_eq!(resolve(Some(ip("10.0.0.2")), &headers, &trusted), Some(ip("10.0.0.2")));
        assert_eq!(resolve(Some(ip("10.0.0.2")), &HeaderMap::new(), &trusted), Some(ip("10.0.0.2")));
    }
}
