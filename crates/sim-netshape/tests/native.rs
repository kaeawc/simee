#![cfg(unix)]
//! The shaper's portable core (`native/netshape.c`), built with the host C compiler and run by
//! `native/netshape_test.c` against real host sockets: profile parsing and publishing, rule
//! matching and its cache, per-fd state, the I/O gates and pacing. Also checks that the shaper and
//! the Rust side (the proxy's `Profile::for_host`) give every host the same profile. Unix only: the C
//! test uses POSIX sockets and `-pthread`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use simee_netshape::{Profile, Rule};

/// Builds `native/netshape_test.c` as `name` in this test's scratch directory.
fn build(name: &str) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("native/netshape_test.c");
    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let built = Command::new(&cc).args(["-std=gnu11", "-O2", "-Wall", "-Wextra", "-pthread", "-o"]).arg(&out).arg(&src).output();
    let built = built.unwrap_or_else(|e| panic!("{cc}: {e}"));
    assert!(built.status.success(), "{cc} {}: {}", src.display(), String::from_utf8_lossy(&built.stderr));
    out
}

fn text(out: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

#[test]
fn shaper_core_passes_its_tests() {
    let bin = build("netshape_core");
    let out = Command::new(&bin).env("TMPDIR", env!("CARGO_TARGET_TMPDIR")).output().expect("run netshape_test");
    assert!(out.status.success(), "{}", text(&out));
    // The C test skips its IPv6 cases where the host has no IPv6; say so in this test's output.
    for line in text(&out).lines().filter(|l| l.starts_with("SKIPPED:")) {
        println!("{line}");
    }
}

#[test]
fn shaper_and_proxy_agree_on_each_hosts_profile() {
    let rule = |pattern: &str| Rule { pattern: pattern.into(), ..Rule::default() };
    let profile = Profile {
        latency_ms: 80,
        down_kbps: 1600,
        up_kbps: 400,
        loss_pct: 2,
        rules: vec![
            Rule { latency_ms: Some(5), offline: Some(false), ..rule("10.0.0.0/8") },
            Rule { offline: Some(true), ..rule("2001:db8::/32") },
            Rule { down_kbps: Some(64), up_kbps: Some(32), loss_pct: Some(30), ..rule("example.com") },
            Rule { latency_ms: Some(900), ..rule("192.168.1.5") },
            Rule { latency_ms: Some(1), ..rule("172.16.0.0/12") },
        ],
        ..Profile::default()
    };
    let conf = Path::new(env!("CARGO_TARGET_TMPDIR")).join("agree.conf");
    std::fs::write(&conf, profile.to_conf()).expect("write profile");
    let hosts = [
        "10.1.2.3",
        "11.0.0.1",
        "2001:db8::5",
        "2001:db9::5",
        "api.example.com",
        "EXAMPLE.com",
        "badexample.com",
        "192.168.1.5",
        "192.168.1.6",
        "172.31.0.1",
        "172.32.0.1",
        "other.test",
    ];
    let out = Command::new(build("netshape_agree")).arg("effective").arg(&conf).args(hosts).output().expect("run netshape_test");
    assert!(out.status.success(), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), hosts.len(), "{stdout}");
    for (line, host) in lines.iter().zip(hosts) {
        let p = profile.for_host(host);
        let bps = |kbps: u32| u64::from(kbps) * 1000 / 8;
        let want = format!("{host} {} {} {} {} {}", u8::from(p.offline), p.latency_ms, bps(p.down_kbps), bps(p.up_kbps), p.loss_pct);
        assert_eq!(*line, want);
    }
}
