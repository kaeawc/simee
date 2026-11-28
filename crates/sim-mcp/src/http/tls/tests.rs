//! The certificate side of TLS: the self-signed certificate's cache, its names and fingerprint.

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::real::{civil_from_days, fingerprint, self_signed, self_signed_names};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("simee-mcp-tls-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        Scratch(dir)
    }

    fn state(&self) -> PathBuf {
        self.0.join("state")
    }

    fn file(&self) -> PathBuf {
        self.state().join(super::real::SELF_SIGNED_FILE)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn at(days: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(days * 86_400)
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|n| (*n).to_owned()).collect()
}

fn leaf(identity: &(Vec<CertificateDer<'static>>, rustls::pki_types::PrivateKeyDer<'static>)) -> Vec<u8> {
    identity.0[0].as_ref().to_vec()
}

#[test]
fn the_fingerprint_is_the_sha256_of_the_der_in_openssl_form() {
    // SHA-256("abc"), from FIPS 180-2's examples.
    assert_eq!(
        fingerprint(&CertificateDer::from(b"abc".to_vec())),
        "BA:78:16:BF:8F:01:CF:EA:41:41:40:DE:5D:AE:22:23:B0:03:61:A3:96:17:7A:9C:B4:10:FF:61:F2:00:15:AD"
    );
}

#[test]
fn calendar_dates_come_from_day_numbers() {
    for (days, date) in [
        (-1, (1969, 12, 31)),
        (0, (1970, 1, 1)),
        (59, (1970, 3, 1)),
        (11_016, (2000, 2, 29)),
        (19_723, (2024, 1, 1)),
        (20_000, (2024, 10, 4)),
        (47_482, (2100, 1, 1)),
    ] {
        assert_eq!(civil_from_days(days), date, "day {days}");
    }
}

#[test]
fn a_self_signed_certificate_names_loopback_the_bind_address_and_what_is_added() {
    let loopback = self_signed_names(Ipv4Addr::LOCALHOST.into(), &[]);
    assert_eq!(loopback, ["localhost", "127.0.0.1", "::1"]);
    assert_eq!(self_signed_names(Ipv4Addr::UNSPECIFIED.into(), &[]), loopback, "every interface is no address to name");
    let specific = self_signed_names(Ipv4Addr::new(10, 0, 0, 5).into(), &names(&["box.lan", "localhost"]));
    assert_eq!(specific, ["localhost", "127.0.0.1", "::1", "10.0.0.5", "box.lan"]);
}

#[test]
fn a_self_signed_certificate_is_made_private_and_reused() {
    let scratch = Scratch::new("reuse");
    let wanted = names(&["localhost", "127.0.0.1"]);
    let first = self_signed(&scratch.state(), &wanted, at(20_000)).expect("made");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(scratch.file()).expect("meta").permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(scratch.state()).expect("meta").permissions().mode() & 0o777, 0o700);
    }
    // Later runs, a month on, serve the same certificate: the fingerprint clients pinned stays good.
    let again = self_signed(&scratch.state(), &wanted, at(20_030)).expect("reused");
    assert_eq!(leaf(&first), leaf(&again));
    // The certificate is also written alone, for a client to trust: no key in it, mode 0600 too.
    let public = std::fs::read_to_string(scratch.state().join(super::real::CERTIFICATE_FILE)).expect("the public copy");
    assert!(public.starts_with("-----BEGIN CERTIFICATE-----") && !public.contains("PRIVATE KEY"), "{public}");
    let shared: Vec<CertificateDer<'static>> =
        CertificateDer::pem_slice_iter(public.as_bytes()).collect::<Result<_, _>>().expect("a certificate");
    assert_eq!(shared[0].as_ref(), leaf(&first).as_slice());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(scratch.state().join(super::real::CERTIFICATE_FILE)).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let mut files: Vec<String> =
        std::fs::read_dir(scratch.state()).expect("dir").map(|e| e.expect("entry").file_name().to_string_lossy().into_owned()).collect();
    files.sort();
    assert_eq!(files, ["mcp-tls-cert.pem", "mcp-tls.pem"], "no temporary file is left behind");
}

#[test]
fn a_self_signed_certificate_is_made_anew_for_other_names_or_when_it_is_about_to_expire() {
    let scratch = Scratch::new("renew");
    let wanted = names(&["localhost", "127.0.0.1"]);
    let first = self_signed(&scratch.state(), &wanted, at(20_000)).expect("made");
    let other = self_signed(&scratch.state(), &names(&["localhost", "box.lan"]), at(20_001)).expect("other names");
    assert_ne!(leaf(&first), leaf(&other), "a certificate for other names is not reused");
    let kept = self_signed(&scratch.state(), &names(&["localhost", "box.lan"]), at(20_002)).expect("kept");
    assert_eq!(leaf(&other), leaf(&kept));
    // Valid for 730 days, renewed in the last 30.
    let near_end = self_signed(&scratch.state(), &names(&["localhost", "box.lan"]), at(20_001 + 695)).expect("kept");
    assert_eq!(leaf(&other), leaf(&near_end), "35 days from the end is not renewed yet");
    let renewed = self_signed(&scratch.state(), &names(&["localhost", "box.lan"]), at(20_001 + 715)).expect("renewed");
    assert_ne!(leaf(&other), leaf(&renewed), "15 days from the end it is");
}

#[test]
fn a_damaged_cache_is_replaced_but_a_shared_or_linked_one_is_refused() {
    let scratch = Scratch::new("damaged");
    let wanted = names(&["localhost"]);
    let first = self_signed(&scratch.state(), &wanted, at(20_000)).expect("made");
    // Its own file, damaged: made again.
    std::fs::write(scratch.file(), "# simee self-signed certificate: names=localhost not_after=99999999999\nnot pem\n").expect("damage");
    let second = self_signed(&scratch.state(), &wanted, at(20_000)).expect("replaced");
    assert_ne!(leaf(&first), leaf(&second));
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        // Readable by others: the key may be known, so it is not served and not silently replaced.
        std::fs::set_permissions(scratch.file(), std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let e = self_signed(&scratch.state(), &wanted, at(20_000)).expect_err("a shared key");
        assert!(e.to_string().contains("can be read by other users"), "{e}");
        // A link is never followed.
        std::fs::remove_file(scratch.file()).expect("remove");
        let target = scratch.0.join("elsewhere.pem");
        std::fs::write(&target, "x").expect("target");
        symlink(&target, scratch.file()).expect("link");
        let e = self_signed(&scratch.state(), &wanted, at(20_000)).expect_err("a link");
        assert!(e.to_string().contains("symbolic link"), "{e}");
        assert_eq!(std::fs::read_to_string(&target).expect("target"), "x", "and nothing was written through it");
    }
}
