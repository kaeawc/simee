//! Random bytes from the host's operating system, by one name on every host: the kernel's source
//! on unix and `ProcessPrng` on Windows, which has no `/dev/urandom` to open. For tokens, ids and
//! the guest's random device; callers that can do without entropy say so themselves.

use std::io;

/// Fills `buf` with random bytes from the OS.
pub fn fill(buf: &mut [u8]) -> io::Result<()> {
    getrandom::fill(buf).map_err(|e| io::Error::other(format!("the OS gave no random bytes: {e}")))
}

/// `n` random bytes as lowercase hex (`2 * n` characters).
pub fn hex(n: usize) -> io::Result<String> {
    use std::fmt::Write as _;
    let mut bytes = vec![0u8; n];
    fill(&mut bytes)?;
    Ok(bytes.iter().fold(String::with_capacity(2 * n), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_are_filled_and_differ_between_calls() {
        let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
        fill(&mut a).expect("fill");
        fill(&mut b).expect("fill");
        assert!(a.iter().any(|&x| x != 0), "not all zeros");
        assert_ne!(a, b, "two draws of 256 bits differ");
    }

    #[test]
    fn hex_is_twice_as_long_and_lowercase_hex() {
        let h = hex(16).expect("hex");
        assert_eq!(h.len(), 32);
        assert!(h.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)), "{h}");
        assert_eq!(hex(0).expect("empty"), "");
    }
}
