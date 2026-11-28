#!/usr/bin/env bash
# Shared helpers for scripts/media/*.sh: repository paths, the desktop binary, dependency checks.
# Sourced, never run. Tools that are missing are named, never installed.

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
desktop_bin=${SIMEE_DESKTOP_BIN:-$root/desktop/target/release/simee-desktop}

# The scenes the desktop smoke lane renders, one per form factor, as "<scene>:<asset name>".
# Scene names are `simee-desktop --list-scenes`; the asset name is what the website refers to.
# shellcheck disable=SC2034 # used by the scripts that source this
scenes=(
  demo:iphone
  ipad:ipad
  apple-watch:apple-watch
  apple-tv:apple-tv
  carplay:carplay
  vision-pro:vision-pro
  android:android-phone
  foldable:android-foldable-open
  folded:android-foldable-folded
  tablet:android-tablet
  watch:wear-os-round
  tv:android-tv
  auto:android-automotive
  android-xr:android-xr-headset
  glasses:android-xr-glasses
)

# The camera angles: `SIMEE_SCENE_CAMERA` value, or "" for the default 3/4 view.
# shellcheck disable=SC2034
angles=(three-quarter:"" front:front side:side back:back)

need() {
  local missing=0 t
  for t in "$@"; do
    if ! command -v "$t" >/dev/null 2>&1; then
      echo "media: $t is not installed" >&2
      missing=1
    fi
  done
  return "$missing"
}

# build_desktop: builds the optimized desktop with Rune's headless tooling when the binary is missing.
build_desktop() {
  if [[ ! -x $desktop_bin ]]; then
    cargo build --release --features tooling --manifest-path "$root/desktop/Cargo.toml"
  fi
}
