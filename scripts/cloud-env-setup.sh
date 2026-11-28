#!/usr/bin/env bash
# Claude cloud environment setup for kaeawc/simee (Ubuntu 24.04 container, x86_64 or arm64).
# Paste into the environment's setup script. Idempotent: safe to re-run; every step warns and goes
# on rather than failing the whole setup, so a session still starts if one optional tool is missing.
#
# What it installs, matching .github/workflows/ci.yml and scripts/check.sh:
#   - the pinned Rust toolchain from rust-toolchain.toml (+ clippy, rustfmt, llvm-tools-preview),
#     the Linux std targets, and aarch64-apple-darwin std so check.sh can clippy macOS code from Linux
#   - cargo-nextest (CI's test runner), cargo-llvm-cov, typos
#   - the script and docs linters lint.sh runs: shellcheck, shfmt, actionlint, yamllint, lychee
#   - system libraries the workspace and the Rune desktop app build and render against: nasm
#     (openh264), libxkbcommon/wayland/X11 (Rune's windowing), Vulkan loader + Mesa lavapipe (wgpu
#     renders headless on the CPU: no GPU in the container), fontconfig/freetype, ALSA, udev
#   - then warms the cargo caches for the root workspace, desktop/ and website/
# It does not install Android SDK images or emulators: cloud containers have no KVM, so device work
# stays on the owner's Mac (issues labelled needs-local:*).
set -uo pipefail

REPO="${REPO:-${CLAUDE_PROJECT_DIR:-$PWD}}"
[ -f "$REPO/Cargo.toml" ] || REPO="$(git -C "$PWD" rev-parse --show-toplevel 2>/dev/null || echo "$PWD")"
case "$(uname -m)" in
  x86_64 | amd64) ARCH=x86_64 ;;
  aarch64 | arm64) ARCH=aarch64 ;;
  *) ARCH="$(uname -m)" ;;
esac
SUDO=""
[ "$(id -u)" -eq 0 ] || SUDO="sudo"
step() { printf '\n==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
export DEBIAN_FRONTEND=noninteractive

# Exact tool versions CI pins (ci.yml's taiki-e/install-action lines), so local results match CI.
SHELLCHECK_V=0.11.0
SHFMT_V=3.14.1
TYPOS_V=1.52.2
ACTIONLINT_V=1.7.7

step "system packages"
PKGS=(
  build-essential pkg-config clang cmake nasm git curl ca-certificates unzip xz-utils jq
  python3 python3-venv
  libssl-dev
  # Rune / wgpu / winit on Linux (desktop/), and headless rendering on the CPU
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev wayland-protocols
  libx11-dev libxcursor-dev libxrandr-dev libxi-dev libx11-xcb-dev libxcb1-dev
  libvulkan-dev mesa-vulkan-drivers vulkan-tools libegl1 libgl1
  libfontconfig1-dev libfreetype-dev
  # audio and device enumeration crates
  libasound2-dev libudev-dev
)
if ! $SUDO apt-get install -y --no-install-recommends "${PKGS[@]}" >/tmp/apt.log 2>&1; then
  $SUDO apt-get update >/tmp/apt-update.log 2>&1 || warn "apt-get update failed"
  $SUDO apt-get install -y --no-install-recommends "${PKGS[@]}" >/tmp/apt.log 2>&1 ||
    warn "some system packages failed to install (see /tmp/apt.log)"
fi

step "rust (rust-toolchain.toml)"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
if ! command -v rustup >/dev/null 2>&1 && [ ! -x "$CARGO_HOME/bin/rustup" ]; then
  curl -fsSL --retry 3 https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none ||
    warn "rustup install failed"
fi
export PATH="$CARGO_HOME/bin:$PATH"
CHANNEL="$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$REPO/rust-toolchain.toml" 2>/dev/null)"
CHANNEL="${CHANNEL:-stable}"
rustup toolchain install "$CHANNEL" --profile minimal \
  --component clippy --component rustfmt --component llvm-tools-preview ||
  warn "rust $CHANNEL install failed"
rustup default "$CHANNEL" >/dev/null 2>&1 || true
# Other-platform std targets: check.sh type-checks macOS code from Linux with clippy when the
# aarch64-apple-darwin std is installed; the Linux targets cover both container architectures.
for t in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu aarch64-apple-darwin; do
  rustup target add --toolchain "$CHANNEL" "$t" >/dev/null 2>&1 || warn "rustup target $t failed"
done
rustc --version && cargo --version

step "cargo tools (match CI)"
# cargo-binstall fetches prebuilt binaries: seconds instead of minutes of compiling each tool.
if ! command -v cargo-binstall >/dev/null 2>&1; then
  curl -fsSL --retry 3 https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh |
    bash >/tmp/binstall.log 2>&1 || warn "cargo-binstall install failed (see /tmp/binstall.log)"
fi
binstall() {
  if command -v cargo-binstall >/dev/null 2>&1; then
    cargo binstall -y --no-confirm --locked "$1" >/dev/null 2>&1 && return 0
  fi
  cargo install --locked "$1" >/dev/null 2>&1
}
binstall cargo-nextest || warn "cargo-nextest install failed"
binstall cargo-llvm-cov || warn "cargo-llvm-cov install failed"
binstall "typos-cli@$TYPOS_V" || warn "typos install failed"
binstall lychee || warn "lychee install failed (lint.sh will skip it)"

step "script and workflow linters (lint.sh)"
BIN=/usr/local/bin
case "$ARCH" in x86_64) SC_ARCH=x86_64 GO_ARCH=amd64 ;; aarch64) SC_ARCH=aarch64 GO_ARCH=arm64 ;; esac
if [ "$(shellcheck --version 2>/dev/null | sed -n 's/^version: //p')" != "$SHELLCHECK_V" ]; then
  curl -fsSL --retry 3 "https://github.com/koalaman/shellcheck/releases/download/v$SHELLCHECK_V/shellcheck-v$SHELLCHECK_V.linux.$SC_ARCH.tar.xz" |
    tar -xJ -C /tmp && $SUDO install -m 0755 "/tmp/shellcheck-v$SHELLCHECK_V/shellcheck" "$BIN/shellcheck" ||
    warn "shellcheck install failed"
fi
if [ "$(shfmt --version 2>/dev/null)" != "v$SHFMT_V" ]; then
  curl -fsSL --retry 3 -o /tmp/shfmt "https://github.com/mvdan/sh/releases/download/v$SHFMT_V/shfmt_v${SHFMT_V}_linux_$GO_ARCH" &&
    $SUDO install -m 0755 /tmp/shfmt "$BIN/shfmt" || warn "shfmt install failed"
fi
if ! command -v actionlint >/dev/null 2>&1; then
  curl -fsSL --retry 3 "https://github.com/rhysd/actionlint/releases/download/v$ACTIONLINT_V/actionlint_${ACTIONLINT_V}_linux_$GO_ARCH.tar.gz" |
    tar -xz -C /tmp actionlint && $SUDO install -m 0755 /tmp/actionlint "$BIN/actionlint" ||
    warn "actionlint install failed"
fi
# Ubuntu 24.04's Python is externally managed (PEP 668): yamllint lives in a venv.
LINT_VENV="$HOME/.venvs/simee-lint"
if [ ! -x "$LINT_VENV/bin/yamllint" ]; then
  { python3 -m venv "$LINT_VENV" && "$LINT_VENV/bin/pip" install -q yamllint; } || warn "yamllint install failed"
fi
[ -x "$LINT_VENV/bin/yamllint" ] && $SUDO ln -sf "$LINT_VENV/bin/yamllint" "$BIN/yamllint"
# Symlink cargo-installed tools into /usr/local/bin so non-login shells find them too.
for b in cargo-nextest cargo-llvm-cov typos lychee; do
  [ -x "$CARGO_HOME/bin/$b" ] && $SUDO ln -sf "$CARGO_HOME/bin/$b" "$BIN/$b"
done

step "headless GPU (wgpu on Mesa lavapipe)"
# No GPU in the container: wgpu falls back to the lavapipe software Vulkan driver, so the desktop's
# headless scene tests and `--snap` renders run here (slowly). Confirm the ICD is visible.
compgen -G '/usr/share/vulkan/icd.d/lvp_icd*.json' >/dev/null ||
  warn "no lavapipe ICD found: desktop render tests will skip or fail"

step "shell env"
if ! grep -q '# >>> simee env >>>' "$HOME/.bashrc" 2>/dev/null; then
  cat >>"$HOME/.bashrc" <<'EOF'
# >>> simee env >>>
export LANG=C.UTF-8 LC_ALL=C.UTF-8
export PATH="$HOME/.cargo/bin:$HOME/.venvs/simee-lint/bin:$HOME/.local/bin:$PATH"
# Small container: keep cargo and nextest from starving the session.
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-$(nproc)}"
export NEXTEST_TEST_THREADS="${NEXTEST_TEST_THREADS:-$(nproc)}"
# wgpu picks the software Vulkan driver; no display server.
export WGPU_BACKEND=vulkan
# <<< simee env <<<
EOF
fi

step "warm the cargo caches"
cd "$REPO" || {
  warn "repo not found at $REPO"
  exit 0
}
if [ -f Cargo.toml ]; then
  cargo fetch --locked >/dev/null 2>&1 || warn "cargo fetch (root) failed"
  # A debug build of the whole workspace plus its test binaries, so the first gate a session runs
  # compiles only what it changed. --keep-going: one crate that needs macOS must not stop the rest.
  cargo test --workspace --no-run --locked --keep-going >/tmp/warm-root.log 2>&1 ||
    warn "root workspace warm build had errors (see /tmp/warm-root.log)"
  cargo clippy --workspace --all-targets --locked --keep-going >/tmp/warm-clippy.log 2>&1 ||
    warn "root clippy warm-up had errors (see /tmp/warm-clippy.log)"
fi
for ws in desktop website; do
  if [ -f "$ws/Cargo.toml" ]; then
    (cd "$ws" && cargo fetch --locked >/dev/null 2>&1 && cargo test --no-run --locked >"/tmp/warm-$ws.log" 2>&1) ||
      warn "$ws warm build had errors (see /tmp/warm-$ws.log)"
  fi
done

step "versions"
for t in rustc cargo cargo-nextest typos shellcheck shfmt actionlint yamllint lychee nasm; do
  printf '  %-14s %s\n' "$t" "$({ "$t" --version 2>/dev/null || "$t" -version 2>/dev/null; } | head -1 || echo missing)"
done
step "done"
