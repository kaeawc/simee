//! Unix domain sockets, on the hosts that have them.
//!
//! The device API, the display stream and the camera feed all speak over `UnixStream` and
//! `UnixListener`. Rust's standard library has them on unix only (the Windows ones are still
//! unstable), so code that must still compile on Windows names these types instead: std's own
//! where they exist, and elsewhere stand-ins that can't be built. Every constructor there fails
//! with [`std::io::ErrorKind::Unsupported`], so the first use says so, and the methods are
//! unreachable because no value exists. A transport for Windows replaces the stand-ins.

#[cfg(unix)]
pub use std::os::unix::net::{Incoming, SocketAddr, UnixListener, UnixStream};

#[cfg(not(unix))]
pub use stand_in::{Incoming, SocketAddr, UnixListener, UnixStream};

/// Whether this host has Unix domain sockets.
pub const AVAILABLE: bool = cfg!(unix);

#[cfg(not(unix))]
mod stand_in {
    use std::io::{self, Read, Write};
    use std::net::Shutdown;
    use std::path::Path;
    use std::time::Duration;

    /// No value of this type exists.
    #[derive(Clone, Copy, Debug)]
    enum Never {}

    fn unsupported<T>() -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Unix domain sockets are not available on this host (Windows needs another transport)",
        ))
    }

    #[derive(Debug)]
    pub struct SocketAddr(Never);

    #[derive(Debug)]
    pub struct UnixStream(Never);

    #[derive(Debug)]
    pub struct UnixListener(Never);

    pub struct Incoming<'a>(&'a UnixListener);

    // As on unix, where dropping closes the socket: nothing to close, and no value to drop.
    impl Drop for UnixStream {
        fn drop(&mut self) {
            match self.0 {}
        }
    }

    impl Drop for UnixListener {
        fn drop(&mut self) {
            match self.0 {}
        }
    }

    impl SocketAddr {
        pub fn as_pathname(&self) -> Option<&Path> {
            match self.0 {}
        }
    }

    impl UnixStream {
        pub fn connect<P: AsRef<Path>>(_path: P) -> io::Result<UnixStream> {
            unsupported()
        }

        pub fn pair() -> io::Result<(UnixStream, UnixStream)> {
            unsupported()
        }

        pub fn try_clone(&self) -> io::Result<UnixStream> {
            match self.0 {}
        }

        pub fn set_read_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
            match self.0 {}
        }

        pub fn set_write_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
            match self.0 {}
        }

        pub fn set_nonblocking(&self, _nonblocking: bool) -> io::Result<()> {
            match self.0 {}
        }

        pub fn shutdown(&self, _how: Shutdown) -> io::Result<()> {
            match self.0 {}
        }

        pub fn peer_addr(&self) -> io::Result<SocketAddr> {
            match self.0 {}
        }
    }

    impl Read for UnixStream {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            match self.0 {}
        }
    }

    impl Read for &UnixStream {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            match self.0 {}
        }
    }

    impl Write for UnixStream {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            match self.0 {}
        }

        fn flush(&mut self) -> io::Result<()> {
            match self.0 {}
        }
    }

    impl Write for &UnixStream {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            match self.0 {}
        }

        fn flush(&mut self) -> io::Result<()> {
            match self.0 {}
        }
    }

    impl UnixListener {
        pub fn bind<P: AsRef<Path>>(_path: P) -> io::Result<UnixListener> {
            unsupported()
        }

        pub fn accept(&self) -> io::Result<(UnixStream, SocketAddr)> {
            match self.0 {}
        }

        pub fn try_clone(&self) -> io::Result<UnixListener> {
            match self.0 {}
        }

        pub fn set_nonblocking(&self, _nonblocking: bool) -> io::Result<()> {
            match self.0 {}
        }

        pub fn local_addr(&self) -> io::Result<SocketAddr> {
            match self.0 {}
        }

        pub fn incoming(&self) -> Incoming<'_> {
            Incoming(self)
        }
    }

    impl Iterator for Incoming<'_> {
        type Item = io::Result<UnixStream>;

        fn next(&mut self) -> Option<Self::Item> {
            match self.0.0 {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;

    #[test]
    fn the_host_either_has_sockets_or_says_it_has_none() {
        let pair = UnixStream::pair();
        if AVAILABLE {
            let (mut a, mut b) = pair.expect("pair");
            a.write_all(b"ping").expect("write");
            let mut got = [0u8; 4];
            b.read_exact(&mut got).expect("read");
            assert_eq!(&got, b"ping");
        } else {
            assert_eq!(pair.expect_err("no sockets").kind(), std::io::ErrorKind::Unsupported);
            let path = std::env::temp_dir().join("simee-no-such.sock");
            assert_eq!(UnixStream::connect(&path).expect_err("connect").kind(), std::io::ErrorKind::Unsupported);
            assert_eq!(UnixListener::bind(&path).expect_err("bind").kind(), std::io::ErrorKind::Unsupported);
        }
    }
}
