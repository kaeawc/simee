//! The host's side of each flow: non-blocking sockets (rustix's safe wrappers) and the host's
//! resolver.
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use rustix::net::{AddressFamily, SocketType, ipproto};

use super::Dns;

fn family(addr: &SocketAddr) -> AddressFamily {
    if addr.is_ipv4() { AddressFamily::INET } else { AddressFamily::INET6 }
}

/// Starts a non-blocking connect: the stream, and whether it already connected (loopback can).
pub fn connect(addr: SocketAddr) -> io::Result<(TcpStream, bool)> {
    let fd = rustix::net::socket(family(&addr), SocketType::STREAM, None)?;
    rustix::io::ioctl_fionbio(&fd, true)?;
    #[cfg(target_vendor = "apple")]
    rustix::net::sockopt::set_socket_nosigpipe(&fd, true)?;
    // The guest's TCP already coalesces; segments go out as they come, as slirp's do.
    rustix::net::sockopt::set_tcp_nodelay(&fd, true)?;
    let done = match rustix::net::connect(&fd, &addr) {
        Ok(()) => true,
        Err(rustix::io::Errno::INPROGRESS) => false,
        Err(e) => return Err(e.into()),
    };
    Ok((TcpStream::from(fd), done))
}

/// The outcome of a connect [`connect`] left in progress, once the socket polls writable.
pub fn connect_result(s: &TcpStream) -> io::Result<()> {
    match rustix::net::sockopt::socket_error(s)? {
        Ok(()) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// A non-blocking UDP socket connected to `addr`.
pub fn udp(addr: SocketAddr) -> io::Result<UdpSocket> {
    let any: SocketAddr = if addr.is_ipv4() { (Ipv4Addr::UNSPECIFIED, 0).into() } else { (std::net::Ipv6Addr::UNSPECIFIED, 0).into() };
    let s = UdpSocket::bind(any)?;
    s.connect(addr)?;
    s.set_nonblocking(true)?;
    Ok(s)
}

/// An unprivileged ICMP echo socket (`SOCK_DGRAM`, `IPPROTO_ICMP`): no root on macOS; on Linux
/// only for groups in `net.ipv4.ping_group_range`. Read and written as a datagram socket.
pub fn ping() -> io::Result<UdpSocket> {
    let fd = rustix::net::socket(AddressFamily::INET, SocketType::DGRAM, Some(ipproto::ICMP))?;
    let s = UdpSocket::from(fd);
    s.set_nonblocking(true)?;
    Ok(s)
}

/// The first `nameserver` in a resolv.conf.
pub fn parse_resolv_conf(text: &str) -> Option<IpAddr> {
    text.lines().find_map(|l| {
        let mut w = l.split_whitespace();
        (w.next() == Some("nameserver")).then(|| w.next()?.parse().ok()).flatten()
    })
}

pub struct Resolver {
    dns: Dns,
    cached: Option<SocketAddr>,
    read_at: Option<Instant>,
}

const REREAD: Duration = Duration::from_secs(5);

impl Resolver {
    pub fn new(dns: Dns) -> Resolver {
        Resolver { dns, cached: None, read_at: None }
    }

    pub fn server(&mut self) -> Option<SocketAddr> {
        match &self.dns {
            Dns::Fixed(a) => Some(*a),
            Dns::System => {
                if self.read_at.is_none_or(|t| t.elapsed() >= REREAD) {
                    self.read_at = Some(Instant::now());
                    let found = std::fs::read_to_string("/etc/resolv.conf").ok().as_deref().and_then(parse_resolv_conf);
                    if found.is_none() && self.cached.is_some() {
                        eprintln!("emu: net: no nameserver in /etc/resolv.conf; guest DNS fails");
                    }
                    self.cached = found.map(|ip| SocketAddr::new(ip, 53));
                }
                self.cached
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_nameserver_wins_and_comments_are_skipped() {
        let conf = "# macOS\ndomain lan\nnameserver fe80::1%en0\nnameserver 192.168.1.1\nnameserver 8.8.8.8\n";
        assert_eq!(parse_resolv_conf(conf), Some(IpAddr::from([192, 168, 1, 1])), "a scoped address doesn't parse");
        assert_eq!(parse_resolv_conf("nameserver 2001:4860:4860::8888"), "2001:4860:4860::8888".parse().ok());
        assert_eq!(parse_resolv_conf("search lan\n"), None);
    }

    #[test]
    fn a_connect_to_a_closed_port_reports_refused() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = l.local_addr().expect("addr");
        drop(l);
        let start = Instant::now();
        let r = connect(addr).and_then(|(s, done)| {
            if done {
                return Ok(());
            }
            let mut fds = [rustix::event::PollFd::new(&s, rustix::event::PollFlags::OUT)];
            rustix::event::poll(&mut fds, None)?;
            connect_result(&s)
        });
        assert_eq!(r.map_err(|e| e.kind()), Err(io::ErrorKind::ConnectionRefused));
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
