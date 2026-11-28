//! `adb forward` and `adb reverse` on a device: the two tunnels a Wear OS pairing is built from
//! ([`crate::wear`]), and any front end that reaches a port in the guest. Ports are numbers, so
//! nothing a caller passes reaches adb's command line as text.
//!
//! A forward lives in the adb server (it listens on the host and dials the device), a reverse in
//! the device's `adbd` (it listens on the device and dials the host through the server): the
//! first is lost when the server restarts, the second when the device does.

use crate::apps::{Adb, failure};

/// One tunnel as adb lists it: `<owner> tcp:<listen> tcp:<target>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tunnel {
    /// The device serial for a forward (`emulator-5554`); for a reverse, what adb calls the
    /// transport (`UsbFfs`).
    pub owner: String,
    /// The TCP port that listens: on the host for a forward, on the device for a reverse.
    pub listen: u16,
    /// The TCP port it dials: on the device for a forward, on the host for a reverse.
    pub target: u16,
}

/// The `tcp:` tunnels in `adb forward --list` or `adb reverse --list`; other sockets
/// (`localabstract:`, `jdwp:`) and anything not shaped like a listing are skipped.
pub fn parse_tunnels(out: &str) -> Vec<Tunnel> {
    let port = |s: &str| s.strip_prefix("tcp:")?.parse::<u16>().ok();
    out.lines()
        .filter_map(|l| {
            let parts: Vec<&str> = l.split_whitespace().collect();
            let [owner, listen, target] = parts[..] else { return None };
            Some(Tunnel { owner: owner.to_owned(), listen: port(listen)?, target: port(target)? })
        })
        .collect()
}

impl Adb {
    /// `adb forward tcp:<host> tcp:<device>`: the host's `host` port reaches the device's `device`
    /// port. With `rebind` false a host port that is already forwarded is refused (adb's `cannot
    /// rebind existing socket`) instead of taken over.
    pub fn bind_forward(&self, host: u16, device: u16, rebind: bool) -> Result<(), String> {
        let (host, device) = (format!("tcp:{host}"), format!("tcp:{device}"));
        let mut args = vec!["forward"];
        if !rebind {
            args.push("--no-rebind");
        }
        args.extend([host.as_str(), device.as_str()]);
        self.tunnel_call("adb forward", &args)
    }

    /// `adb forward --list`: every forward the adb server holds, for all devices ([`Adb::forwards`]
    /// is this device's, as ports; `remove_forward` takes one down).
    pub fn forward_tunnels(&self) -> Result<Vec<Tunnel>, String> {
        self.tunnel_list("adb forward --list", &["forward", "--list"])
    }

    /// `adb reverse tcp:<device> tcp:<host>`: the device's `device` port reaches the host's `host`
    /// port (and from there whatever the host forwards it to).
    pub fn reverse(&self, device: u16, host: u16) -> Result<(), String> {
        self.tunnel_call("adb reverse", &["reverse", &format!("tcp:{device}"), &format!("tcp:{host}")])
    }

    /// `adb reverse --list`: the reverses of this device.
    pub fn reverse_tunnels(&self) -> Result<Vec<Tunnel>, String> {
        self.tunnel_list("adb reverse --list", &["reverse", "--list"])
    }

    /// `adb reverse --remove tcp:<device>`.
    pub fn remove_reverse(&self, device: u16) -> Result<(), String> {
        self.tunnel_call("adb reverse --remove", &["reverse", "--remove", &format!("tcp:{device}")])
    }

    fn tunnel_call(&self, what: &str, args: &[&str]) -> Result<(), String> {
        match self.run(args, self.deadlines.query)? {
            (true, _) => Ok(()),
            (false, text) => Err(failure(what, &text)),
        }
    }

    fn tunnel_list(&self, what: &str, args: &[&str]) -> Result<Vec<Tunnel>, String> {
        match self.run(args, self.deadlines.query)? {
            (true, text) => Ok(parse_tunnels(&text)),
            (false, text) => Err(failure(what, &text)),
        }
    }
}

/// Run against `testing`'s fake adb, which is unix only.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testing::AdbRig;

    #[test]
    fn listings_keep_only_tcp_tunnels() {
        let text = "emulator-5554 tcp:5602 tcp:5601
UsbFfs tcp:5601 tcp:5602
emulator-5554 tcp:6000 localabstract:x
                    emulator-5554 tcp:notaport tcp:1
short line

  emulator-5556   tcp:41001   tcp:5277  
";
        let t = |owner: &str, listen, target| Tunnel { owner: owner.into(), listen, target };
        assert_eq!(parse_tunnels(text), [t("emulator-5554", 5602, 5601), t("UsbFfs", 5601, 5602), t("emulator-5556", 41001, 5277)]);
        assert!(parse_tunnels("").is_empty());
        assert!(parse_tunnels("tcp:1 tcp:2").is_empty(), "no owner is no listing");
        assert!(parse_tunnels("a tcp:70000 tcp:1").is_empty(), "past the last port");
    }

    #[test]
    fn forward_binds_lists_and_removes() {
        let rig = AdbRig::new("forward");
        let adb = rig.adb("emulator-5554");
        let fake = &rig.fake;
        adb.bind_forward(5602, 5601, false).expect("forward");
        assert_eq!(fake.forwards(), ["emulator-5554 tcp:5602 tcp:5601"]);
        assert_eq!(adb.forward_tunnels().expect("list"), [Tunnel { owner: "emulator-5554".into(), listen: 5602, target: 5601 }]);
        adb.remove_forward(5602).expect("remove");
        assert!(fake.forwards().is_empty());
        let calls = fake.calls();
        assert_eq!(calls, ["forward --no-rebind tcp:5602 tcp:5601", "forward --list", "forward --remove tcp:5602"]);
    }

    #[test]
    fn forward_without_rebind_refuses_a_forwarded_port_and_with_it_takes_over() {
        let rig = AdbRig::new("forward");
        let adb = rig.adb("emulator-5554");
        let fake = &rig.fake;
        adb.bind_forward(5602, 5601, false).expect("first");
        let err = Adb { serial: "emulator-5556".into(), ..adb.clone() }.bind_forward(5602, 5601, false).expect_err("taken");
        assert!(err.contains("cannot rebind"), "{err}");
        Adb { serial: "emulator-5556".into(), ..adb.clone() }.bind_forward(5602, 5601, true).expect("rebind");
        assert_eq!(fake.forwards(), ["emulator-5556 tcp:5602 tcp:5601"]);
        assert_eq!(fake.calls()[2], "forward tcp:5602 tcp:5601");
    }

    #[test]
    fn removing_a_forward_that_is_not_there_is_an_error() {
        let rig = AdbRig::new("forward");
        let adb = rig.adb("emulator-5554");
        let err = adb.remove_forward(5999).expect_err("absent");
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn reverse_binds_lists_and_removes_per_device() {
        let rig = AdbRig::new("forward");
        let adb = rig.adb("emulator-5556");
        let fake = &rig.fake;
        adb.reverse(5601, 5602).expect("reverse");
        assert_eq!(fake.reverses(), ["emulator-5556 tcp:5601 tcp:5602"]);
        assert_eq!(adb.reverse_tunnels().expect("list"), [Tunnel { owner: "UsbFfs".into(), listen: 5601, target: 5602 }]);
        let other = Adb { serial: "emulator-5554".into(), ..adb.clone() };
        assert!(other.reverse_tunnels().expect("other").is_empty(), "a reverse belongs to its device");
        adb.remove_reverse(5601).expect("remove");
        assert!(fake.reverses().is_empty());
        assert_eq!(fake.calls(), ["reverse tcp:5601 tcp:5602", "reverse --list", "reverse --list", "reverse --remove tcp:5601"]);
    }

    #[test]
    fn an_adb_refusal_is_the_error_with_its_last_line() {
        let rig = AdbRig::new("forward");
        let adb = rig.adb("emulator-5554");
        let fake = &rig.fake;
        fake.ports_taken(1);
        let err = adb.bind_forward(5602, 5601, false).expect_err("busy");
        assert!(err.starts_with("adb forward failed:") && err.contains("Address already in use"), "{err}");
        fake.reverse_fails();
        let err = adb.reverse(5601, 5602).expect_err("closed");
        assert!(err.starts_with("adb reverse failed:") && err.contains("closed"), "{err}");
    }
}
