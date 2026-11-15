#!/usr/bin/env bash
# Core JSON corpora live only in the pinned submodule. cargo test stages them
# from third_party/bitcoin/src/test/data each run (see core_fixture.rs).
#
# --check: submodule files exist AND fixtures/ has no copies of those names.
# --write: stage a copy into --dest (default $CARGO_TARGET_DIR/core-data) for
#          inspection. Not used by cargo test.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
MODE=""
CORE_DATA="${ROOT}/third_party/bitcoin/src/test/data"
FIXTURES="${ROOT}/crates/rbitcoin-consensus/tests/fixtures"
DEST="${CARGO_TARGET_DIR:-$ROOT/target}/core-data"
FILES=(script_tests.json tx_valid.json tx_invalid.json)

usage() {
  echo "usage: $0 --check|--write [--core-data DIR] [--fixtures DIR] [--dest DIR]" >&2
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --check) MODE=check; shift ;;
    --write) MODE=write; shift ;;
    --core-data)
      [[ $# -ge 2 ]] || usage
      CORE_DATA="$2"
      shift 2
      ;;
    --fixtures)
      [[ $# -ge 2 ]] || usage
      FIXTURES="$2"
      shift 2
      ;;
    --dest)
      [[ $# -ge 2 ]] || usage
      DEST="$2"
      shift 2
      ;;
    -h | --help) usage ;;
    *) usage ;;
  esac
done

[[ -n "$MODE" ]] || usage

if [[ ! -d "$CORE_DATA" ]]; then
  echo "missing core data dir: $CORE_DATA" >&2
  echo "hint: ./scripts/core-functional/init-submodule.sh" >&2
  exit 1
fi

if [[ "$MODE" == write ]]; then
  mkdir -p "$DEST"
  for f in "${FILES[@]}"; do
    src="${CORE_DATA}/${f}"
    if [[ ! -f "$src" ]]; then
      echo "missing core fixture: ${f}" >&2
      exit 1
    fi
    cp -f "$src" "${DEST}/${f}"
    echo "wrote ${DEST}/${f}"
  done
  exit 0
fi

err=0
for f in "${FILES[@]}"; do
  src="${CORE_DATA}/${f}"
  if [[ ! -f "$src" ]]; then
    echo "missing core fixture: ${f}" >&2
    err=1
    continue
  fi
  if [[ -e "${FIXTURES}/${f}" ]]; then
    echo "in-tree copy must not exist: ${f}" >&2
    err=1
  fi
done

if [[ "$err" -ne 0 ]]; then
  echo "sync-core-fixtures: check failed" >&2
  exit 1
fi
echo "sync-core-fixtures: ok (${#FILES[@]} files in ${CORE_DATA}; no in-tree copies)"
exit 0
