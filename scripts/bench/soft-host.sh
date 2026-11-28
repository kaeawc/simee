#!/usr/bin/env bash
# What a host without KVM and without a GPU (a cloud agent's VM, a CI container) can do with simee:
# the release build of emu, a software-mode boot under QEMU's TCG, what a booted device costs idle,
# and how fast a screenshot comes back.
#
#   scripts/bench/soft-host.sh [--cache DIR] [--qemu BIN | --build-qemu] [--image DIR] [--memory MB]
#                              [--lcd WxH@DPI] [--port N] [--calls N] [--emu BIN] [--simee BIN]
#                              [--skip-build] [--skip-boot] [--no-cold] [--no-quiet-gate]
#                              [--rounds N] [--quick] [--out DIR]
#
# Phases, each round where it says so:
#   host     scripts/bench/host-profile.sh, at the start and the end, beside the results
#            (<out>/soft-host-<time>.profile.json and .profile-end.json)
#   build    each round, into an empty CARGO_TARGET_DIR: `cargo build --release -p simee-emu`
#            (wall seconds, and CPU seconds, which load can't inflate), `-p simee` on top of it, and
#            the rebuild after touching apps/emu/src/main.rs. Dependencies are fetched first, untimed.
#            The binaries of the last round are the ones the boot phase runs.
#   qemu     once, when --build-qemu and there is no QEMU yet: QEMU 11.1.3 from its release tarball
#            (the version and SHA-256 .github/workflows/software-mode.yml pins), configured as that
#            workflow does. Software mode needs QEMU 11.1 or newer; Ubuntu 22.04 ships 6.2.
#   image    once, when the system image isn't in --cache's SDK: `emu -provision` (a download from
#            Google's repository: no credentials) and the renderer. Its time is network-bound.
#   boot     each round, a new AVD (4 vCPUs, --memory MB, default 1536) under `emu -avd -accel off`:
#            a cold boot (the first of the new userdata; --no-cold skips it), then a warm boot with
#            launch until adb lists the device, until sys.boot_completed, peak RSS, steal over the
#            boot (the host's /proc/stat), host CPU 60-120 s and 240-300 s after the boot (cores of
#            emu plus QEMU), settled RSS, then the time per step through `simee serve` (tap,
#            screenshot, tap and screenshot: scripts/ios-step-bench.py, --calls each) and
#            `adb exec-out screencap` (5 calls: it takes seconds under TCG).
#
# The host must be quiet: this script waits for the quiet-host gate (scripts/bench/quiet.sh, #468:
# a 1-minute load below 0.5 per CPU, no foreign emulators or compilers) before it starts and again
# before each build round after the first, before QEMU's build and before each round of boots, and
# exits 4 when the host stays busy (BENCH_QUIET_WAIT s, 300). --no-quiet-gate runs anyway and says so
# loudly: those numbers are not for publishing. On a host that is somebody's workstation, an agent
# is the load: don't work in the same VM while it measures.
#
# Everything it downloads or builds lands in --cache (default ~/.cache/simee-bench: qemu/, sdk/);
# nothing else on the host is touched, and the AVDs it creates are deleted by their own name.
# Phases that can't run say why and the run goes on: no QEMU (build one with --build-qemu), no
# network for the image, less than the free disk a phase needs. The note under the table lists them.
#
# On a host with a usable /dev/kvm this still boots with `-accel off`: software mode, what a host
# without KVM does. --quick: one round, short idle windows, 5 calls (a smoke test, not a number).
# --skip-boot: build only (no QEMU, no image). Needs x86_64 Linux, python3, perl, curl, and cargo unless --emu and --simee are given.
# Reproduces docs/research/cloud-hosts.md; the boot and idle rows are the ones of
# scripts/bench/software-mode.sh and api-steps.sh --android, taken the way a user starts a device.
set -uo pipefail
. "$(dirname "$0")/lib.sh"

# The pinned QEMU: the same release as .github/workflows/software-mode.yml.
QEMU_VERSION=11.1.3
QEMU_SHA256=5a8289c7940dc77d3873c81a6c29ba3afe1446fc9d36e77fa1658cdbecb84b22

cache=${SIMEE_BENCH_CACHE:-$HOME/.cache/simee-bench}
qemu=${EMU_QEMU:-}
build_qemu=0
image=system-images/android-36/default/x86_64
memory=1536
lcd=720x1280@320
port=5720
calls=20
skip_build=0
skip_boot=0
cold=1
BENCH_EMU=${BENCH_EMU:-}
BENCH_SIMEE=${BENCH_SIMEE:-}
while [ $# -gt 0 ]; do
  case $1 in
    --cache)
      cache=$2
      shift 2
      ;;
    --qemu)
      qemu=$2
      shift 2
      ;;
    --build-qemu)
      build_qemu=1
      shift
      ;;
    --image)
      image=$2
      shift 2
      ;;
    --memory)
      memory=$2
      shift 2
      ;;
    --lcd)
      lcd=$2
      shift 2
      ;;
    --port)
      port=$2
      shift 2
      ;;
    --calls)
      calls=$2
      shift 2
      ;;
    --emu)
      BENCH_EMU=$2
      shift 2
      ;;
    --simee)
      BENCH_SIMEE=$2
      shift 2
      ;;
    --skip-build)
      skip_build=1
      shift
      ;;
    --skip-boot)
      skip_boot=1
      shift
      ;;
    --no-cold)
      cold=0
      shift
      ;;
    *)
      bench_opt "$@"
      shift "$BENCH_SHIFT"
      ;;
  esac
done
[ "$BENCH_QUICK" = 1 ] && calls=5
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != x86_64 ]; then bench_skip "software mode is measured on x86_64 Linux"; fi
if [ -z "$BENCH_EMU" ] && ! command -v cargo >/dev/null; then bench_skip "no cargo (or give --emu and --simee)"; fi
[ -z "$qemu" ] && [ -x "$cache/qemu/bin/qemu-system-x86_64" ] && qemu=$cache/qemu/bin/qemu-system-x86_64
NOTES=()
note() {
  NOTES+=("$*")
  bench_log "NOTE: $*"
}

free_gb() { df -Pk "$1" | awk 'NR == 2 { printf "%d\n", $4 / 1048576 }'; }
# need_disk GB WHAT: returns 1, with a note, when the filesystem of $cache has less free.
need_disk() {
  local have
  have=$(free_gb "$cache")
  [ "$have" -ge "$1" ] && return 0
  note "$2 needs about $1 GB free on $cache's filesystem and it has $have GB: skipped"
  return 1
}

# The shared quiet gate (lib.sh's bench_gate: load, foreign emulators and compilers, thermal, power)
# before a phase after the first; bench_start runs the first. --no-quiet-gate skips it, loudly.
soft_gate() {
  [ "$BENCH_QUIET_GATE" = 0 ] && bench_log "WARNING: --no-quiet-gate: the host may be busy; these numbers are not for publishing"
  bench_gate
}

# CPU time counters of the whole host, for steal: "steal total" jiffies since boot.
stat_now() { awk '/^cpu / { t = 0; for (i = 2; i <= 9; i++) t += $i; print $9, t }' /proc/stat; }
steal_pct() {
  awk -v a="$1" -v b="$2" 'BEGIN { split(a, x, " "); split(b, y, " "); t = y[2] - x[2]; printf "%.2f", t ? 100 * (y[1] - x[1]) / t : 0 }'
}
# cpu_window PID SECONDS: cores used by PID's tree (emu and its QEMU) over SECONDS.
cpu_window() {
  local c0 c1 t0
  c0=$(cpu_s "$1")
  t0=$(now)
  sleep "$2"
  c1=$(cpu_s "$1")
  awk -v a="$c0" -v b="$c1" -v d="$(since "$t0" 3)" 'BEGIN { printf "%.2f", (b - a) / d }'
}

# --- host --------------------------------------------------------------------------------------
stamp=$(date +%Y%m%d-%H%M%S)
mkdir -p "$BENCH_OUT" "$cache"
bench_refuse_protected_port "$port"
bench_start soft-host
"$BENCH_DIR/host-profile.sh" >"$BENCH_OUT/soft-host-$stamp.profile.json" || bench_log "host-profile.sh failed"
mkdir -p "$BENCH_WORK/bin"
bench_log "host profile: $BENCH_OUT/soft-host-$stamp.profile.json"
if [ -e /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
  note "/dev/kvm is usable on this host; the boots still run with -accel off (software mode)"
fi
run_stat0=$(stat_now)

# --- build -------------------------------------------------------------------------------------
if [ "$skip_build" = 0 ] && [ -z "$BENCH_EMU" ]; then
  (cd "$BENCH_REPO" && cargo fetch --locked >"$BENCH_WORK/fetch.log" 2>&1) || bench_die "cargo fetch failed; see $BENCH_WORK/fetch.log"
  need_disk 3 "the release build" || skip_build=1
fi
if [ "$skip_build" = 0 ] && [ -z "$BENCH_EMU" ]; then
  target="$BENCH_WORK/target"
  for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
    [ "$BENCH_ROUND" -gt 1 ] && soft_gate
    rm -rf "$target"
    read -r wall cpu rc < <(cd "$BENCH_REPO" && CARGO_TARGET_DIR=$target cpu_timed cargo build --locked --release -p simee-emu)
    cp "$BENCH_WORK/cpu_timed.log" "$BENCH_WORK/build-emu-$BENCH_ROUND.log"
    [ "${rc:-1}" = 0 ] || {
      tail -20 "$BENCH_WORK/build-emu-$BENCH_ROUND.log" >&2
      bench_die "cargo build --release -p simee-emu failed"
    }
    bench_sample emu "cold release build of emu, wall s" "$wall"
    bench_sample emu "cold release build of emu, CPU s" "$cpu"
    read -r wall cpu rc < <(cd "$BENCH_REPO" && CARGO_TARGET_DIR=$target cpu_timed cargo build --locked --release -p simee)
    [ "${rc:-1}" = 0 ] || bench_die "cargo build --release -p simee failed; see $BENCH_WORK/cpu_timed.log"
    bench_sample emu "then the simee CLI on top, wall s" "$wall"
    touch "$BENCH_REPO/apps/emu/src/main.rs"
    read -r wall cpu rc < <(cd "$BENCH_REPO" && CARGO_TARGET_DIR=$target cpu_timed cargo build --locked --release -p simee-emu)
    [ "${rc:-1}" = 0 ] || bench_die "the rebuild failed; see $BENCH_WORK/cpu_timed.log"
    bench_sample emu "rebuild of emu after touching main.rs, wall s" "$wall"
    cp "$target/release/emu" "$target/release/simee" "$BENCH_WORK/bin/"
    bench_log "target dir: $(du -sm "$target" | cut -f1) MB"
    rm -rf "$target"
  done
  BENCH_EMU="$BENCH_WORK/bin/emu"
  BENCH_SIMEE="$BENCH_WORK/bin/simee"
fi

# --- QEMU --------------------------------------------------------------------------------------
build_qemu_now() {
  local prefix=$cache/qemu src=$cache/qemu-src wall cpu rc
  need_disk 3 "building QEMU" || return 1
  rm -rf "$src"
  mkdir -p "$src"
  # shellcheck disable=SC2016 # the script is bash's, and expands its own arguments.
  read -r wall cpu rc < <(cpu_timed bash -c '
    set -e
    cd "$1"
    curl -sSfLo qemu.tar.xz "https://download.qemu.org/qemu-$2.tar.xz"
    echo "$3  qemu.tar.xz" | sha256sum -c
    tar xf qemu.tar.xz
    cd "qemu-$2"
    ./configure --prefix="$4" --target-list=x86_64-softmmu --disable-docs --disable-werror \
      --enable-slirp --enable-fdt=system --disable-gtk --disable-sdl --disable-opengl --disable-spice \
      --disable-vnc --disable-xen --disable-debug-info
    make -j"$(nproc)"
    make install' _ "$src" "$QEMU_VERSION" "$QEMU_SHA256" "$prefix")
  cp "$BENCH_WORK/cpu_timed.log" "$BENCH_WORK/qemu-build.log"
  rm -rf "$src"
  if [ "${rc:-1}" != 0 ]; then
    tail -15 "$BENCH_WORK/qemu-build.log" >&2
    note "building QEMU $QEMU_VERSION failed (its build dependencies: ninja-build libglib2.0-dev libpixman-1-dev libslirp-dev libfdt-dev flex bison python3-venv, and python3-tomli where Python is older than 3.11)"
    return 1
  fi
  qemu=$prefix/bin/qemu-system-x86_64
  BENCH_ROUND=1
  bench_sample emu "QEMU $QEMU_VERSION built from source (one run), wall s" "$wall"
  bench_sample emu "QEMU $QEMU_VERSION built from source (one run), CPU s" "$cpu"
}

boot_ok=1
if [ "$skip_boot" = 1 ]; then
  boot_ok=0
else
  if [ -z "$qemu" ] || [ ! -x "$qemu" ]; then
    if [ "$build_qemu" = 1 ]; then
      soft_gate
      build_qemu_now || boot_ok=0
    else
      note "no QEMU 11.1 or newer (--qemu, EMU_QEMU, or --build-qemu): the boot phase did not run"
      boot_ok=0
    fi
  fi
  if [ "$boot_ok" = 1 ]; then
    qver=$("$qemu" -version 2>/dev/null | sed -n 's/^QEMU emulator version \([0-9]*\.[0-9]*\).*/\1/p')
    case $qver in
      11.[1-9] | 11.[1-9][0-9] | 1[2-9].* | [2-9][0-9].*) ;;
      *)
        note "$qemu is QEMU ${qver:-of unknown version}; software mode needs 11.1 or newer: the boot phase did not run"
        boot_ok=0
        ;;
    esac
  fi
fi

# --- the image ---------------------------------------------------------------------------------
if [ "$boot_ok" = 1 ]; then
  if [ -z "$BENCH_EMU" ]; then
    (cd "$BENCH_REPO" && cargo build -q --locked --release -p simee-emu -p simee) || bench_die "cargo build failed"
    BENCH_EMU="$BENCH_REPO/target/release/emu"
    BENCH_SIMEE="$BENCH_REPO/target/release/simee"
  fi
  [ -n "$BENCH_SIMEE" ] || BENCH_SIMEE="$(dirname "$BENCH_EMU")/simee"
  [ -x "$BENCH_SIMEE" ] || {
    note "no simee binary next to $BENCH_EMU (--simee): the boot phase did not run"
    boot_ok=0
  }
fi
if [ "$boot_ok" = 1 ]; then
  export ANDROID_HOME="$cache/sdk" ANDROID_SDK_ROOT="$cache/sdk"
  mkdir -p "$ANDROID_HOME"
  sysdir=$ANDROID_HOME/$image
  case $image in /*) sysdir=$image ;; *) ;; esac
  if [ ! -f "$sysdir/system.img" ] || [ ! -x "$ANDROID_HOME/platform-tools/adb" ]; then
    api=$(printf '%s\n' "$image" | sed -n 's|.*android-\([0-9][0-9]*\)/.*|\1|p')
    tag=$(basename "$(dirname "$image")")
    abi=$(basename "$image")
    if [ -z "$api" ] || ! need_disk 4 "provisioning the system image"; then
      boot_ok=0
    else
      bench_log "provisioning API $api $tag $abi into $ANDROID_HOME"
      t0=$(now)
      if "$BENCH_EMU" -provision "$api" -tag "$tag" -abi "$abi" >"$BENCH_WORK/provision.log" 2>&1; then
        BENCH_ROUND=1
        bench_sample emu "image and platform-tools download (one run, network-bound), s" "$(since "$t0" 1)"
      else
        tail -5 "$BENCH_WORK/provision.log" >&2
        note "provisioning the image failed (no network, or no space): the boot phase did not run"
        boot_ok=0
      fi
    fi
  fi
fi
if [ "$boot_ok" = 1 ] && [ ! -d "$ANDROID_HOME/simee/renderer" ]; then
  "$BENCH_EMU" -provision-renderer >"$BENCH_WORK/renderer.log" 2>&1 || {
    tail -5 "$BENCH_WORK/renderer.log" >&2
    note "provisioning the renderer failed: the boot phase did not run"
    boot_ok=0
  }
fi
[ "$boot_ok" = 1 ] && [ ! -f "$sysdir/system.img" ] && {
  note "no system image at $sysdir: the boot phase did not run"
  boot_ok=0
}

# --- boot --------------------------------------------------------------------------------------
if [ "$boot_ok" = 1 ]; then
  android_env
  IMAGE_DIR=$sysdir
  android_port "$port"
  sock_dir
  serial=emulator-$port
  arm="emu:EMU_QEMU=$qemu"
  settle=60
  win=60
  [ "$BENCH_QUICK" = 1 ] && settle=10 && win=10
  for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
    need_disk 2 "round $BENCH_ROUND's AVD" || break
    soft_gate
    avd="bench-soft-$$-$BENCH_ROUND"
    avd_create "$avd" "$IMAGE_DIR" -cores 4 -memory "$memory" -lcd "$lcd"
    if [ "$cold" = 1 ]; then
      BOOT_TIMEOUT=${BOOT_TIMEOUT:-2400}
      if android_boot "$arm" "$avd" "$port" "$BENCH_WORK/boot-cold-$BENCH_ROUND.log" -accel off; then
        bench_sample emu "cold boot (new userdata): launch until sys.boot_completed, s" "$BOOT_S"
        bench_sample emu "cold boot (new userdata): peak RSS of emu and QEMU, MB" "$PEAK_RSS_MB"
      else
        bench_sample emu "cold boot (new userdata): launch until sys.boot_completed, s" "over-${BOOT_TIMEOUT}"
      fi
      android_stop "$BOOT_PID" "$port"
    fi
    BOOT_TIMEOUT=${BOOT_TIMEOUT:-2400}
    s0=$(stat_now)
    if ! android_boot "$arm" "$avd" "$port" "$BENCH_WORK/boot-warm-$BENCH_ROUND.log" -accel off; then
      bench_sample emu "warm boot: launch until sys.boot_completed, s" "over-${BOOT_TIMEOUT}"
      android_stop "$BOOT_PID" "$port"
      avd_delete "$avd"
      continue
    fi
    bench_sample emu "warm boot: steal during the boot, % of CPU time" "$(steal_pct "$s0" "$(stat_now)")"
    bench_sample emu "warm boot: launch until adb lists the device, s" "$ADB_S"
    bench_sample emu "warm boot: launch until sys.boot_completed, s" "$BOOT_S"
    bench_sample emu "warm boot: peak RSS of emu and QEMU, MB" "$PEAK_RSS_MB"
    sleep "$settle"
    bench_sample emu "idle host CPU ${settle}-$((settle + win)) s after boot, cores" "$(cpu_window "$BOOT_PID" "$win")"
    if [ "$BENCH_QUICK" != 1 ]; then
      sleep 120
      bench_sample emu "idle host CPU 240-300 s after boot, cores" "$(cpu_window "$BOOT_PID" "$win")"
      bench_sample emu "RSS 300 s after boot, MB" "$(rss_mb "$BOOT_PID")"
    else
      bench_sample emu "RSS $((settle + win)) s after boot, MB" "$(rss_mb "$BOOT_PID")"
    fi
    bench_sample emu "steal since the boot began, % of CPU time" "$(steal_pct "$s0" "$(stat_now)")"
    # Screenshot and tap latency through `simee serve` (one connection), then adb's screencap.
    sock="$SOCK_DIR/steps.sock"
    rm -f "$sock"
    serve_start "$BENCH_SIMEE" "$sock"
    python3 -I "$BENCH_REPO/scripts/ios-step-bench.py" "$sock" "$avd" "$calls" 360 600 >"$BENCH_WORK/steps-$BENCH_ROUND.out" 2>&1
    bench_stop_pid "$SERVE_PID"
    if grep -q median "$BENCH_WORK/steps-$BENCH_ROUND.out"; then
      while IFS= read -r line; do
        step=${line%%: median*}
        ms=$(printf '%s\n' "$line" | sed -n 's/.*: median \([0-9.]*\) ms.*/\1/p')
        [ -n "$ms" ] && bench_sample emu "simee $step, median ms" "$ms"
      done <"$BENCH_WORK/steps-$BENCH_ROUND.out"
    else
      tail -5 "$BENCH_WORK/steps-$BENCH_ROUND.out" >&2
      bench_sample emu "simee screenshot, median ms" failed
    fi
    "$BENCH_ADB" -s "$serial" exec-out screencap -p >/dev/null 2>&1
    : >"$BENCH_WORK/screencap.ms"
    # A screencap takes seconds under TCG: five calls, not --calls.
    for _ in $(seq 1 5); do
      t0=$(now)
      "$BENCH_ADB" -s "$serial" exec-out screencap -p >/dev/null 2>&1
      since "$t0" 3 | awk '{ printf "%.1f\n", $1 * 1000 }' >>"$BENCH_WORK/screencap.ms"
    done
    bench_sample emu "adb exec-out screencap -p, median ms" "$(median <"$BENCH_WORK/screencap.ms")"
    android_stop "$BOOT_PID" "$port"
    avd_delete "$avd"
  done
fi
BENCH_ROUND=1
bench_sample emu "steal over the whole run, % of CPU time" "$(steal_pct "$run_stat0" "$(stat_now)")"
"$BENCH_DIR/host-profile.sh" --window 5 >"$BENCH_OUT/soft-host-$stamp.profile-end.json" || bench_log "host-profile.sh failed"
summary="Image $image, $memory MB guest, 4 vCPUs, QEMU ${qver:-none}, -accel off. Host profile: soft-host-$stamp.profile.json."
[ ${#NOTES[@]} -gt 0 ] && summary="$summary Not measured or changed: $(printf '%s; ' "${NOTES[@]}")"
bench_finish emu "$summary"
