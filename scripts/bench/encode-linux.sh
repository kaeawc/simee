#!/usr/bin/env bash
# H.264 encode cost of simee-encode's Linux encoder (OpenH264, built from source with the
# `openh264` feature) on synthetic phone screens: no device and no emulator, so it runs on any
# Linux host with a C++ compiler (and `nasm`, which makes the x86_64 build about 2.3x faster).
#
#   scripts/bench/encode-linux.sh [--size "WxH ..."] [--max-size "0 1280"] [--fps "30 60"]
#                                 [--seconds S] [--content drawer|noise] [--bitrate BPS]
#                                 [--profile cb|main] [--rounds N] [--quick] [--out DIR]
#
# Each round, for each --max-size (0: the full frame) and each rate in --fps, runs the
# linux_bench example for --seconds (10): an app drawer scrolling on a screen of each --size (a
# phone's 1080x2400 and a tablet's 2560x1600), presented at 60 fps and encoded at the cap through the real encode loop, and
# records: frames encoded per second, per-frame encode latency median and p95 (the colour
# conversion and scaling included), the conversion on its own, CPU of the process (% of a core),
# the luma PSNR of the decoded stream against what the encoder was given, output kbit/s, and the
# first and the median IDR's size. The screens are rendered before the run, so the generator's
# cost is not in the numbers. Defaults: Constrained Baseline, 4 Mbit/s average, 8 Mbit/s ceiling,
# an IDR every 2 s, --max-size "0 1280", --fps "30 60". --quick: one round of 5 s.
# OPENH264_NO_ASM=1 in the environment builds without assembly (the number a host with no nasm
# gets). Reproduces docs/streaming.md "Encode cost on Linux".
set -uo pipefail
. "$(dirname "$0")/lib.sh"
sizes="1080x2400 2560x1600"
max_sizes="0 1280"
rates="30 60"
seconds=10
extra=()
while [ $# -gt 0 ]; do
  case $1 in
    --size)
      sizes=$2
      shift 2
      ;;
    --max-size)
      max_sizes=$2
      shift 2
      ;;
    --fps)
      rates=$2
      shift 2
      ;;
    --seconds)
      seconds=$2
      shift 2
      ;;
    --content | --bitrate)
      extra+=("$1" "$2")
      shift 2
      ;;
    --profile)
      case $2 in
        cb) ;;
        main) extra+=(--profile main) ;;
        *) bench_die "--profile $2: expected cb or main" ;;
      esac
      shift 2
      ;;
    *)
      bench_opt "$@"
      shift "$BENCH_SHIFT"
      ;;
  esac
done
[ "$BENCH_QUICK" = 1 ] && seconds=5
[ "$(uname -s)" = Linux ] || bench_skip "this is the Linux encoder (macOS has VideoToolbox: encode.sh)"
command -v c++ >/dev/null 2>&1 || command -v g++ >/dev/null 2>&1 || bench_skip "OpenH264 builds from C++ source: no c++ or g++"
bench_start encode-linux
bench_log "building the linux_bench example (OpenH264 from source)"
(cd "$BENCH_REPO" && cargo build -q --release -p simee-encode --features openh264 --example linux_bench) || bench_die "cargo build --example linux_bench failed"
bin="${CARGO_TARGET_DIR:-$BENCH_REPO/target}/release/examples/linux_bench"

arms=""
for BENCH_ROUND in $(seq 1 "$BENCH_ROUNDS"); do
  for size in $sizes; do
    for cap in $max_sizes; do
      for fps in $rates; do
        if [ "$cap" = 0 ]; then arm="$size full, $fps fps"; else arm="$size capped at $cap, $fps fps"; fi
        case ",$arms," in *",$arm,"*) ;; *) arms="${arms:+$arms,}$arm" ;; esac
        json="$BENCH_WORK/round-$BENCH_ROUND-$size-$cap-$fps.json"
        capflag=()
        [ "$cap" = 0 ] || capflag=(--max-size "$cap")
        if ! "$bin" --size "$size" --fps "$fps" --seconds "$seconds" ${capflag[@]+"${capflag[@]}"} ${extra[@]+"${extra[@]}"} >"$json" 2>"${json%.json}.log"; then
          tail -5 "${json%.json}.log" >&2
          bench_sample "$arm" "encoded, fps" failed
          continue
        fi
        bench_log "$(cat "$json")"
        python3 -I -c '
import json, sys
j = json.load(open(sys.argv[1]))
rows = [
    ("encoded, fps", j["encoded_fps"]),
    ("encode latency p50, ms", j["encode_ms_p50"]),
    ("encode latency p95, ms", j["encode_ms_p95"]),
    ("colour conversion and scaling p50, ms", j["convert_ms_p50"]),
    ("process CPU, % of a core", j["cpu_pct_of_a_core"]),
    ("luma PSNR, dB", j["luma_psnr_db"]),
    ("output, kbit/s", j["kbit_s"]),
    ("first IDR, KB", j["first_idr_kb"]),
    ("IDR median, KB", j["idr_kb_median"]),
]
for metric, value in rows:
    print(f"{metric}\t{value}")
' "$json" | while IFS=$'\t' read -r metric value; do bench_sample "$arm" "$metric" "$value"; done
      done
    done
  done
done
asm=with
if [ -n "${OPENH264_NO_ASM:-}" ] || ! command -v nasm >/dev/null 2>&1; then asm=without; fi
bench_finish "$arms" "OpenH264 built $asm x86 assembly, a synthetic app drawer scrolling on ${sizes// /, }, ${seconds} s per run, presented at 60 fps; ${extra[*]-encoder defaults}."
