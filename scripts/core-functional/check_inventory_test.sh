#!/usr/bin/env bash
# Red/green pin for check_inventory.py (no node, no Core checkout).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CHECK="$ROOT/scripts/core-functional/check_inventory.py"
PASS=0
FAIL=0
source "$ROOT/scripts/contract-assert.sh"
assert_ok() { assert_ok_quiet "$@"; }


assert_fail_msg() {
  local name="$1"
  local needle="$2"
  shift 2
  local out
  if out="$("$@" 2>&1)"; then
    echo "not ok - $name (expected failure)"
    FAIL=$((FAIL + 1))
    return
  fi
  if printf '%s' "$out" | grep -q -- "$needle"; then
    echo "ok - $name"
    PASS=$((PASS + 1))
  else
    echo "not ok - $name (missing '$needle' in: $out)"
    FAIL=$((FAIL + 1))
  fi
}

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/rbitcoin-cf-inv.XXXXXX")"
cleanup() { rm -rf "$WORKDIR"; }
trap cleanup EXIT

TESTS="$WORKDIR/tests"
mkdir -p "$TESTS"
printf '# fake\n' >"$TESTS/feature_help.py"
printf '# fake\n' >"$TESTS/wallet_basic.py"
printf '# fake\n' >"$TESTS/p2p_ping.py"

good_inv() {
  cat >"$1" <<'EOF'
pin = "v31.1"
core_commit = "9be056a8a72b624dae9623b2f7bded92c2a21c91"

[[test]]
name = "feature_help.py"
status = "run"

[[test]]
name = "wallet_basic.py"
status = "skip"
reason = "no-wallet"

[[test]]
name = "p2p_ping.py"
status = "skip"
reason = "rpc-missing"
analog = "none"
EOF
}

# --- missing on disk ---
good_inv "$WORKDIR/inv.toml"
printf '# extra row only in inventory\n' >/dev/null
cat >>"$WORKDIR/inv.toml" <<'EOF'

[[test]]
name = "rpc_ghost.py"
status = "run"
EOF
assert_fail_msg "inventory names a missing file" "missing on disk: rpc_ghost.py" \
  python3 "$CHECK" --tests-dir "$TESTS" --inventory "$WORKDIR/inv.toml"

# --- file on disk not in inventory ---
good_inv "$WORKDIR/inv.toml"
printf '# fake\n' >"$TESTS/rpc_uptime.py"
assert_fail_msg "file on disk not in inventory" "not in inventory: rpc_uptime.py" \
  python3 "$CHECK" --tests-dir "$TESTS" --inventory "$WORKDIR/inv.toml"
rm -f "$TESTS/rpc_uptime.py"

# --- skip without reason ---
cat >"$WORKDIR/inv.toml" <<'EOF'
pin = "v31.1"
core_commit = "9be056a8a72b624dae9623b2f7bded92c2a21c91"

[[test]]
name = "feature_help.py"
status = "run"

[[test]]
name = "wallet_basic.py"
status = "skip"

[[test]]
name = "p2p_ping.py"
status = "skip"
reason = "rpc-missing"
EOF
assert_fail_msg "skip without reason" "skip without reason: wallet_basic.py" \
  python3 "$CHECK" --tests-dir "$TESTS" --inventory "$WORKDIR/inv.toml"

# --- run with reason ---
good_inv "$WORKDIR/inv.toml"
# rewrite feature_help as run+reason
python3 - <<PY
from pathlib import Path
p = Path("$WORKDIR/inv.toml")
t = p.read_text()
t = t.replace('name = "feature_help.py"\nstatus = "run"',
              'name = "feature_help.py"\nstatus = "run"\nreason = "no-wallet"')
p.write_text(t)
PY
assert_fail_msg "run must not set reason" "run must not set reason: feature_help.py" \
  python3 "$CHECK" --tests-dir "$TESTS" --inventory "$WORKDIR/inv.toml"

# --- reason=unknown illegal ---
cat >"$WORKDIR/inv.toml" <<'EOF'
pin = "v31.1"
core_commit = "9be056a8a72b624dae9623b2f7bded92c2a21c91"

[[test]]
name = "feature_help.py"
status = "run"

[[test]]
name = "wallet_basic.py"
status = "skip"
reason = "unknown"

[[test]]
name = "p2p_ping.py"
status = "skip"
reason = "rpc-missing"
EOF
assert_fail_msg "reason=unknown illegal" "illegal reason: unknown" \
  python3 "$CHECK" --tests-dir "$TESTS" --inventory "$WORKDIR/inv.toml"

# --- analog required for rpc-missing ---
cat >"$WORKDIR/inv.toml" <<'EOF'
pin = "v31.1"
core_commit = "9be056a8a72b624dae9623b2f7bded92c2a21c91"

[[test]]
name = "feature_help.py"
status = "run"

[[test]]
name = "wallet_basic.py"
status = "skip"
reason = "no-wallet"

[[test]]
name = "p2p_ping.py"
status = "skip"
reason = "rpc-missing"
EOF
assert_fail_msg "rpc-missing requires analog" "missing analog: p2p_ping.py" \
  python3 "$CHECK" --tests-dir "$TESTS" --inventory "$WORKDIR/inv.toml"

# --- analog required for rpc-dialect ---
cat >"$WORKDIR/inv.toml" <<'EOF'
pin = "v31.1"
core_commit = "9be056a8a72b624dae9623b2f7bded92c2a21c91"

[[test]]
name = "feature_help.py"
status = "run"

[[test]]
name = "wallet_basic.py"
status = "skip"
reason = "no-wallet"

[[test]]
name = "p2p_ping.py"
status = "skip"
reason = "rpc-dialect"
EOF
assert_fail_msg "rpc-dialect requires analog" "missing analog: p2p_ping.py" \
  python3 "$CHECK" --tests-dir "$TESTS" --inventory "$WORKDIR/inv.toml"

# --- analog required for no-prune / core-internal / no-utxo-set ---
cat >"$WORKDIR/inv.toml" <<'EOF'
pin = "v31.1"
core_commit = "9be056a8a72b624dae9623b2f7bded92c2a21c91"

[[test]]
name = "feature_help.py"
status = "run"

[[test]]
name = "wallet_basic.py"
status = "skip"
reason = "no-wallet"

[[test]]
name = "p2p_ping.py"
status = "skip"
reason = "no-prune"
EOF
assert_fail_msg "no-prune requires analog" "missing analog: p2p_ping.py" \
  python3 "$CHECK" --tests-dir "$TESTS" --inventory "$WORKDIR/inv.toml"

# --- node-behavior names its follow-up issue ---
nb_inv() {
  cat >"$1" <<EOF
pin = "v31.1"
core_commit = "9be056a8a72b624dae9623b2f7bded92c2a21c91"

[[test]]
name = "feature_help.py"
status = "run"

[[test]]
name = "wallet_basic.py"
status = "skip"
reason = "no-wallet"

[[test]]
name = "p2p_ping.py"
status = "skip"
reason = "node-behavior"
analog = "$2"
EOF
}
nb_inv "$WORKDIR/inv.toml" "follow-up: pong order"
assert_fail_msg "node-behavior requires an issue" "node-behavior needs a #issue: p2p_ping.py" \
  python3 "$CHECK" --tests-dir "$TESTS" --inventory "$WORKDIR/inv.toml"
nb_inv "$WORKDIR/inv.toml" "#1483: pong order"
assert_ok "node-behavior with an issue" \
  python3 "$CHECK" --tests-dir "$TESTS" --inventory "$WORKDIR/inv.toml"

# --- dangling core_analogs:: name ---
cat >"$WORKDIR/inv.toml" <<'EOF'
pin = "v31.1"
core_commit = "9be056a8a72b624dae9623b2f7bded92c2a21c91"

[[test]]
name = "feature_help.py"
status = "run"

[[test]]
name = "wallet_basic.py"
status = "skip"
reason = "no-wallet"

[[test]]
name = "p2p_ping.py"
status = "skip"
reason = "rpc-missing"
analog = "core_analogs::analog_does_not_exist"
EOF
assert_fail_msg "dangling core_analogs" "dangling analog: p2p_ping.py" \
  python3 "$CHECK" --tests-dir "$TESTS" --inventory "$WORKDIR/inv.toml"

# --- happy path ---
good_inv "$WORKDIR/inv.toml"
assert_ok "happy path" python3 "$CHECK" --tests-dir "$TESTS" --inventory "$WORKDIR/inv.toml"

echo
echo "$PASS passed, $FAIL failed"
if [ "$FAIL" -ne 0 ]; then
  exit 1
fi
