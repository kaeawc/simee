#!/usr/bin/env bash
# Redraws every file in assets/brand/ (app icon, .icns, favicon, menu bar template, PNGs, SVGs)
# from the mark's description in crates/sim-core/src/mark/. Run it after changing the cut, its
# light or a tone ramp, and commit what changes; a sim-core test fails while the SVGs are stale.
#
#   scripts/brand.sh            # into assets/brand/
#   scripts/brand.sh <dir>      # somewhere else, to compare
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cargo run --quiet --release --manifest-path "$root/Cargo.toml" -p simee-core --example brand -- "${1:-$root/assets/brand}"
