//! A stand-in for a running simee, for tests here and in crates that drive Android devices
//! (feature `testing`): an advertisement naming this process, a console that records commands
//! and answers like simee, and a display socket that records input lines.
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

use sim_core::frame::Frame;

use crate::Android;

/// What a fake emulator received.
pub struct FakeEmulator {
    /// Console commands, in order.
    pub console: Arc<Mutex<Vec<String>>>,
    /// Lines written to the display socket (touches, keys, wheel), in order.
    pub display: Arc<Mutex<Vec<String>>>,
    /// What `fold status` answers before its `OK` (simee's hinge report); None: a bare `OK`.
    pub fold_status: Arc<Mutex<Option<String>>>,
    /// What simee's `rotation` answers: degrees the guest rotated its display (0 at first).
    pub rotation: Arc<Mutex<u32>>,
    /// What each camera is linked to (`camera list`): `camera link` sets it, and a test can seed
    /// it. Empty: `camera list` answers a bare `OK`.
    pub cameras: Arc<Mutex<Vec<(String, String)>>>,
    /// A facing (`back`, `front`) whose `camera link` fails with `KO`; None: none does.
    pub camera_link_fails: Arc<Mutex<Option<String>>>,
    /// The NAT's conditions, as `network status` reports them: a test can seed them (a loss the
    /// API's integer percent can't ask for).
    pub network: Arc<Mutex<sim_core::network::Conditions>>,
    /// While true, `network loss` fails with `KO` (an emu that has the command but errs).
    pub loss_fails: Arc<std::sync::atomic::AtomicBool>,
}

/// Starts a fake emulator for AVD `avd` and advertises it in `android`'s running dir, so the AVD
/// lists as booted. `screenshot <path>` writes a 2x1 PNG; `car get`/`car list` answer with
/// vehicle HAL readings.
pub fn fake_emulator(android: &Android, avd: &str) -> FakeEmulator {
    fake(android, avd, false, false)
}

/// [`fake_emulator`] as the stock emulator: advertised without `emulator=simee`, and simee's own
/// console commands (`fold status`, `network loss`) get stock's refusal.
pub fn fake_stock_emulator(android: &Android, avd: &str) -> FakeEmulator {
    fake(android, avd, true, false)
}

/// [`fake_emulator`] as a simee emu built before `network loss`: advertised `emulator=simee`, but
/// its console answers `network loss` with stock's `KO:  bad sub-command`.
pub fn fake_old_emulator(android: &Android, avd: &str) -> FakeEmulator {
    fake(android, avd, false, true)
}

fn fake(android: &Android, avd: &str, stock: bool, no_loss: bool) -> FakeEmulator {
    let console = Arc::new(Mutex::new(Vec::new()));
    let fold_status: Arc<Mutex<Option<String>>> = Arc::default();
    let status = fold_status.clone();
    let rotation = Arc::new(Mutex::new(0));
    let turned = rotation.clone();
    let cameras: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let camera_link_fails: Arc<Mutex<Option<String>>> = Arc::default();
    let (links, fails) = (cameras.clone(), camera_link_fails.clone());
    // `network speed|delay|status`, shared by every console session as the emulator's NAT is.
    let network: Arc<Mutex<sim_core::network::Conditions>> = Arc::default();
    let shared_network = network.clone();
    let loss_fails: Arc<std::sync::atomic::AtomicBool> = Arc::default();
    let loss_broken = loss_fails.clone();
    // The advertised adb port is the console's plus one, so the console must not be on 65535 (an
    // ephemeral port the OS hands out now and then): bind again, keeping the unusable one held so
    // the next bind gets another.
    let mut skipped = Vec::new();
    let listener = loop {
        let l = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("console");
        if l.local_addr().expect("addr").port() < u16::MAX {
            break l;
        }
        skipped.push(l);
    };
    drop(skipped);
    let port = listener.local_addr().expect("addr").port();
    let log = console.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            // A thread per connection, so concurrent clients are served concurrently.
            let (log, status, turned) = (log.clone(), status.clone(), turned.clone());
            let (links, fails, network) = (links.clone(), fails.clone(), network.clone());
            let loss_broken = loss_broken.clone();
            std::thread::spawn(move || {
                let mut out = stream.try_clone().expect("clone");
                let _ = out.write_all(b"Android Console: type 'help' for a list of commands\r\nOK\r\n");
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    let line = line.trim().to_owned();
                    if line == "quit" {
                        break;
                    }
                    let reply = match line.split_whitespace().collect::<Vec<_>>().as_slice() {
                        // Authentication is the session's, not a command: answered, never recorded
                        // (clients send it whenever `~/.emulator_console_auth_token` exists).
                        ["auth", ..] => {
                            let _ = out.write_all(b"Android Console: type 'help' for a list of commands\r\nOK\r\n");
                            continue;
                        }
                        ["ping"] => "I am alive!\r\nOK".to_owned(),
                        ["frames"] => "3\r\nOK".to_owned(),
                        ["rotation"] if stock => "KO: unknown command, try 'help'".to_owned(),
                        ["rotation"] => format!("rotation: {}\r\nOK", turned.lock().expect("rotation")),
                        ["fold", "status"] if stock => "KO: Usage: \"fold\"".to_owned(),
                        ["fold", "status"] => match status.lock().expect("status").as_deref() {
                            Some(lines) => format!("{lines}\r\nOK"),
                            None => "OK".to_owned(),
                        },
                        ["car", "get", ..] => "property=GEAR_SELECTION id=0x11400400 area=0 status=available value=8\r\nOK".to_owned(),
                        ["car", "list"] => "property=GEAR_SELECTION id=0x11400400 area=0 status=available value=8\r\n\
                                        property=INFO_MAKE id=0x11100101 area=0 status=available value=Toy Car\r\nOK"
                            .to_owned(),
                        ["camera", "list"] => {
                            let mut text = String::new();
                            for (facing, source) in links.lock().expect("cameras").iter() {
                                text.push_str(&format!("{facing}: {source}\r\n"));
                            }
                            format!("{text}OK")
                        }
                        // Like emu's console: the source is the rest of the line, its whitespace
                        // collapsed to single spaces.
                        ["camera", "link", facing, source @ ..] if !source.is_empty() => {
                            if fails.lock().expect("fails").as_deref() == Some(*facing) {
                                "KO: camera link failed".to_owned()
                            } else {
                                let mut links = links.lock().expect("cameras");
                                links.retain(|(f, _)| f != facing);
                                // The stock values are the test card, as emu reports them.
                                let source = match source.join(" ").as_str() {
                                    "emulated" | "virtualscene" => "pattern:testcard".to_owned(),
                                    other => other.to_owned(),
                                };
                                links.push(((*facing).to_owned(), source));
                                "OK".to_owned()
                            }
                        }
                        // As emu's console (and stock's) answer them.
                        ["network", "status"] => format!("{}\r\nOK", network.lock().expect("network").status()),
                        ["network", "speed", speed] => match network.lock().expect("network").set_speed(speed) {
                            true => "OK".to_owned(),
                            false => "KO: invalid <speed> argument, see 'help network speed' for valid values".to_owned(),
                        },
                        ["network", "delay", delay] => match network.lock().expect("network").set_latency(delay) {
                            true => "OK".to_owned(),
                            false => "KO: invalid <delay> argument, see 'help network delay' for valid values".to_owned(),
                        },
                        // simee's own: stock's console has no such sub-command.
                        ["network", "loss", ..] if stock || no_loss => "KO:  bad sub-command".to_owned(),
                        ["network", "loss", ..] if loss_broken.load(std::sync::atomic::Ordering::SeqCst) => {
                            "KO: network loss failed".to_owned()
                        }
                        ["network", "loss", loss @ ..] => match network.lock().expect("network").set_loss(&loss.join(" ")) {
                            Ok(()) => "OK".to_owned(),
                            Err(why) => format!("KO: invalid <loss> argument: {why}; use <percent> or <up>:<down>"),
                        },
                        ["screenshot", path] => {
                            let frame = Frame { width: 2, height: 1, rgba: vec![9, 9, 9, 255, 8, 8, 8, 255] };
                            std::fs::write(path, sim_core::frame::encode_png(&frame).expect("png")).expect("write png");
                            // The emulator takes a moment to encode, so overlapping screenshots overlap here.
                            sleep(Duration::from_millis(150));
                            format!("{path} (2x1, frame 3)\r\nOK")
                        }
                        _ => "OK".to_owned(),
                    };
                    log.lock().expect("log").push(line);
                    let _ = out.write_all(format!("{reply}\r\n").as_bytes());
                }
            });
        }
    });
    let display = Arc::new(Mutex::new(Vec::new()));
    let dir = android.env.avd_home.join(format!("{avd}.avd/simee"));
    std::fs::create_dir_all(&dir).expect("simee dir");
    let socket = std::os::unix::net::UnixListener::bind(dir.join("display.sock")).expect("display");
    let lines = display.clone();
    std::thread::spawn(move || {
        for conn in socket.incoming().flatten() {
            let lines = lines.clone();
            std::thread::spawn(move || {
                for l in BufReader::new(conn).lines().map_while(Result::ok) {
                    lines.lock().expect("lines").push(l);
                }
            });
        }
    });
    let running = android.running_dir.clone().expect("running dir");
    std::fs::create_dir_all(&running).expect("running");
    let emulator = if stock { "" } else { "emulator=simee\n" };
    let ad = format!("port.serial={port}\nport.adb={}\navd.name={avd}\n{emulator}", port + 1);
    std::fs::write(running.join(format!("pid_{}.ini", std::process::id())), ad).expect("advertise");
    FakeEmulator { console, display, fold_status, rotation, cameras, camera_link_fails, network: shared_network, loss_fails }
}

/// `lines` once it holds `n` entries (or after 5 s, whatever it holds).
pub fn wait_for(lines: &Mutex<Vec<String>>, n: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while lines.lock().expect("lines").len() < n && Instant::now() < deadline {
        sleep(Duration::from_millis(10));
    }
    lines.lock().expect("lines").clone()
}

/// A stand-in `adb` in `android`'s SDK (`platform-tools/adb`, a shell script): it logs each
/// invocation and answers the app commands the way a device does, with the texts a real
/// API 36 guest prints. Package `com.example.app` is installed with a launcher activity (and
/// third-party), `com.missing.app` isn't, `broken.apk` fails to install, `com.example.crash`
/// has an activity that doesn't start, `com.example.svc` launches but never has a process (its
/// `pidof` exits 1, as toybox's does), `com.example.late`'s process shows up on the third
/// `pidof`, `com.example.leaky` has two launcher activities (LeakCanary's alias second), and
/// `am force-stop com.example.hang` never returns (it writes its PID to `hang.pid` beside the
/// script first). `ps` lists a guest's processes (init, zygote, system_server, the native daemons,
/// apps); `kill` succeeds (logged only: no host process is signalled) except on pids 520
/// (surfaceflinger) and 530 (cameraserver), which need `su 0` (present for `-9`, missing for 530's
/// `-15`); and an `exec` line, and its stop, run on the host's `sh`. Airplane mode is a file
/// beside the script that `cmd connectivity airplane-mode` (or `settings put`) sets; `sdk` beside it sets the API level (36 without it) and `settings get` reads; the
/// network listings (`sim_android::net`) answer with a root listing of the kernel's socket tables
/// and an eBPF per-uid stats dump. The settle probe
/// (`sim_android::settle::PROBE`) answers a booted guest whose uptime moves 1 s per probe and whose
/// system_server uses 60% of a core between the first two probes and 2% after (the probe's own
/// shell and `cat` busy throughout, as they are left out). The package manager has the `slim`
/// profile's `apps.turbo`, `apps.wellbeing` and `partnersetup` (which it refuses to disable) and
/// not `apps.restore`; what is disabled is kept in `disabled` beside the script.
pub fn fake_adb(android: &Android) -> FakeAdb {
    let dir = android.env.sdk_root.as_ref().expect("sdk root").join("platform-tools");
    std::fs::create_dir_all(&dir).expect("platform-tools");
    write_script(&dir.join("adb"), FAKE_ADB);
    FakeAdb { log: dir.join("adb.log") }
}

/// Writes `body` as an executable `/bin/sh` script at `path`, ready to run: written under a
/// temporary name and renamed into place, then run once (a probe the script answers by exiting)
/// until Linux stops refusing it as busy. Another test thread's fork while the file was open for
/// writing holds it open in that child until the child's exec, and an exec of the script in that
/// window fails with ETXTBSY ("Text file busy"); once one exec succeeds, no process can still
/// hold it open for writing.
pub fn write_script(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().expect("script name").to_string_lossy();
    let temp = path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    std::fs::write(&temp, format!("#!/bin/sh\n[ -n \"$SIMEE_FAKE_PROBE\" ] && exit 0\n{body}")).expect("script");
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::fs::rename(&temp, path).expect("rename script");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::process::Command::new(path).env("SIMEE_FAKE_PROBE", "1").status() {
            Ok(_) => return,
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && Instant::now() < deadline => sleep(Duration::from_millis(5)),
            Err(e) => panic!("couldn't run {}: {e}", path.display()),
        }
    }
}

/// What a [`fake_adb`] was asked.
pub struct FakeAdb {
    log: std::path::PathBuf,
}

impl FakeAdb {
    /// Each invocation's arguments, in order, without the leading `-s <serial>`.
    pub fn calls(&self) -> Vec<String> {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        text.lines().map(|l| l.split_once(' ').and_then(|(_, rest)| rest.split_once(' ')).map_or(l, |(_, r)| r).to_owned()).collect()
    }
}

const FAKE_ADB: &str = r#"echo "$*" >> "$(dirname "$0")/adb.log"
[ "$1" = "-s" ] && shift 2
# What an API 36 guest's toybox `ps -A -o PID,PPID,%CPU,RSS,ETIME,NAME,CMDLINE` prints (trimmed).
ps_rows() {
  printf '%s\n' \
'  PID  PPID %CPU    RSS ELAPSED NAME                    CMDLINE' \
'    1     0  0.0  11304   15:02 init                    /system/bin/init second_stage' \
'    2     0  0.0      0   15:02 [kthreadd]              [kthreadd]' \
'  310     1  0.1   2620   14:58 zygote64                zygote64' \
'  311     1  0.0   2100   14:58 zygote                  zygote' \
'  612   310  3.2 301520   14:50 system_server           system_server' \
'  520     1  0.4  20480   14:58 surfaceflinger          /system/bin/surfaceflinger --flag' \
'  200     1  0.0   4012   15:00 servicemanager          /system/bin/servicemanager' \
'  201     1  0.0   4400   15:00 hwservicemanager        /system/system_ext/bin/hwservicemanager' \
'  202     1  0.0   3300   15:00 vndservicemanager       /vendor/bin/vndservicemanager /dev/vndbinder' \
'  203     1  0.0   9120   15:00 vold                    /system/bin/vold --blkid_context=u:r:blkid:s0' \
'  204     1  0.0   3010   15:00 lmkd                    /system/bin/lmkd' \
'  205     1  0.1   8200   15:00 logd                    /system/bin/logd' \
'  206     1  0.2   7300   15:00 adbd                    /apex/com.android.adbd/bin/adbd --root_seclabel=u:r:su:s0' \
'  207     1  0.0   9300   15:00 netd                    /system/bin/netd' \
'  208     1  0.0   9400   15:00 installd                /system/bin/installd' \
'  209     1  0.0   2200   15:01 ueventd                 /system/bin/ueventd' \
'  210     1  0.0   9500   15:00 keystore2               /system/bin/keystore2 /data/misc/keystore' \
'  211     1  0.0   9600   15:00 statsd                  /apex/com.android.os.statsd/bin/statsd' \
'  530     1  0.0  12000   14:58 cameraserver            /system/bin/cameraserver' \
'  900   310  1.5 180224   14:40 com.android.systemui    com.android.systemui' \
' 4321   310  7.9 120332   00:42 com.example.app         com.example.app' \
' 4400   310  7.9  60112   00:40 com.example.app:remote  com.example.app:remote' \
' 4500   310  0.0  50000   00:30 com.google.android.gms.persistent com.google.android.gms.persistent'
}
# The network listings' tables: an API 36 guest's /proc/net, fds, ps and pm (trimmed).
net_owners() {
  printf '%s\n' '@net:ps' '  PID   UID USER           NAME' '    1     0 root           init' \
'  612  1000 system         system_server' ' 4321 10086 u0_a86         com.example.app' \
'@net:packages' 'package:com.example.app uid:10086' 'package:android uid:1000' 'package:com.android.settings uid:1000'
}
net_sockets() {
  printf '%s\n' '@net:root' '@net:tcp' \
'  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode' \
'   0: 0F02000A:A1B2 2E2E3AD8:01BB 01 00000000:00000000 02:000A7B2C 00000000 10086        0 18002 2 0000000000000000 20 4 30 10 -1' \
'@net:tcp6' \
'  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode' \
'   0: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 18003 1 0000000000000000 100 0 0 10 0' \
'   1: 0000000000000000FFFF00000F02000A:B000 0000000000000000FFFF00002E2E3AD8:01BB 01 00000000:00000000 02:000A7B2C 00000000 10086        0 18004 2 0000000000000000 20 4 30 10 -1' \
'@net:udp' \
'  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops' \
'  100: 00000000:14E9 00000000:0000 07 00000000:00000000 00:00000000 00000000  1021        0 18005 2 0000000000000000 0' \
'@net:udp6' '@net:fd' '/proc/612/fd:' 'lrwx------ 1 system system 64 2026-11-28 10:00 300 -> socket:[18003]' '' \
'/proc/4321/fd:' 'lrwx------ 1 u0_a86 u0_a86 64 2026-11-28 10:00 57 -> socket:[18002]' \
'lrwx------ 1 u0_a86 u0_a86 64 2026-11-28 10:00 58 -> socket:[18004]'
  net_owners
}
net_traffic() {
  printf '%s\n' '@net:bpf' 'BPF map status:' '  mAppUidStatsMap: OK' '  mStatsMapA: OK' 'TrafficController:' '  mAppUidStatsMap:' '    uid rxBytes rxPackets txBytes txPackets' \
'    1000 4096 10 2048 8' '    10086 123456 100 23456 50' '  mStatsMapA:'
  net_owners
}
# One /proc/<pid>/stat line: pid, comm, ppid, utime and stime ticks.
stat() { echo "$1 ($2) S $3 1 1 0 -1 4194560 100 0 0 0 $4 $5 0 0 20 0 1 0 100 0 0"; }
d="$(dirname "$0")"
installed() { printf 'package:android\npackage:com.android.systemui\npackage:com.google.android.apps.turbo\npackage:com.google.android.apps.wellbeing\npackage:com.google.android.partnersetup\n'; }
case "$1" in
install)
  case "$*" in
    *broken.apk*) echo "Performing Streamed Install" >&2; echo "adb: failed to install broken.apk: Failure [INSTALL_PARSE_FAILED_NOT_APK: Failed to parse /data/app/vmdl1.tmp/base.apk]" >&2; exit 1;;
  esac
  echo "Performing Streamed Install"
  echo "Success";;
shell)
  case "$2" in
    "pm uninstall com.missing.app") echo "Failure [DELETE_FAILED_INTERNAL_ERROR]";;
    "pm uninstall "*) echo "Success";;
    "pm list packages -3") printf 'package:com.example.app\npackage:com.example.zeta\n';;
    "pm list packages; echo @disabled; pm list packages -d") installed; echo @disabled; cat "$d/disabled" 2>/dev/null || true;;
    "pm disable-user "*|"pm enable "*)
      echo "$2" | tr ';' '\n' | while read -r c; do
        p=${c##* }
        case "$c" in
          "pm disable-user --user 0 com.google.android.partnersetup")
            echo "Error: java.lang.IllegalArgumentException: Cannot disable a protected package: $p";;
          "pm disable-user --user 0 "*)
            grep -qx "package:$p" "$d/disabled" 2>/dev/null || echo "package:$p" >> "$d/disabled"
            echo "Package $p new state: disabled-user";;
          "pm enable --user 0 "*)
            grep -vx "package:$p" "$d/disabled" > "$d/disabled.tmp"; mv "$d/disabled.tmp" "$d/disabled"
            echo "Package $p new state: enabled";;
          "echo @disabled") echo @disabled;;
          "pm list packages -d") cat "$d/disabled" 2>/dev/null || true;;
        esac
      done;;
    "echo @self \$\$; cat /proc/uptime; getprop sys.boot_completed; cat /proc/[0-9]*/stat 2>/dev/null; true")
      n=$(cat "$d/settle.n" 2>/dev/null || echo 0); echo $((n + 1)) > "$d/settle.n"
      busy=$(( n == 0 ? 0 : 60 + (n - 1) * 2 ))
      echo "@self 7000"; echo "$n.00 1.00"; echo 1
      stat 1 init 0 10 20; stat 612 system_server 310 "$busy" 0; stat 900 ndroid.systemui 310 50 10
      stat 7000 sh 206 $((n * 100)) 0; stat 7001 cat 7000 $((n * 100)) 0;;
    "pm list packages -3 -f") printf 'package:/data/app/~~Ab==/com.example.app-Cd==/base.apk=com.example.app\n';;
    "pm list packages") printf 'package:com.android.settings\npackage:com.example.zeta\npackage:android\npackage:com.example.app\n';;
    # Device state (#418): settings and SystemUI's demo broadcasts succeed, com.example.app is in
    # the foreground, and a trim-memory signal reaches it.
    "am broadcast -a com.android.systemui.demo "*) printf 'Broadcasting: Intent { act=com.android.systemui.demo flg=0x400000 (has extras) }\nBroadcast completed: result=0\n';;
    "dumpsys activity activities | grep "*) echo '  topResumedActivity=ActivityRecord{6d5b2c1 u0 com.example.app/.MainActivity t12}';;
    "am send-trim-memory com.example.app RUNNING_CRITICAL") ;;
    "cmd package query-activities --brief --components -a android.intent.action.MAIN -c android.intent.category.LAUNCHER com.example.app") echo 'com.example.app/.MainActivity';;
    "cmd package query-activities --brief --components -a android.intent.action.MAIN -c android.intent.category.LAUNCHER com.example.crash") echo 'com.example.crash/.Gone';;
    "cmd package query-activities --brief --components -a android.intent.action.MAIN -c android.intent.category.LAUNCHER com.example.svc") echo 'com.example.svc/.Main';;
    "cmd package query-activities --brief --components -a android.intent.action.MAIN -c android.intent.category.LAUNCHER com.example.late") echo 'com.example.late/.Main';;
    "cmd package query-activities --brief --components -a android.intent.action.MAIN -c android.intent.category.LAUNCHER com.example.leaky")
      printf 'com.example.leaky/.MainActivity\ncom.example.leaky/leakcanary.internal.activity.LeakLauncherActivity\n';;
    "cmd package query-activities"*) echo "No activities found";;
    "am start"*"com.example.crash/.Gone"*)
      printf 'Starting: Intent { act=android.intent.action.MAIN cat=[android.intent.category.LAUNCHER] cmp=com.example.crash/.Gone }\nError type 3\nError: Activity class {com.example.crash/com.example.crash.Gone} does not exist.\n';;
    "am start"*"android.intent.action.VIEW"*"nowhere://"*)
      printf 'Starting: Intent { act=android.intent.action.VIEW dat=nowhere://x }\nError: Activity not started, unable to resolve Intent { act=android.intent.action.VIEW dat=nowhere://x flg=0x10000000 }\n';;
    "am start"*"android.intent.action.VIEW"*) printf 'Starting: Intent { act=android.intent.action.VIEW dat=https://example.com/ }\n';;
    "am start"*) printf 'Starting: Intent { act=android.intent.action.MAIN cat=[android.intent.category.LAUNCHER] cmp=com.example.app/.MainActivity }\nStatus: ok\nLaunchState: COLD\nActivity: com.example.app/.MainActivity\nTotalTime: 812\nWaitTime: 820\nComplete\n';;
    "pidof -s com.example.app") echo "4321";;
    "pidof -s com.example.late")
      probes="$(dirname "$0")/late.probes"
      echo x >> "$probes"
      [ $(wc -l < "$probes") -ge 3 ] && echo "7777" || exit 1;;
    "pidof -s "*) exit 1;;
    "am force-stop com.example.hang") echo $$ > "$(dirname "$0")/hang.pid"; exec sleep 10;;
    "am force-stop "*) ;;
    "ps -A -o PID,PPID,%CPU,RSS,ETIME,NAME,CMDLINE") ps_rows;;
    "ps -A -o PID,PPID,%CPU,RSS,ETIME,NAME,CMDLINE; echo @packages; pm list packages")
      ps_rows; echo @packages; printf 'package:android\npackage:com.android.systemui\npackage:com.example.app\n';;
    "kill -"*" 520"|"kill -"*" 530") echo "/system/bin/sh: kill: ${2##* }: Operation not permitted" >&2; exit 1;;
    "su 0 kill -9 520"|"su 0 kill -9 530") ;;
    "su 0 kill -15 530") echo "/system/bin/sh: su: inaccessible or not found" >&2; exit 127;;
    # Every other signal succeeds, and is only logged: the fake never signals a host process.
    "kill -"*) ;;
    "settings get global airplane_mode_on") cat "$(dirname "$0")/airplane" 2>/dev/null || echo 0;;
    "getprop ro.build.version.sdk") cat "$(dirname "$0")/sdk" 2>/dev/null || echo 36;;
    "settings put global airplane_mode_on "*) echo "${2##* }" > "$(dirname "$0")/airplane";;
    # After the airplane-mode arm above: case takes the first match.
    "settings put "*) ;;
    "am broadcast -a android.intent.action.AIRPLANE_MODE"*) echo "Broadcast completed: result=0";;
    "cmd connectivity airplane-mode enable") echo 1 > "$(dirname "$0")/airplane";;
    "cmd connectivity airplane-mode disable") echo 0 > "$(dirname "$0")/airplane";;
    # sim_android::net's socket listing, as root (google_apis has su), and its traffic counters.
    *"echo @net:fd;"*) net_sockets;;
    *"mAppUidStatsMap"*) net_traffic;;
    # exec's argv line runs on the host's sh, as the guest's would run it, in a child that outlives
    # this adb (as adbd's shell outlives an adb client that is stopped). With `exec.slow` beside the
    # script, adb takes a second to reach the device first.
    "r='simee_start() "*)
      [ -e "$(dirname "$0")/exec.slow" ] && sleep 1
      /bin/sh -c "$2" & wait $!; exit $?;;
    # exec's stop runs on the host's sh too: it signals only the group of the process exec started.
    "simee_start() "*) exec /bin/sh -c "$2";;
    *) echo "fake adb: unhandled: $2" >&2; exit 2;;
  esac;;
*) echo "fake adb: unhandled: $*" >&2; exit 2;;
esac
"#;
