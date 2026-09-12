//! SSRF policy for notification downloads.
//!
//! A notification's `links[rel=canonical].href` is attacker-controlled from
//! this process's point of view: the Global Broker only relays messages from
//! WMO-registered centres, but a compromised or misconfigured centre could
//! point a link at `http://169.254.169.254/` or an internal service. The
//! default policy is therefore:
//!
//! - `https` only;
//! - the host must be a DNS name — never an IP literal, `localhost`, or a
//!   `.local` / `.internal` / `.localhost` name;
//! - every address the name resolves to must be public (no loopback,
//!   link-local, RFC 1918, ULA, multicast, unspecified);
//! - the HTTP client follows no redirects (a public host could 302 to an
//!   internal one).
//!
//! An operator can tighten this with `download_allowlist` (URL prefixes):
//! when set, a URL must **also** match one of them. A fixed default
//! allowlist is deliberately not shipped — the Global Cache hostnames vary
//! per cache and change without notice, so it would break silently.
//!
//! The name is resolved once, before the request; the client may re-resolve
//! and get a different answer (DNS rebinding). That window is accepted for
//! background fetches of public weather data — documented, not defended.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

use url::{Host, Url};

use crate::Wis2Error;

#[derive(Debug, Clone)]
pub struct DownloadPolicy {
    allowlist: Vec<String>,
}

impl DownloadPolicy {
    /// `allowlist` entries are URL prefixes (`https://host/path/`), already
    /// validated by `ds_core::config::validate_wis2`.
    pub fn new(allowlist: Vec<String>) -> Self {
        DownloadPolicy { allowlist }
    }

    /// Whether the policy is in allowlist (strict) mode.
    pub fn is_strict(&self) -> bool {
        !self.allowlist.is_empty()
    }

    /// Parse and vet `href` without touching the network (scheme, host
    /// shape, allowlist). Returns the parsed URL on success.
    pub fn check_static(&self, href: &str) -> Result<Url, Wis2Error> {
        let url = Url::parse(href).map_err(|e| Wis2Error::Policy(format!("'{href}': {e}")))?;
        if url.scheme() != "https" {
            return Err(Wis2Error::Policy(format!(
                "'{href}': only https downloads are allowed"
            )));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Wis2Error::Policy(format!(
                "'{href}': credentials in the URL are not allowed"
            )));
        }
        match url.host() {
            Some(Host::Domain(d)) => {
                let d = d.trim_end_matches('.').to_ascii_lowercase();
                if d == "localhost"
                    || d.ends_with(".localhost")
                    || d.ends_with(".local")
                    || d.ends_with(".internal")
                    || d.ends_with(".home.arpa")
                    || !d.contains('.')
                {
                    return Err(Wis2Error::Policy(format!(
                        "'{href}': host '{d}' is not a public DNS name"
                    )));
                }
            }
            Some(Host::Ipv4(_)) | Some(Host::Ipv6(_)) => {
                return Err(Wis2Error::Policy(format!(
                    "'{href}': IP-literal hosts are not allowed"
                )));
            }
            None => {
                return Err(Wis2Error::Policy(format!("'{href}': no host")));
            }
        }
        if self.is_strict() && !self.allowlist.iter().any(|p| href.starts_with(p.as_str())) {
            return Err(Wis2Error::Policy(format!(
                "'{href}': not under any download_allowlist prefix"
            )));
        }
        Ok(url)
    }

    /// Resolve the host and require every address to be public. Blocking
    /// (getaddrinfo) — call it through `spawn_blocking` or accept the stall
    /// on the background runtime.
    pub fn check_resolved(url: &Url) -> Result<(), Wis2Error> {
        let host = url
            .host_str()
            .ok_or_else(|| Wis2Error::Policy(format!("'{url}': no host")))?;
        let port = url.port_or_known_default().unwrap_or(443);
        let addrs = (host, port)
            .to_socket_addrs()
            .map_err(|e| Wis2Error::Download(format!("'{url}': dns: {e}")))?;
        let mut any = false;
        for a in addrs {
            any = true;
            if !is_public(a.ip()) {
                return Err(Wis2Error::Policy(format!(
                    "'{url}': host resolves to non-public address {}",
                    a.ip()
                )));
            }
        }
        if !any {
            return Err(Wis2Error::Download(format!("'{url}': dns: no addresses")));
        }
        Ok(())
    }
}

/// Public-internet address test (the complement of every reserved range a
/// download must never reach).
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public_v4(v4),
            None => {
                !(v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    || is_ula(v6)
                    || is_link_local_v6(v6)
                    || v6.segments()[0] == 0x2001 && v6.segments()[1] == 0x0db8)
            }
        },
    }
}

fn is_public_v4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    !(v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_unspecified()
        || v4.is_multicast()
        || v4.is_documentation()
        || o[0] == 100 && (64..=127).contains(&o[1]) // CGNAT 100.64/10
        || o[0] == 0
        || o[0] >= 240)
}

fn is_ula(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xfe00) == 0xfc00
}

fn is_link_local_v6(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_checks() {
        let p = DownloadPolicy::new(vec![]);
        assert!(p
            .check_static("https://gc.wis.cma.cn/20260912/x.bufr4")
            .is_ok());
        assert!(p
            .check_static("https://wis2globalcache.s3.amazonaws.com/data/x.xml")
            .is_ok());
        assert!(p
            .check_static("https://meteo.fra1.digitaloceanspaces.com/a.geojson?AWSAccessKeyId=K&Expires=1&Signature=S%3D")
            .is_ok());
        // Rejected shapes.
        for bad in [
            "http://gc.example.org/x",
            "https://127.0.0.1/x",
            "https://[::1]/x",
            "https://169.254.169.254/latest/meta-data",
            "https://localhost/x",
            "https://foo.localhost/x",
            "https://db.internal/x",
            "https://printer.local/x",
            "https://intranet/x",
            "https://user:pw@gc.example.org/x",
            "ftp://gc.example.org/x",
            "not a url",
        ] {
            assert!(p.check_static(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn allowlist_is_strict_mode() {
        let p = DownloadPolicy::new(vec!["https://gc.example.org/data/".into()]);
        assert!(p.is_strict());
        assert!(p.check_static("https://gc.example.org/data/x.bufr").is_ok());
        assert!(p
            .check_static("https://gc.example.org/other/x.bufr")
            .is_err());
        assert!(p
            .check_static("https://other.example.org/data/x.bufr")
            .is_err());
    }

    #[test]
    fn public_address_table() {
        let pub_ok = [
            "8.8.8.8",
            "185.15.59.224",
            "2a00:1450:4001::1",
            "::ffff:8.8.8.8",
        ];
        for a in pub_ok {
            assert!(is_public(a.parse().unwrap()), "{a}");
        }
        let private = [
            "10.0.0.1",
            "172.16.5.5",
            "192.168.1.1",
            "127.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "192.0.2.1",
            "240.0.0.1",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12::1",
            "ff02::1",
            "::ffff:10.0.0.1",
            "2001:db8::1",
        ];
        for a in private {
            assert!(!is_public(a.parse().unwrap()), "{a}");
        }
    }
}
