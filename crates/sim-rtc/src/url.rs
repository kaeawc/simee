//! The slice of URL handling WHIP needs: `http`/`https` URLs with a host, a port and a path,
//! `Location` resolution (RFC 3986 §5.2 for the reference forms servers send), and whether a host
//! is loopback.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }
}

/// An absolute `http` or `https` URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    scheme: Scheme,
    /// The host without IPv6 brackets, lowercased.
    host: String,
    port: u16,
    /// Path and query, always starting with `/`; never the fragment.
    path: String,
}

impl Url {
    /// Parse an absolute URL. User info (`user:pass@`) is refused: credentials go in the bearer
    /// token, never in a URL that ends up in logs.
    pub fn parse(s: &str) -> Result<Url> {
        let s = s.trim();
        // Errors name the URL, never the credentials someone put in it.
        let shown = redact_userinfo(s);
        let bad = |why: &str| Error::InvalidInput(format!("URL {shown:?}: {why}"));
        let (scheme, rest) = s.split_once("://").ok_or_else(|| bad("not absolute"))?;
        let scheme = match scheme.to_ascii_lowercase().as_str() {
            "http" => Scheme::Http,
            "https" => Scheme::Https,
            _ => return Err(bad("only http and https are supported")),
        };
        let rest = rest.split('#').next().unwrap_or_default();
        let split = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, path) = rest.split_at(split);
        if authority.contains('@') {
            return Err(bad("user info is not allowed; pass a bearer token instead"));
        }
        let (host, port) = split_host_port(authority).ok_or_else(|| bad("bad host or port"))?;
        if host.is_empty() {
            return Err(bad("no host"));
        }
        if host.bytes().any(|b| b.is_ascii_whitespace() || b.is_ascii_control()) {
            return Err(bad("bad host"));
        }
        let path = match path {
            "" => "/".to_owned(),
            p if p.starts_with('?') => format!("/{p}"),
            p => p.to_owned(),
        };
        if path.bytes().any(|b| b.is_ascii_whitespace() || b.is_ascii_control()) {
            return Err(bad("whitespace in the path"));
        }
        Ok(Url { scheme, host: host.to_ascii_lowercase(), port: port.unwrap_or(scheme.default_port()), path })
    }

    /// Resolve a reference (a `Location` header) against this URL: absolute (`http://…`),
    /// network-path (`//host/…`), absolute-path (`/…`) or relative (`res/1`) references.
    pub fn join(&self, reference: &str) -> Result<Url> {
        let reference = reference.trim();
        // RFC 3986 §3.1: a scheme is letters, digits, `+`, `-` and `.` before the first `:`.
        let scheme = reference.split_once(':').map(|(s, _)| s).filter(|s| {
            s.starts_with(|c: char| c.is_ascii_alphabetic()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))
        });
        if scheme.is_some() {
            return Url::parse(reference);
        }
        if let Some(rest) = reference.strip_prefix("//") {
            return Url::parse(&format!("{}://{rest}", self.scheme.as_str()));
        }
        let path = if reference.starts_with('/') {
            reference.to_owned()
        } else if reference.is_empty() {
            self.path.clone()
        } else if reference.starts_with('?') {
            let base = self.path.split('?').next().unwrap_or("/");
            format!("{base}{reference}")
        } else {
            let base = self.path.split('?').next().unwrap_or("/");
            let dir = &base[..base.rfind('/').map_or(0, |i| i + 1)];
            format!("{dir}{reference}")
        };
        let mut url = self.clone();
        url.path = remove_dot_segments(&path);
        if url.path.bytes().any(|b| b.is_ascii_whitespace() || b.is_ascii_control()) {
            return Err(Error::InvalidInput(format!("reference {reference:?}: whitespace")));
        }
        Ok(url)
    }

    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Path and query, for the request line.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// `host[:port]` for the `Host` header (brackets around IPv6; no port when it's the
    /// scheme's default).
    pub fn authority(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if self.port == self.scheme.default_port() { host } else { format!("{host}:{}", self.port) }
    }

    /// The host as an IP address, when it is one.
    pub fn ip(&self) -> Option<IpAddr> {
        self.host.parse().ok()
    }

    /// Whether the host is this machine: `localhost` (and `*.localhost`, RFC 6761), `127/8` or
    /// `::1`. Names other than localhost are not resolved: a name that resolves to loopback
    /// today can resolve elsewhere tomorrow.
    pub fn is_loopback(&self) -> bool {
        if self.host == "localhost" || self.host.ends_with(".localhost") {
            return true;
        }
        match self.ip() {
            Some(IpAddr::V4(v4)) => v4.is_loopback(),
            Some(IpAddr::V6(v6)) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
            None => false,
        }
    }

    /// The IP to reach the host with when it's loopback by name.
    pub fn loopback_ip(&self) -> Option<IpAddr> {
        if self.host == "localhost" || self.host.ends_with(".localhost") {
            return Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
        }
        match self.ip() {
            Some(ip) if ip.is_loopback() => Some(ip),
            // An IPv4-mapped address means the IPv4 stack: a server on 127.0.0.1 isn't on ::1.
            Some(IpAddr::V6(v6)) => v6.to_ipv4_mapped().filter(|v4| v4.is_loopback()).map(IpAddr::V4),
            _ => None,
        }
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}{}", self.scheme.as_str(), self.authority(), self.path)
    }
}

/// `s` with any `user:pass@` in its authority replaced by `<redacted>@`.
fn redact_userinfo(s: &str) -> String {
    let Some((scheme, rest)) = s.split_once("://") else { return s.to_owned() };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..end].rfind('@') {
        Some(at) => format!("{scheme}://<redacted>@{}", &rest[at + 1..]),
        None => s.to_owned(),
    }
}

/// `host`, `host:port`, `[v6]` or `[v6]:port`.
fn split_host_port(authority: &str) -> Option<(&str, Option<u16>)> {
    let parse_port =
        |p: &str| -> Option<Option<u16>> { if p.is_empty() { Some(None) } else { p.parse().ok().filter(|&p| p != 0).map(Some) } };
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        host.parse::<Ipv6Addr>().ok()?;
        let port = match after {
            "" => None,
            p => parse_port(p.strip_prefix(':')?)?,
        };
        return Some((host, port));
    }
    match authority.rsplit_once(':') {
        Some((host, _)) if host.contains(':') => None,
        Some((host, port)) => Some((host, parse_port(port)?)),
        None => Some((authority, None)),
    }
}

/// RFC 3986 §5.2.4, on the path (the query, if any, is kept as is).
fn remove_dot_segments(path: &str) -> String {
    let (path, query) = match path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path, None),
    };
    let mut out: Vec<&str> = Vec::new();
    let segments: Vec<&str> = path.split('/').collect();
    for (i, seg) in segments.iter().enumerate() {
        let last = i == segments.len() - 1;
        match *seg {
            "." => {
                if last {
                    out.push("");
                }
            }
            ".." => {
                if out.len() > 1 {
                    out.pop();
                }
                if last {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    let mut joined = out.join("/");
    if !joined.starts_with('/') {
        joined.insert(0, '/');
    }
    match query {
        Some(q) => format!("{joined}?{q}"),
        None => joined,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).expect(s)
    }

    #[test]
    fn parses_hosts_ports_and_paths() {
        let u = url("http://127.0.0.1:8889/sim/whip");
        assert_eq!((u.host(), u.port(), u.path()), ("127.0.0.1", 8889, "/sim/whip"));
        assert_eq!(u.to_string(), "http://127.0.0.1:8889/sim/whip");
        let u = url("HTTPS://Example.COM?x=1#frag");
        assert_eq!((u.scheme(), u.host(), u.port(), u.path()), (Scheme::Https, "example.com", 443, "/?x=1"));
        assert_eq!(u.authority(), "example.com");
        let u = url("http://[::1]:8889/a");
        assert_eq!((u.host(), u.authority()), ("::1", "[::1]:8889".to_owned()));
        assert_eq!(url("http://h").path(), "/");
    }

    #[test]
    fn refuses_what_it_cannot_send() {
        for bad in [
            "127.0.0.1:8889/whip",
            "ftp://h/x",
            "http://user:pw@h/x",
            "http://h:0/x",
            "http://h:99999/x",
            "http://::1/x",
            "http:///x",
            "http://h/a b",
            "http://[zz]/x",
        ] {
            assert!(Url::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn joins_location_references() {
        let base = url("http://localhost:8889/live/sim/whip");
        assert_eq!(base.join("/live/sim/whip/abc").expect("abs").to_string(), "http://localhost:8889/live/sim/whip/abc");
        assert_eq!(base.join("whip/abc").expect("rel").path(), "/live/sim/whip/abc");
        assert_eq!(base.join("../res/1").expect("dots").path(), "/live/res/1");
        assert_eq!(base.join("./x/./y/../z").expect("dots").path(), "/live/sim/x/z");
        assert_eq!(base.join("//other:9/r").expect("net").to_string(), "http://other:9/r");
        assert_eq!(base.join("http://10.0.0.2/r").expect("full").host(), "10.0.0.2");
        assert_eq!(base.join("?session=2").expect("query").path(), "/live/sim/whip?session=2");
        assert_eq!(base.join("../../../../up").expect("clamped").path(), "/up");
    }

    #[test]
    fn knows_loopback_hosts() {
        for host in [
            "http://localhost/",
            "http://LOCALHOST:1/",
            "http://a.localhost/",
            "http://127.0.0.1/",
            "http://127.9.9.9/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
        ] {
            assert!(url(host).is_loopback(), "{host}");
            assert!(url(host).loopback_ip().is_some(), "{host}");
        }
        assert_eq!(url("http://[::ffff:127.0.0.1]/").loopback_ip(), Some(IpAddr::V4(Ipv4Addr::LOCALHOST)), "mapped stays IPv4");
        assert_eq!(url("http://[::1]/").loopback_ip(), Some(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        for creds in ["http://user:pw@h/x", "ftp://user:pw@h/x", "http://user:pw@/x"] {
            let err = Url::parse(creds).expect_err(creds).to_string();
            assert!(!err.contains("pw"), "{err}");
        }
        for host in [
            "http://192.168.1.2/",
            "http://10.0.0.1/",
            "http://example.com/",
            "http://localhost.example.com/",
            "http://[::2]/",
            "http://0.0.0.0/",
        ] {
            assert!(!url(host).is_loopback(), "{host}");
            assert!(url(host).loopback_ip().is_none(), "{host}");
        }
    }
}
