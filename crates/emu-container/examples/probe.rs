//! What this host offers a container, and which backend `choose` would pick for an x86_64 or
//! arm64 guest: `cargo run -p simee-emu-container --example probe`. KVM and TCG are reported as
//! unavailable here (the real checks live in `apps/emu`), so the answer is the container's.
#[cfg(unix)]
use simee_emu_container::probe::{FsView, probe};
#[cfg(unix)]
use simee_emu_container::{Backend, choose};

#[cfg(unix)]
fn main() {
    for line in probe(&FsView).lines() {
        println!("{line}");
    }
    let abi = if std::env::consts::ARCH == "aarch64" { "arm64-v8a" } else { "x86_64" };
    let no = |what: &str| Err(format!("{what} isn't checked by this example"));
    match choose(Some(Backend::Container), abi, no("KVM"), no("TCG")) {
        Ok(s) => println!("container: usable ({})", s.why),
        Err(e) => println!("container: {e}"),
    }
}

#[cfg(not(unix))]
fn main() {
    println!("containers need a Linux host");
}
