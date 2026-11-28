#!/usr/bin/env bash
# Runs the MCP eval suite (docs/mcp-evals.md) and prints its results table.
#
#   scripts/mcp-evals.sh                      # the reference plans against the fake backend (what CI runs)
#   scripts/mcp-evals.sh --update-baseline    # also rewrite crates/sim-mcp/tests/evals/baseline.json
#   scripts/mcp-evals.sh --agent '<command>'  # also hand each task's goal to an agent (live; never in CI)
#
# The tables land in target/mcp-evals/ (results.md, and live.md with --agent); CARGO_TARGET_DIR moves them.
set -euo pipefail
cd "$(dirname "$0")/.." || exit 1

usage() {
  echo "usage: scripts/mcp-evals.sh [--update-baseline] [--agent '<command>']" >&2
  exit 2
}

while [[ $# -gt 0 ]]; do
  case $1 in
    --update-baseline) export SIMEE_EVALS_UPDATE_BASELINE=1 ;;
    --agent)
      [[ $# -ge 2 && -n $2 ]] || usage
      export SIMEE_EVALS_AGENT=$2
      shift
      ;;
    *) usage ;;
  esac
  shift
done

status=0
cargo test -p simee-mcp --test evals --quiet || status=$?

out=${CARGO_TARGET_DIR:-target}/mcp-evals
for table in results.md live.md; do
  if [[ -f $out/$table && ($table == results.md || -n ${SIMEE_EVALS_AGENT:-}) ]]; then
    echo
    cat "$out/$table"
  fi
done
exit "$status"
