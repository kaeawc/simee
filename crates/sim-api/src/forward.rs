//! When a client may send a method to a running server instead of running it itself: the
//! handshake (`identity`) and the methods that always run in the caller.
//!
//! The CLI forwards a command only to a server that is the same build, from the same executable
//! file (unchanged since the server started), with the same environment, and that answers the
//! handshake within [`HANDSHAKE_TIMEOUT`]. Anything else (an older or newer build, a server that
//! doesn't know `identity`, a suspended or wedged one) and the command runs in the caller.

use sim_core::net::UnixStream;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::Client;
use crate::methods::METHODS;

/// How long a client waits for a server to accept and answer the handshake before running the
/// command itself. A forwarded command afterwards has no timeout: a boot can take minutes.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);

/// Methods a client never forwards: they always run in the process that calls them.
///
/// - `boot` starts the Android emulator as a child of the calling process. Run on a server, every
///   emulator booted from the CLI would be the server's child, in its process group (Ctrl-C on
///   `simee serve` would kill them all), never waited for (a crashed one stays a zombie that
///   reads as running), and started from the server's build of `emu`.
/// - `network_shape`, `network_state`, `network_flows`, `network_clear`: the shaping proxy is a
///   thread of the process that started it, and the others read or stop that process's proxy.
/// - `audio_capture_start`, `audio_capture_stop`, `audio_mute`, `audio_state`: a capture or a mute
///   is a helper process the caller starts and owns; the others read or stop the caller's.
/// - `boot_many`: boots like `boot` (its threads and settle waits belong to the caller).
///
/// Everything else either has no host-side effect beyond the call or acts through the device
/// (its launchd spawns what `launch` and `exec` run), so it gives the same result on a matching
/// server. When unsure about a new method, add it here: running in the caller is always correct.
pub const IN_CALLER: &[&str] = &[
    "boot",
    "boot_many",
    "network_shape",
    "network_state",
    "network_flows",
    "network_clear",
    "audio_capture_start",
    "audio_capture_stop",
    "audio_mute",
    "audio_state",
];

/// Whether a client may send `method` to a server whose identity matches its own.
pub fn forwards(method: &str) -> bool {
    !IN_CALLER.contains(&method)
}

/// The environment variables a method's result can depend on, besides every `ANDROID_*`, `EMU_*`
/// and `SIMEE_*` one (SDK and AVD paths, the `emu` a backend runs, the shaper's settings): the
/// developer directory (which simulator service and tools), `HOME` (the default SDK and AVD
/// homes) and `TMPDIR` (where helpers are cached). Their values are hashed, not sent.
///
/// `PATH` is not one of them: a pyenv or nvm shim, or a shell that adds a directory, changes it
/// without changing what any method does. The one thing found through it, `adb` when the SDK has
/// none, is part of the environment as the path it resolves to.
const ENVIRONMENT: &[&str] = &["DEVELOPER_DIR", "HOME", "TMPDIR"];

/// Prefixes of variables that are part of the environment.
const ENVIRONMENT_PREFIXES: &[&str] = &["ANDROID_", "EMU_", "SIMEE_"];

/// `SIMEE_*` variables that only steer the client (where and whether to forward, test targets):
/// a server never reads them, so they don't keep a client from using it.
const CLIENT_ONLY: &[&str] = &["SIMEE_SOCKET", "SIMEE_IN_PROCESS", "SIMEE_REQUIRE_SERVER", "SIMEE_VERBOSE", "SIMEE_E2E_"];

/// Whether variable `key` is part of the environment a server must share with its clients.
fn in_environment(key: &str) -> bool {
    if CLIENT_ONLY.iter().any(|c| if c.ends_with('_') { key.starts_with(c) } else { key == *c }) {
        return false;
    }
    ENVIRONMENT.contains(&key) || ENVIRONMENT_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// The first `name` on `path` that is an executable file.
fn on_path(path: &std::ffi::OsStr, name: &str) -> Option<std::path::PathBuf> {
    use sim_core::hostfs::PermissionsExt;
    std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// The hashed form of an environment: the variables that matter, sorted, and the `adb` its `PATH`
/// finds.
fn environment_of(vars: impl IntoIterator<Item = (String, String)>, path: Option<&std::ffi::OsStr>) -> String {
    let mut kept: Vec<String> = vars.into_iter().filter(|(k, _)| in_environment(k)).map(|(k, v)| format!("{k}={v}")).collect();
    // A variable that is unset hashes as set to "", as before.
    for k in ENVIRONMENT {
        if !kept.iter().any(|kv| kv.split_once('=').is_some_and(|(key, _)| key == *k)) {
            kept.push(format!("{k}="));
        }
    }
    kept.sort();
    let adb = path.and_then(|p| on_path(p, "adb")).map(|p| p.display().to_string()).unwrap_or_default();
    kept.push(format!("adb@PATH={adb}"));
    format!("{:016x}", fnv(kept))
}

/// FNV-1a, 64-bit: a stable hash with no dependency.
fn fnv(parts: impl IntoIterator<Item = impl AsRef<[u8]>>) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for &b in part.as_ref().iter().chain(&[0]) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// The compiled-in build: the crate version and a hash of the method table (names, parameters,
/// descriptions).
pub fn build() -> String {
    let table = METHODS.iter().flat_map(|(m, p, d)| [*m, *p, *d]);
    format!("{}+{:016x}", env!("CARGO_PKG_VERSION"), fnv(table))
}

/// The executable this process runs, as it was when first asked. On macOS, its Mach-O UUID: the
/// linker derives it from the linked code, so any change to the code changes it, while a relink of
/// the same code (`cargo test` rebuilding the binary, a copy) keeps it. Elsewhere, or for a binary
/// without one, its path, inode, size and modification time, which any rebuild changes. Either way
/// a CLI rebuilt from changed code while a server runs doesn't match it.
fn executable() -> Option<String> {
    use sim_core::hostfs::MetadataExt;
    let path = std::env::current_exe().ok()?.canonicalize().ok()?;
    if let Some(uuid) = macho_uuid_of(&path) {
        return Some(format!("macho-uuid:{uuid}"));
    }
    let m = std::fs::metadata(&path).ok()?;
    Some(format!("{}:{}:{}:{}.{}", path.display(), m.ino(), m.len(), m.mtime(), m.mtime_nsec()))
}

/// The UUID (`LC_UUID`) of the Mach-O file at `path`, as hex; the first slice's of a universal file.
fn macho_uuid_of(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = vec![0; 64 * 1024];
    let n = file.read(&mut head).ok()?;
    head.truncate(n);
    let uuid = match macho_uuid(&head) {
        Some(u) => u,
        None => {
            let offset = fat_first_offset(&head)?;
            file.seek(SeekFrom::Start(offset)).ok()?;
            let mut slice = vec![0; 64 * 1024];
            let n = file.read(&mut slice).ok()?;
            slice.truncate(n);
            macho_uuid(&slice)?
        }
    };
    Some(uuid.iter().map(|b| format!("{b:02x}")).collect())
}

/// `LC_UUID` from the load commands of a 64-bit little-endian Mach-O image starting at `image[0]`.
fn macho_uuid(image: &[u8]) -> Option<[u8; 16]> {
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const LC_UUID: u32 = 0x1b;
    let u32_at = |off: usize| image.get(off..off + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if u32_at(0)? != MH_MAGIC_64 {
        return None;
    }
    let ncmds = u32_at(16)?;
    let mut off = 32usize;
    for _ in 0..ncmds {
        let (cmd, size) = (u32_at(off)?, u32_at(off + 4)? as usize);
        if cmd == LC_UUID {
            return image.get(off + 8..off + 24)?.try_into().ok();
        }
        if size < 8 {
            return None;
        }
        off += size;
    }
    None
}

/// The file offset of the first architecture's image in a universal (fat) Mach-O header.
fn fat_first_offset(head: &[u8]) -> Option<u64> {
    let u32_be = |off: usize| head.get(off..off + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    match u32_be(0)? {
        // fat_header, then fat_arch { cputype, cpusubtype, offset, size, align }
        0xcafe_babe if u32_be(4)? > 0 => Some(u64::from(u32_be(8 + 8)?)),
        // fat_arch_64 { cputype, cpusubtype, offset: u64, size: u64, align, reserved }
        0xcafe_babf if u32_be(4)? > 0 => Some((u64::from(u32_be(16)?) << 32) | u64::from(u32_be(20)?)),
        _ => None,
    }
}

fn environment() -> String {
    let vars = std::env::vars_os().map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()));
    environment_of(vars, std::env::var_os("PATH").as_deref())
}

/// This process's identity, fixed the first time it is asked (a server asks when it binds, so a
/// rebuild of its executable afterwards is seen as a different build).
pub fn identity() -> &'static Value {
    static IDENTITY: OnceLock<Value> = OnceLock::new();
    IDENTITY.get_or_init(|| json!({"build": build(), "exe": executable(), "env": environment()}))
}

impl Client {
    /// Connects to the server on `path` and checks that it is this build, from this executable,
    /// with this environment. Fails when it isn't, when it doesn't answer `identity` (a build from
    /// before the handshake), or when connecting and answering take longer than `timeout` (a
    /// suspended or wedged server: the kernel accepts the connection, nobody reads it).
    pub fn handshake(path: &Path, timeout: Duration) -> Result<Client, String> {
        let start = Instant::now();
        let ours = identity();
        if ours["exe"].is_null() {
            return Err("this process's executable is unknown".into());
        }
        // Connecting to a unix socket can block (a full backlog on Linux): connect on a thread and
        // stop waiting at the deadline. A thread still blocked then ends with the process.
        let (tx, rx) = std::sync::mpsc::channel();
        let target = path.to_owned();
        std::thread::Builder::new()
            .name("simee-connect".into())
            .spawn(move || drop(tx.send(UnixStream::connect(target))))
            .map_err(|e| e.to_string())?;
        let stream = rx.recv_timeout(timeout).map_err(|_| "connecting timed out".to_owned())?.map_err(|e| e.to_string())?;
        let left = timeout.saturating_sub(start.elapsed()).max(Duration::from_millis(1));
        stream.set_read_timeout(Some(left)).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(left)).map_err(|e| e.to_string())?;
        let mut client = Client::from_stream(stream);
        let theirs = client.call("identity", &Value::Null)?;
        if theirs != *ours {
            return Err(format!("the server is another build or environment: {theirs}"));
        }
        let stream = client.stream.get_ref();
        stream.set_read_timeout(None).map_err(|e| e.to_string())?;
        stream.set_write_timeout(None).map_err(|e| e.to_string())?;
        Ok(client)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn the_methods_run_in_the_caller_are_pinned() {
        // Changing this list changes which process owns a device's emulator, proxy or audio
        // helper: see `IN_CALLER`.
        assert_eq!(
            IN_CALLER,
            [
                "boot",
                "boot_many",
                "network_shape",
                "network_state",
                "network_flows",
                "network_clear",
                "audio_capture_start",
                "audio_capture_stop",
                "audio_mute",
                "audio_state"
            ]
        );
        for m in IN_CALLER {
            assert!(METHODS.iter().any(|(name, ..)| name == m), "`{m}` is a method");
            assert!(!forwards(m));
        }
        for m in ["identity", "devices", "tap", "screenshot", "launch", "exec", "create", "shutdown"] {
            assert!(forwards(m), "{m}");
        }
    }

    #[test]
    fn identity_names_this_build_executable_and_environment() {
        let id = identity();
        assert!(id["build"].as_str().is_some_and(|b| b.starts_with(env!("CARGO_PKG_VERSION"))));
        assert!(id["exe"].as_str().is_some_and(|e| e.contains(':')));
        assert_eq!(id["env"].as_str().map(str::len), Some(16));
        assert_ne!(fnv(["a", "b"]), fnv(["ab"]), "parts are delimited");
        let mut s = crate::Session::default();
        assert_eq!(crate::call_method(&mut s, "identity", &Value::Null).as_ref(), Ok(id));
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    #[test]
    fn path_and_client_only_variables_dont_change_the_environment() {
        let base = [("HOME", "/Users/u"), ("DEVELOPER_DIR", "/Applications/Xcode.app/Contents/Developer"), ("ANDROID_HOME", "/sdk")];
        let empty = std::env::temp_dir().join(format!("simee-env-empty-{}", std::process::id()));
        std::fs::create_dir_all(&empty).expect("dir");
        let path = |dirs: &[&Path]| std::env::join_paths(dirs).expect("path");
        let ours = environment_of(env(&base), Some(&path(&[&empty])));
        // A shim directory in front (pyenv), and the client's own switches: the same environment.
        let shim = std::env::temp_dir().join(format!("simee-env-shim-{}", std::process::id()));
        std::fs::create_dir_all(&shim).expect("dir");
        let mut with_client = env(&base);
        with_client.extend(env(&[("SIMEE_SOCKET", "/tmp/x.sock"), ("SIMEE_REQUIRE_SERVER", "1"), ("SIMEE_E2E_UDID", "U"), ("PYENV", "1")]));
        assert_eq!(environment_of(with_client, Some(&path(&[&shim, &empty]))), ours);
        // Order doesn't matter.
        let mut reversed = env(&base);
        reversed.reverse();
        assert_eq!(environment_of(reversed, Some(&path(&[&empty]))), ours);
        // What a method reads does.
        for (k, v) in [("DEVELOPER_DIR", "/Library/Developer/CommandLineTools"), ("ANDROID_HOME", "/other"), ("HOME", "/Users/v")] {
            let mut changed = env(&base);
            changed.retain(|(key, _)| key != k);
            changed.push((k.to_owned(), v.to_owned()));
            assert_ne!(environment_of(changed, Some(&path(&[&empty]))), ours, "{k}");
        }
        for (k, v) in [("ANDROID_AVD_HOME", "/avd"), ("EMU_BIN", "/emu"), ("SIMEE_NETSHAPE", "/p"), ("TMPDIR", "/t/")] {
            let mut more = env(&base);
            more.push((k.to_owned(), v.to_owned()));
            assert_ne!(environment_of(more, Some(&path(&[&empty]))), ours, "{k}");
        }
        // An `adb` that PATH now finds (the Android backend falls back to it) does too.
        let adb = shim.join("adb");
        std::fs::write(&adb, "#!/bin/sh\n").expect("adb");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        assert_ne!(environment_of(env(&base), Some(&path(&[&shim, &empty]))), ours, "adb on PATH");
        std::fs::remove_dir_all(shim).ok();
        std::fs::remove_dir_all(empty).ok();
    }

    /// A minimal 64-bit Mach-O header with `cmds` (cmd, payload) load commands.
    fn macho(cmds: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (cmd, payload) in cmds {
            body.extend(cmd.to_le_bytes());
            body.extend((8 + payload.len() as u32).to_le_bytes());
            body.extend(payload);
        }
        let mut image = Vec::new();
        for word in [0xfeed_facf, 0x0100_000c, 0, 2, cmds.len() as u32, body.len() as u32, 0, 0] {
            image.extend(u32::to_le_bytes(word));
        }
        image.extend(body);
        image
    }

    #[test]
    fn the_macho_uuid_is_read_from_the_load_commands() {
        let uuid: Vec<u8> = (1..=16).collect();
        let image = macho(&[(0x19, vec![0; 64]), (0x1b, uuid.clone()), (0x2, vec![0; 16])]);
        assert_eq!(macho_uuid(&image).map(|u| u.to_vec()), Some(uuid.clone()));
        assert_eq!(macho_uuid(&macho(&[(0x19, vec![0; 64])])), None, "no LC_UUID");
        assert_eq!(macho_uuid(b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0"), None, "not Mach-O");
        assert_eq!(macho_uuid(&image[..40]), None, "truncated");
        // A universal file: the first slice's.
        let mut fat = Vec::new();
        for word in [0xcafe_babe_u32, 1, 0x0100_000c, 0, 4096, image.len() as u32, 14] {
            fat.extend(word.to_be_bytes());
        }
        assert_eq!(fat_first_offset(&fat), Some(4096));
        let path = std::env::temp_dir().join(format!("simee-fat-{}", std::process::id()));
        fat.resize(4096, 0);
        fat.extend(&image);
        std::fs::write(&path, &fat).expect("write");
        assert_eq!(macho_uuid_of(&path), Some("0102030405060708090a0b0c0d0e0f10".to_owned()));
        std::fs::remove_file(path).ok();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_macos_executable_is_known_by_its_uuid_and_a_copy_matches() {
        let exe = std::env::current_exe().expect("exe");
        let uuid = macho_uuid_of(&exe).expect("the linker writes LC_UUID");
        assert_eq!(identity()["exe"], json!(format!("macho-uuid:{uuid}")));
        // A copy (a new inode, as a relink of the same code makes) has the same identity.
        let copy = std::env::temp_dir().join(format!("simee-exe-copy-{}", std::process::id()));
        std::fs::copy(&exe, &copy).expect("copy");
        assert_eq!(macho_uuid_of(&copy), Some(uuid));
        std::fs::remove_file(copy).ok();
    }

    fn socket(tag: &str) -> PathBuf {
        let path = crate::test_socket(&format!("simee-hs-{tag}-{}.sock", std::process::id()));
        std::fs::remove_file(&path).ok();
        path
    }

    /// A server that answers every request with `answer(method)`.
    fn answering(tag: &str, answer: fn(&str) -> String) -> PathBuf {
        let path = socket(tag);
        let listener = UnixListener::bind(&path).expect("bind");
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut writer = stream.try_clone().expect("clone");
                    for line in BufReader::new(stream).lines().map_while(Result::ok) {
                        let req: Value = serde_json::from_str(&line).expect("request");
                        let reply = answer(req["method"].as_str().unwrap_or(""));
                        writeln!(writer, "{reply}").expect("write");
                    }
                });
            }
        });
        path
    }

    #[test]
    fn a_matching_server_is_used() {
        let path = socket("match");
        crate::serve_in_background(path.clone()).expect("serve");
        let mut client = Client::handshake(&path, HANDSHAKE_TIMEOUT).expect("same build");
        assert_eq!(client.call("identity", &Value::Null).as_ref(), Ok(identity()));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn another_build_or_an_old_server_is_not_used() {
        let other = answering("other", |_| {
            let mut id = identity().clone();
            id["build"] = json!("0.0.0+0000000000000000");
            json!({"id": 1, "ok": true, "result": id}).to_string()
        });
        let err = Client::handshake(&other, HANDSHAKE_TIMEOUT).err().expect("another build");
        assert!(err.starts_with("the server is another build"), "{err}");
        let old = answering("old", |m| json!({"id": 1, "ok": false, "error": format!("unknown method `{m}`; call `help`")}).to_string());
        let err = Client::handshake(&old, HANDSHAKE_TIMEOUT).err().expect("no handshake");
        assert_eq!(err, "unknown method `identity`; call `help`");
        std::fs::remove_file(other).ok();
        std::fs::remove_file(old).ok();
    }

    #[test]
    fn a_server_that_never_answers_times_out() {
        // Accepts and never replies (wedged), and never accepts (suspended: the kernel's backlog
        // completes the connection).
        let wedged = socket("wedged");
        let listener = UnixListener::bind(&wedged).expect("bind");
        std::thread::spawn(move || {
            let held: Vec<_> = listener.incoming().flatten().collect();
            drop(held);
        });
        let suspended = socket("suspended");
        let _backlog = UnixListener::bind(&suspended).expect("bind");
        for path in [&wedged, &suspended] {
            let start = Instant::now();
            assert!(Client::handshake(path, Duration::from_millis(300)).is_err());
            let took = start.elapsed();
            assert!(took >= Duration::from_millis(250) && took < Duration::from_secs(1), "{took:?}");
        }
        std::fs::remove_file(wedged).ok();
        std::fs::remove_file(suspended).ok();
    }
}
