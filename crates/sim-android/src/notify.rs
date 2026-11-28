//! iOS's `push`, `notify` and `biometric` on a running Android device
//! (docs/research/android-notify-biometric.md):
//!
//! - [`Adb::push`]: a Firebase Cloud Messaging message delivered to an app the way Google Play
//!   services delivers one: the `com.google.android.c2dm.intent.RECEIVE` broadcast to the app's
//!   own FCM receiver, with the message's data as string extras. That receiver only takes the
//!   broadcast from a holder of `com.google.android.c2dm.permission.SEND` (Play services), so it
//!   is sent as root (adbd's uid 0 or `su 0`), which the activity manager lets through.
//! - [`Adb::broadcast`]: `am broadcast -a <action>` with typed extras, as the shell user, to every
//!   registered receiver or one package's.
//! - [`Device::finger_touch`]: the console's `finger touch <id>` and `finger remove`, which the
//!   goldfish fingerprint HAL hears as a finger on the sensor and lifted.

use crate::Device;
use crate::apps::{Adb, failure, launcher_component, package_name, shell_quote};

/// The broadcast Play services sends an app for each FCM message.
pub const FCM_RECEIVE: &str = "com.google.android.c2dm.intent.RECEIVE";

/// FCM's reserved data keys: the FCM server refuses a message that uses them, and here they would
/// clash with the extras the SDK reads.
const FCM_RESERVED: &[&str] = &["from", "message_type", "collapse_key"];
const FCM_RESERVED_PREFIXES: &[&str] = &["google.", "gcm."];

/// An FCM message: its `data` (string values, as FCM's are) and its `notification` fields
/// (`title`, `body`, `channel_id`, `image`, ...), which the Firebase SDK displays as the app's
/// own notification when the app isn't in the foreground and hands to `onMessageReceived` when it is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FcmMessage {
    pub data: Vec<(String, String)>,
    pub notification: Vec<(String, String)>,
}

impl FcmMessage {
    /// The broadcast's string extras, as Play services sends them: each data pair under its own
    /// key, each notification field as `gcm.n.<wire name>` with `gcm.n.e=1`, the marker the SDK
    /// reads as "this message has a notification". A field takes FCM v1's `AndroidNotification`
    /// name; where the wire key differs (`channel_id` is `gcm.n.android_channel_id`) it is mapped,
    /// and the wire name is accepted too. Keys: firebase-android-sdk `Constants.MessageNotificationKeys`.
    pub fn extras(&self) -> Result<Vec<(String, String)>, String> {
        if self.data.is_empty() && self.notification.is_empty() {
            return Err("an FCM message needs `data` or `notification`".into());
        }
        let mut out = Vec::new();
        for (key, value) in &self.data {
            if key.is_empty() {
                return Err("an FCM data key can't be empty".into());
            }
            if FCM_RESERVED.contains(&key.as_str()) || FCM_RESERVED_PREFIXES.iter().any(|p| key.starts_with(p)) {
                return Err(format!("`{key}` is reserved by FCM (from, message_type, collapse_key and google.* or gcm.* keys)"));
            }
            out.push((key.clone(), value.clone()));
        }
        if !self.notification.is_empty() {
            for (field, value) in &self.notification {
                if field.is_empty() || !field.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                    return Err(format!("`{field}` isn't an FCM notification field (like title, body or channel_id)"));
                }
                out.push((format!("gcm.n.{}", wire_field(field)), value.clone()));
            }
            out.push(("gcm.n.e".into(), "1".into()));
        }
        Ok(out)
    }
}

/// A notification field's key suffix on the wire: FCM v1's `AndroidNotification` names, except
/// those the SDK reads under another name (`channel_id` is `android_channel_id`).
fn wire_field(field: &str) -> &str {
    match field {
        "channel_id" => "android_channel_id",
        other => other,
    }
}

/// A broadcast extra's value, by the `am` flag that sends it.
#[derive(Clone, Debug, PartialEq)]
pub enum Extra {
    /// `--es`.
    Str(String),
    /// `--esn`: a null string.
    Null,
    /// `--ez`.
    Bool(bool),
    /// `--ei`.
    Int(i32),
    /// `--el`.
    Long(i64),
    /// `--ef`.
    Float(f64),
}

impl Extra {
    /// `am`'s arguments for this extra under `key`, quoted for the guest's shell.
    fn args(&self, key: &str) -> String {
        let key = shell_quote(key);
        match self {
            Extra::Str(s) => format!("--es {key} {}", shell_quote(s)),
            Extra::Null => format!("--esn {key}"),
            Extra::Bool(b) => format!("--ez {key} {b}"),
            Extra::Int(n) => format!("--ei {key} {n}"),
            Extra::Long(n) => format!("--el {key} {n}"),
            Extra::Float(f) => format!("--ef {key} {f}"),
        }
    }
}

/// `am broadcast`'s arguments: the action or component, the package and the extras.
fn broadcast_args(target: &str, extras: &[(String, Extra)]) -> String {
    let mut line = format!("am broadcast {target}");
    for (key, value) in extras {
        line.push(' ');
        line.push_str(&value.args(key));
    }
    line
}

/// What `am broadcast` printed, as an outcome: it exits 0 or not depending on the release, and
/// says `Broadcast completed` only when the broadcast was sent.
fn broadcast_result(what: &str, out: &str) -> Result<(), String> {
    if out.lines().any(|l| l.trim_start().starts_with("Broadcast completed")) {
        return Ok(());
    }
    let reason = out.lines().map(str::trim).find(|l| l.to_ascii_lowercase().contains("exception") || l.starts_with("Error"));
    Err(failure(what, reason.unwrap_or(out)))
}

impl Adb {
    /// `package`'s FCM receiver (`com.example.app/com.google.firebase.iid.FirebaseInstanceIdReceiver`).
    pub fn fcm_receiver(&self, package: &str) -> Result<String, String> {
        let package = package_name(package)?;
        let found =
            self.shell(&format!("cmd package query-receivers --brief --components -a {FCM_RECEIVE} {package}"), self.deadlines.query)?;
        launcher_component(&found, package).ok_or_else(|| {
            format!("{package} has no receiver for {FCM_RECEIVE}: it doesn't use Firebase Cloud Messaging (or isn't installed; `apps` lists what is)")
        })
    }

    /// Delivers `message` to `package`'s FCM receiver as Play services would, as root. An app
    /// that was force-stopped gets nothing until it is started again, as with real FCM.
    pub fn push(&self, package: &str, message: &FcmMessage) -> Result<(), String> {
        let extras: Vec<(String, Extra)> = message.extras()?.into_iter().map(|(k, v)| (k, Extra::Str(v))).collect();
        let receiver = self.fcm_receiver(package)?;
        let am = broadcast_args(&format!("-n {} -a {FCM_RECEIVE}", shell_quote(&receiver)), &extras);
        let line = format!("if [ \"$(id -u)\" = 0 ]; then {am}; else su 0 {am}; fi");
        let (_, out) = self.run(&["shell", &line], self.deadlines.query)?;
        if out.contains("su:") && (out.contains("not found") || out.contains("inaccessible")) {
            return Err(format!(
                "delivering an FCM message needs root (the receiver only takes {FCM_RECEIVE} from Google Play services): adbd isn't \
                 root and `su 0` isn't there; use a google_apis image (it has su) or run `adb -s {} root`",
                self.serial
            ));
        }
        broadcast_result("push", &out)
    }

    /// `am broadcast -a <action>`, to `package`'s receivers or every registered one, as the shell
    /// user: a protected system broadcast is refused, and since Android 8 a receiver declared in
    /// a manifest only hears an implicit broadcast that names its package.
    pub fn broadcast(&self, action: &str, package: Option<&str>, extras: &[(String, Extra)]) -> Result<(), String> {
        if action.is_empty() || action.contains(['\n', '\r', '\0']) {
            return Err(format!("`{action}` isn't a broadcast action"));
        }
        if let Some((key, _)) = extras.iter().find(|(k, _)| k.is_empty() || k.contains(['\n', '\r', '\0'])) {
            return Err(format!("`{key}` isn't an extra's name"));
        }
        let mut target = format!("-a {}", shell_quote(action));
        if let Some(package) = package {
            target.push_str(&format!(" -p {}", package_name(package)?));
        }
        let (_, out) = self.run(&["shell", &broadcast_args(&target, extras)], self.deadlines.query)?;
        broadcast_result("broadcast", &out)
    }
}

/// The finger `biometric`'s `nomatch` touches unless told one: the emulator's own controls enroll
/// fingers 1 to 10, so 99 is enrolled only by a test that chose it.
pub const NOMATCH_FINGER: u32 = 99;

impl Device {
    /// A finger with id `id` (at least 1) touches the fingerprint sensor and is lifted: the
    /// console's `finger touch <id>` and `finger remove`. The guest's HAL reports a match when
    /// `id` is enrolled, a failed attempt when it isn't, and during enrollment enrolls it.
    pub fn finger_touch(&self, id: u32) -> Result<(), String> {
        if id == 0 {
            return Err("a finger id is at least 1".into());
        }
        self.console(&[format!("finger touch {id}"), "finger remove".to_owned()]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(p: &[(&str, &str)]) -> Vec<(String, String)> {
        p.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    #[test]
    fn an_fcm_message_is_the_extras_play_services_sends() {
        let m = FcmMessage { data: pairs(&[("order", "42")]), notification: pairs(&[("title", "Hi"), ("body", "It's here")]) };
        assert_eq!(m.extras(), Ok(pairs(&[("order", "42"), ("gcm.n.title", "Hi"), ("gcm.n.body", "It's here"), ("gcm.n.e", "1")])));
        let data_only = FcmMessage { data: pairs(&[("k", "v")]), notification: Vec::new() };
        assert_eq!(data_only.extras(), Ok(pairs(&[("k", "v")])), "a data message has no notification marker");
    }

    #[test]
    fn a_notification_channel_reaches_the_key_the_sdk_reads() {
        for field in ["channel_id", "android_channel_id"] {
            let m = FcmMessage { data: Vec::new(), notification: pairs(&[(field, "orders")]) };
            assert_eq!(m.extras(), Ok(pairs(&[("gcm.n.android_channel_id", "orders"), ("gcm.n.e", "1")])), "{field}");
        }
    }

    #[test]
    fn fcm_messages_are_refused_as_the_fcm_server_refuses_them() {
        assert!(FcmMessage::default().extras().expect_err("empty").contains("needs `data` or `notification`"));
        for key in ["from", "message_type", "collapse_key", "google.x", "gcm.n.e", ""] {
            let m = FcmMessage { data: pairs(&[(key, "v")]), notification: Vec::new() };
            assert!(m.extras().is_err(), "{key}");
        }
        let m = FcmMessage { data: Vec::new(), notification: pairs(&[("Title; rm", "x")]) };
        assert!(m.extras().expect_err("field").contains("isn't an FCM notification field"));
    }

    #[test]
    fn extras_are_quoted_with_their_am_flags() {
        let extras = [
            ("s".to_owned(), Extra::Str("it's".into())),
            ("n".to_owned(), Extra::Null),
            ("b".to_owned(), Extra::Bool(true)),
            ("i".to_owned(), Extra::Int(-3)),
            ("l".to_owned(), Extra::Long(1 << 40)),
            ("f".to_owned(), Extra::Float(0.5)),
        ];
        assert_eq!(
            broadcast_args("-a 'x'", &extras),
            r"am broadcast -a 'x' --es 's' 'it'\''s' --esn 'n' --ez 'b' true --ei 'i' -3 --el 'l' 1099511627776 --ef 'f' 0.5"
        );
    }

    #[test]
    fn am_broadcast_outcomes() {
        assert_eq!(broadcast_result("b", "Broadcasting: Intent { act=x flg=0x400000 }\nBroadcast completed: result=0\n"), Ok(()));
        let denied = "Broadcasting: Intent { act=android.intent.action.BOOT_COMPLETED flg=0x400000 }\n\
                      Security exception: Permission Denial: not allowed to send broadcast android.intent.action.BOOT_COMPLETED from pid=1, uid=2000\n\
                      \njava.lang.SecurityException: Permission Denial\n";
        assert_eq!(
            broadcast_result("b", denied),
            Err("b failed: Security exception: Permission Denial: not allowed to send broadcast android.intent.action.BOOT_COMPLETED from pid=1, uid=2000".into())
        );
        assert_eq!(broadcast_result("b", ""), Err("b failed: no output".into()));
    }

    /// Against `testing`'s fake adb and emulator, which are unix only.
    #[cfg(unix)]
    mod adb {
        use std::path::PathBuf;

        use super::super::*;
        use crate::Android;
        use crate::testing::{FakeAdb, FakeEmulator, fake_adb, fake_emulator};

        struct Fixture {
            root: PathBuf,
            android: Android,
            adb: Adb,
            calls: FakeAdb,
            emulator: FakeEmulator,
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }

        fn fixture(tag: &str) -> Fixture {
            let root = PathBuf::from("/tmp").join(format!("sa-notify-{tag}-{}", std::process::id()));
            let image = root.join("sdk/system-images/android-36/google_apis/arm64-v8a");
            std::fs::create_dir_all(&image).expect("image dir");
            std::fs::write(image.join("system.img"), b"s").expect("system.img");
            std::fs::write(image.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
            let mut android = Android::new(emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") });
            android.running_dir = Some(root.join("running"));
            android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
            let calls = fake_adb(&android);
            let emulator = fake_emulator(&android, "phone");
            let adb = android.device("phone").expect("device").adb(&android).expect("adb");
            Fixture { root, android, adb, calls, emulator }
        }

        #[test]
        fn push_finds_the_fcm_receiver_and_broadcasts_as_root() {
            let f = fixture("push");
            let m = FcmMessage { data: vec![("order".into(), "42".into())], notification: vec![("title".into(), "Hi".into())] };
            assert_eq!(f.adb.push("com.example.app", &m), Ok(()));
            let am = "am broadcast -n 'com.example.app/com.google.firebase.iid.FirebaseInstanceIdReceiver' -a com.google.android.c2dm.intent.RECEIVE \
                      --es 'order' '42' --es 'gcm.n.title' 'Hi' --es 'gcm.n.e' '1'";
            assert_eq!(
                f.calls.calls(),
                [
                    "shell cmd package query-receivers --brief --components -a com.google.android.c2dm.intent.RECEIVE com.example.app"
                        .to_owned(),
                    format!("shell if [ \"$(id -u)\" = 0 ]; then {am}; else su 0 {am}; fi"),
                ]
            );
        }

        #[test]
        fn push_says_why_it_cant_deliver() {
            let f = fixture("pushfail");
            let m = FcmMessage { data: vec![("k".into(), "v".into())], notification: Vec::new() };
            let err = f.adb.push("com.example.zeta", &m).expect_err("no receiver");
            assert!(err.starts_with("com.example.zeta has no receiver for com.google.android.c2dm.intent.RECEIVE"), "{err}");
            assert!(f.adb.push("not a package", &m).expect_err("name").contains("isn't a package name"));
            assert!(f.adb.push("com.example.app", &FcmMessage::default()).is_err());
            assert_eq!(f.calls.calls().len(), 1, "only the receiver lookup ran: {:?}", f.calls.calls());
            std::fs::write(f.adb.adb.with_file_name("noroot"), "").expect("noroot");
            let err = f.adb.push("com.example.app", &m).expect_err("no root");
            assert!(err.starts_with("delivering an FCM message needs root"), "{err}");
        }

        #[test]
        fn broadcast_sends_the_action_with_its_package_and_extras() {
            let f = fixture("broadcast");
            let extras = [("count".to_owned(), Extra::Int(3))];
            assert_eq!(f.adb.broadcast("com.example.REFRESH", Some("com.example.app"), &extras), Ok(()));
            assert_eq!(f.adb.broadcast("com.example.REFRESH", None, &[]), Ok(()));
            let err = f.adb.broadcast("android.intent.action.BOOT_COMPLETED", None, &[]).expect_err("protected");
            assert!(err.starts_with("broadcast failed: Security exception: Permission Denial"), "{err}");
            assert!(f.adb.broadcast("", None, &[]).is_err());
            assert!(f.adb.broadcast("a.b", Some("bad pkg"), &[]).is_err());
            assert!(f.adb.broadcast("a.b", None, &[(String::new(), Extra::Null)]).is_err());
            assert_eq!(
                f.calls.calls(),
                [
                    "shell am broadcast -a 'com.example.REFRESH' -p com.example.app --ei 'count' 3",
                    "shell am broadcast -a 'com.example.REFRESH'",
                    "shell am broadcast -a 'android.intent.action.BOOT_COMPLETED'",
                ]
            );
        }

        #[test]
        fn a_finger_touches_the_sensor_and_lifts() {
            let f = fixture("finger");
            let d = f.android.device("phone").expect("device");
            assert_eq!(d.finger_touch(1), Ok(()));
            assert_eq!(d.finger_touch(0), Err("a finger id is at least 1".into()));
            assert_eq!(*f.emulator.console.lock().expect("console"), ["finger touch 1", "finger remove"]);
        }
    }
}
