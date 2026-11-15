#!/usr/bin/env bash
# Enforce production LCOV ≥ 91% floor. CRAP --fail-above 30 after that.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"

# Instrumented objects must not share a target dir with plain cargo test /
# clippy (different RUSTFLAGS fingerprints thrash rebuilds). Dev shell uses
# target/dev; musl release is nix/crane only.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR_COV:-$ROOT/target/cov}"
echo "coverage: CARGO_TARGET_DIR=$CARGO_TARGET_DIR"

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found; enter nix-shell first" >&2
  exit 1
fi

# Core JSON corpora are staged from the pinned submodule (not in-tree copies).
./scripts/core-functional/init-submodule.sh

# `cargo llvm-cov test` builds `journey-node` when a journey execs it.
# Node and cli smoke go through `cli_main`, not the bin. A plain
# `cargo build` here compiled the workspace a second time with different
# RUSTFLAGS.

# Prefer system llvm-tools when rustup component is unavailable (Nix).
if [[ -z "${LLVM_COV:-}" ]] && command -v llvm-cov >/dev/null 2>&1; then
  export LLVM_COV="$(command -v llvm-cov)"
fi
if [[ -z "${LLVM_PROFDATA:-}" ]] && command -v llvm-profdata >/dev/null 2>&1; then
  export LLVM_PROFDATA="$(command -v llvm-profdata)"
fi

# main.rs trampolines are one-liners; logic is covered via cli_main in libs.
# Test files / rbitcoin-test / rbitcoin-bench / testutil are not in the ratio.
# The journey tests still run, so production lines they hit still count.
# Do not match the substring "test" (regtest_pad.rs stays).
IGNORE='(/\.cargo/|/rustc-|/nix/store/|library/std/|/src/main\.rs$|/tests/|_tests\.rs$|/tests\.rs$|/testutil\.rs$|/tests_verify\.rs$|/crates/rbitcoin-test/|/crates/rbitcoin-bench/)'

if command -v cargo-llvm-cov >/dev/null 2>&1 || cargo llvm-cov --version >/dev/null 2>&1; then
  ./scripts/coverage.test.sh
  # Default: do NOT clean instrumented artifacts. Incremental llvm-cov rebuilds
  # are much faster for iterative work and still re-run all tests with coverage.
  # Force a full clean when debugging stale counters: COVERAGE_CLEAN=1 ./scripts/coverage.sh
  if [[ "${COVERAGE_CLEAN:-0}" == "1" ]]; then
    echo "COVERAGE_CLEAN=1: wiping llvm-cov workspace artifacts"
    cargo llvm-cov clean --workspace
  fi

  # Branch coverage requires nightly on many toolchains; line gate is always on.
  EXTRA=()
  if cargo llvm-cov test --help 2>&1 | grep -q -- '--fail-under-branches'; then
    if rustc -vV 2>/dev/null | grep -q nightly; then
      EXTRA+=(--branch --fail-under-branches 90)
    fi
  fi
  mkdir -p "$ROOT/coverage"
  # Wall-clock cap: a hung instrumented test (P2P/confirm join) otherwise sits
  # until the runner's 6h default. Override: LLVM_COV_TEST_TIMEOUT=20m.
  LLVM_COV_TEST_TIMEOUT="${LLVM_COV_TEST_TIMEOUT:-12m}"
  LLVM_COV_TEST=(
    cargo llvm-cov test --workspace --exclude rbitcoin-bench
    --ignore-filename-regex "$IGNORE"
    "${EXTRA[@]}"
    --html --output-dir "$ROOT/coverage"
  )
  echo "coverage: llvm-cov test wall ${LLVM_COV_TEST_TIMEOUT}"
  set +e
  if command -v timeout >/dev/null 2>&1; then
    timeout --kill-after=30s "${LLVM_COV_TEST_TIMEOUT}" "${LLVM_COV_TEST[@]}"
    llvm_cov_st=$?
  else
    "${LLVM_COV_TEST[@]}"
    llvm_cov_st=$?
  fi
  set -e
  if [[ "$llvm_cov_st" -eq 124 ]]; then
    echo "FAIL: cargo llvm-cov test exceeded ${LLVM_COV_TEST_TIMEOUT} (hung test binary?)" >&2
    exit 1
  fi
  if [[ "$llvm_cov_st" -ne 0 ]]; then
    exit "$llvm_cov_st"
  fi

  REPORT="$(cargo llvm-cov report --ignore-filename-regex "$IGNORE" 2>/dev/null || true)"
  echo "$REPORT"

  cargo llvm-cov report \
    --ignore-filename-regex "$IGNORE" \
    --lcov --output-path "$ROOT/coverage/lcov.info" || true

  # Line gate: LCOV ≥ 91% floor (llvm-cov LH jitters; no never-falls ratchet).
  LCOV_STATS="$(python3 - <<'PY'
from pathlib import Path
p = Path("coverage/lcov.info")
lf = lh = 0
if p.exists():
    for line in p.read_text(errors="replace").splitlines():
        if line.startswith("LF:"):
            lf += int(line[3:])
        elif line.startswith("LH:"):
            lh += int(line[3:])
print(f"{lh} {lf}")
PY
)"
  read -r LCOV_HIT LCOV_TOT <<<"$LCOV_STATS"
  LCOV_HIT="${LCOV_HIT:-0}"
  LCOV_TOT="${LCOV_TOT:-0}"
  if [[ "$LCOV_TOT" -le 0 ]]; then
    echo "FAIL: no LCOV totals (missing coverage/lcov.info or empty LF)" >&2
    exit 1
  fi
  # Display and gate both use 2-decimal percent (llvm-cov LH jitters).
  LCOV_PCT="$(python3 -c "print(f'{100.0*$LCOV_HIT/$LCOV_TOT:.2f}')")"
  MISS=$((LCOV_TOT > LCOV_HIT ? LCOV_TOT - LCOV_HIT : 0))
  echo "LCOV lines: ${LCOV_HIT}/${LCOV_TOT} (${LCOV_PCT}%) miss=${MISS} (production files)"
  echo "Line coverage gate: ${LCOV_PCT}% now; pass iff unrounded LH*100 >= LF*91"
  echo "Gate math: 91% floor (llvm-cov LH jitters; no never-falls ratchet)"

  # Optional HTML diagnostic (not the pass condition).
  HTML_PRESENT=0
  if find "$ROOT/coverage" -name '*.html' -print -quit 2>/dev/null | grep -q .; then
    HTML_PRESENT=1
  fi
  if [[ "$HTML_PRESENT" -eq 1 ]]; then
    UNCOV_TOTAL="$(
      python3 - <<'PY'
from pathlib import Path
import re
n = 0
for p in Path("coverage").rglob("*.html"):
    t = p.read_text(errors="replace")
    n += len(re.findall(r"class=['\"]uncovered-line['\"]", t))
print(n)
PY
    )"
    echo "HTML uncovered-line markers (diagnostic): ${UNCOV_TOTAL}"
  fi

  GATE_JSON="$ROOT/coverage/gate.json"
  if ! python3 "$ROOT/scripts/coverage-gate.py" \
    --lh "$LCOV_HIT" --lf "$LCOV_TOT" --status-out "$GATE_JSON"; then
    cargo llvm-cov report --ignore-filename-regex "$IGNORE" --show-missing-lines || true
    exit 1
  fi
  SHA="${GITHUB_SHA:-$(git rev-parse HEAD)}"
  GATE_FLOOR="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["floor"])' "$GATE_JSON")"
  python3 "$ROOT/scripts/coverage-badge.py" \
    --lh "$LCOV_HIT" --lf "$LCOV_TOT" --gate "$GATE_FLOOR" \
    --sha "$SHA" --scope production \
    --lcov "$ROOT/coverage/lcov.info" \
    --out "$ROOT/coverage/badge.json"
  echo "Wrote coverage/badge.json (${LCOV_PCT}% production)"
  echo "Note: full branch coverage requires nightly --branch; region-partial lines may still appear in text report."
  echo "Tip: set COVERAGE_CLEAN=1 only when you need a cold instrumented rebuild."
  echo "Tooling: use llvmPackages matching rustc (rustc 1.95 → LLVM 21; see shell.nix)."
  ./scripts/coverage-crap.sh "$ROOT/coverage/lcov.info" "$ROOT/coverage/crap.json"
  exit 0
fi

echo "cargo-llvm-cov not installed; running tests without coverage gate." >&2
echo "Install: cargo install cargo-llvm-cov --locked" >&2
echo "Or: nix-shell -p cargo-llvm-cov" >&2
cargo test --workspace
echo "WARNING: coverage gate skipped (tooling missing)" >&2
exit 0
