#!/usr/bin/env bash
# What CI checks, fastest first, stopping at the first failure; each step prints its time.
#
#   scripts/check.sh            # everything the change needs (the pre-push hook runs this)
#   scripts/check.sh --quick    # linters, fmt, host clippy and tests only
#   scripts/check.sh --all      # every step, whatever changed
#
# On Windows, where there is no bash, `cargo xtask check` (xtask/) runs the same steps.
#
# Steps: the script and docs linters (scripts/lint.sh: each one installed here, else a skip
# line); rustfmt; clippy for this host; clippy for the other platform's target when its std is
# installed (macOS code from Linux: `rustup target add aarch64-apple-darwin`, and the reverse),
# which type-checks code this host never compiles; the workspace tests (nextest if installed);
# the desktop workspace when desktop/ changed (or with --all). A change that touches only docs
# runs the linters and nothing else.
# Changed files are those since the merge base with origin/main, plus uncommitted ones.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

mode=changed
case "${1:-}" in
  --quick) mode=quick ;;
  --all) mode=all ;;
  "") ;;
  *)
    echo "usage: scripts/check.sh [--quick|--all]" >&2
    exit 2
    ;;
esac

base=$(git merge-base HEAD origin/main 2>/dev/null || git rev-parse HEAD~1 2>/dev/null || echo HEAD)
changed=$( (
  git diff --name-only "$base"...HEAD
  git diff --name-only HEAD
  git ls-files --others --exclude-standard
) 2>/dev/null | sort -u)
touches() { [[ $mode == all ]] || command grep -qE "$1" <<<"$changed"; }

total=$(date +%s)
step() {
  local name=$1
  shift
  local start
  start=$(date +%s)
  printf 'check: %-34s' "$name"
  local out
  if out=$("$@" 2>&1); then
    printf 'ok  (%ss)\n' $(($(date +%s) - start))
  else
    printf 'FAILED (%ss)\n\n%s\n\n  rerun: %s\n' $(($(date +%s) - start)) "$out" "$*"
    exit 1
  fi
}

# Seconds each, and docs need them too: before the docs-only exit. shellcheck needs shfmt to
# list the scripts.
for tool in shellcheck shfmt lychee typos actionlint yamllint; do
  bin=$tool
  [[ $tool == shellcheck ]] && ! command -v shfmt >/dev/null 2>&1 && bin=shfmt
  if command -v "$bin" >/dev/null 2>&1; then
    step "$tool" scripts/lint.sh "$tool"
  else
    printf 'check: %-34sskipped: %s not installed\n' "$tool" "$bin"
  fi
done

if [[ $mode == changed && -n $changed ]] && ! command grep -qvE '\.md$|^docs/|\.png$' <<<"$changed"; then
  echo "check: only docs changed; nothing to build"
  exit 0
fi

# Without zip and unzip, emu-sdk's installer tests print SKIP and return instead of running.
if [[ $(uname -s) == Linux ]]; then
  missing=$(for t in zip unzip; do command -v "$t" >/dev/null 2>&1 || printf '%s ' "$t"; done)
  if [[ -n $missing ]]; then
    echo "check: warning: ${missing}missing (apt-get install zip unzip): emu-sdk's installs_renderer_from_a_local_archive and installs_system_images_with_sparse_disks will SKIP, and CI runs them" >&2
  fi
fi

installed_target() { rustup target list --installed 2>/dev/null | command grep -qx "$1"; }

step fmt cargo fmt --all --check
step "fmt (desktop)" rustfmt --check --edition 2024 desktop/src/main.rs
step clippy cargo clippy --all-targets --quiet -- -D warnings
step "macOS test list" python3 scripts/ci-scope.py check
if [[ $mode != quick ]]; then
  case "$(uname -s)" in
    Linux) other=aarch64-apple-darwin ;;
    Darwin) other=x86_64-unknown-linux-gnu ;;
    *) other= ;;
  esac
  if [[ -n $other ]] && installed_target "$other"; then
    step "clippy ($other)" cargo clippy --target "$other" --all-targets --quiet -- -D warnings
  elif [[ -n $other ]]; then
    printf 'check: %-34sskipped: rustup target add %s\n' "clippy ($other)" "$other"
  fi
fi
if cargo nextest --version >/dev/null 2>&1; then
  step tests cargo nextest run --workspace --no-fail-fast --status-level fail --final-status-level fail --hide-progress-bar
else
  step tests cargo test --workspace --no-fail-fast --quiet
fi
# A strong import newer than macOS 11.0 stops dyld from loading emu or simee on older macOS.
if [[ $(uname -s) == Darwin ]]; then
  step "release imports" bash -c 'cargo build --release --quiet -p simee-emu -p simee && scripts/check-imports.sh'
fi
# The desktop is its own workspace with a large dependency (Rune): only when it changed. CI builds
# it on every pull request, which catches a shared crate's change breaking it.
if [[ $mode != quick ]] && touches '^desktop/'; then
  step "desktop clippy" bash -c 'cd desktop && cargo clippy --all-targets --quiet -- -D warnings'
  step "desktop tests" bash -c 'cd desktop && cargo test --quiet'
fi
echo "check: all passed in $(($(date +%s) - total))s"
