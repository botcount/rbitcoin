#!/usr/bin/env bash
# Hermetic pin for release-cut / release-gate / release-post / tag-only.
# Does not push anywhere.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CUT="$ROOT/scripts/release-cut.sh"
GATE="$ROOT/scripts/release-gate.sh"
POST="$ROOT/scripts/release-post.sh"
REL="$ROOT/scripts/release.sh"
NOTES="$ROOT/scripts/release-notes.sh"
PASS=0
FAIL=0
source "$ROOT/scripts/contract-assert.sh"
export GIT_AUTHOR_NAME=rbitcoin-release-test
export GIT_AUTHOR_EMAIL=test@example.invalid
export GIT_COMMITTER_NAME=rbitcoin-release-test
export GIT_COMMITTER_EMAIL=test@example.invalid
WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/rbitcoin-release-flow.XXXXXX")"
cleanup() { rm -rf "$WORKDIR"; }
trap cleanup EXIT


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

git_c() {
  git -c user.name=rbitcoin-release-test -c user.email=test@example.invalid "$@"
}

add_highlight() {
  local dest="$1"
  local bullet="$2"
  local tmp
  tmp="$(mktemp "${TMPDIR:-/tmp}/rbitcoin-hl.XXXXXX")"
  awk -v b="$bullet" '
    /^### Highlights$/ { print; print ""; print b; next }
    { print }
  ' "$dest/CHANGELOG.md" >"$tmp"
  mv "$tmp" "$dest/CHANGELOG.md"
}

write_version_files() {
  local dest="$1"
  local ver="$2"
  mkdir -p "$dest/nix"
  cat >"$dest/Cargo.toml" <<EOF
[workspace.package]
version = "${ver}"
EOF
  cat >"$dest/nix/rbitcoin.nix" <<EOF
{
  version = "${ver}";
}
EOF
}

seed_dev_tree() {
  local dest="$1"
  write_version_files "$dest" "0.5.99"
  cat >"$dest/CHANGELOG.md" <<'EOF'
## [Unreleased]

### Changed

- **Thing:** ship this.

## [0.5.1] — 2026-08-23

Notes for 0.5.1.
EOF
  git_c -C "$dest" init -q -b master
  git_c -C "$dest" add Cargo.toml nix/rbitcoin.nix CHANGELOG.md
  git_c -C "$dest" commit -q -m init
}

seed_ship_tree() {
  local dest="$1"
  write_version_files "$dest" "0.6.0"
  cat >"$dest/CHANGELOG.md" <<'EOF'
## [Unreleased]

## [0.6.0] — 2026-09-06

### Highlights

- **Operator binary:** musl snapshot for this tag.

## [0.5.1] — 2026-08-23

Notes for 0.5.1.
EOF
  git_c -C "$dest" init -q -b master
  git_c -C "$dest" add Cargo.toml nix/rbitcoin.nix CHANGELOG.md
  git_c -C "$dest" commit -q -m init
}

# --- cut: minor 0.5.99 → 0.6.0 ---
MINOR="$WORKDIR/minor"
mkdir -p "$MINOR"
seed_dev_tree "$MINOR"
assert_ok "cut --print-plan names 0.6.0 / v0.6.x / 0.6.99" \
  bash -c "bash '$CUT' --root '$MINOR' --minor --print-plan --date 2026-09-06 | grep -qx 'ship=0.6.0' && bash '$CUT' --root '$MINOR' --minor --print-plan --date 2026-09-06 | grep -qx 'maint=v0.6.x' && bash '$CUT' --root '$MINOR' --minor --print-plan --date 2026-09-06 | grep -qx 'dev_next=0.6.99'"

bash "$CUT" --root "$MINOR" --minor --date 2026-09-06
assert_ok "cut --minor writes Cargo 0.6.0" \
  grep -q 'version = "0.6.0"' "$MINOR/Cargo.toml"
assert_ok "cut --minor writes nix 0.6.0" \
  grep -q 'version = "0.6.0"' "$MINOR/nix/rbitcoin.nix"
assert_ok "cut --minor keeps Unreleased heading" \
  grep -qE '^## \[Unreleased\]' "$MINOR/CHANGELOG.md"
assert_ok "cut --minor adds dated 0.6.0 heading" \
  grep -qE '^## \[0\.6\.0\] — 2026-09-06' "$MINOR/CHANGELOG.md"
assert_ok "cut --minor moves Unreleased body under 0.6.0" \
  grep -q 'ship this' "$MINOR/CHANGELOG.md"
assert_ok "cut --minor inserts Highlights heading" \
  grep -qE '^### Highlights' "$MINOR/CHANGELOG.md"

# --- cut absorbs changelog.d into the shipped section ---
FRAG="$WORKDIR/frag"
mkdir -p "$FRAG"
seed_dev_tree "$FRAG"
mkdir -p "$FRAG/changelog.d"
cat >"$FRAG/changelog.d/715-qc.md" <<'EOF'
Fixed

- **QC:** one runner.
EOF
cat >"$FRAG/changelog.d/changed-note.md" <<'EOF'
Changed

- **Book:** fragments land in an existing section.
EOF
bash "$CUT" --root "$FRAG" --minor --date 2026-09-06
assert_ok "cut folds a new Fixed fragment under 0.6.0" \
  grep -q 'QC:' "$FRAG/CHANGELOG.md"
assert_ok "cut appends Changed fragment under the existing heading" \
  grep -q 'fragments land' "$FRAG/CHANGELOG.md"
assert_ok "cut keeps the Unreleased body that was already in CHANGELOG" \
  grep -q 'ship this' "$FRAG/CHANGELOG.md"
assert_ok "cut deletes absorbed fragments" \
  bash -c '! test -e "$FRAG/changelog.d/715-qc.md" && ! test -e "$FRAG/changelog.d/changed-note.md"'
BAD="$WORKDIR/badfrag"
mkdir -p "$BAD"
seed_dev_tree "$BAD"
mkdir -p "$BAD/changelog.d"
printf '%s\n' "Bogus" "- **No:**" >"$BAD/changelog.d/bad.md"
assert_fail_msg "cut rejects a fragment with an unknown category" \
  "category must be" \
  bash "$CUT" --root "$BAD" --minor --date 2026-09-07
assert_ok "gate kind is ship after minor cut" \
  bash -c "[[ \$(bash '$GATE' --root '$MINOR' --kind) == ship ]]"
assert_fail_msg "gate fails ship until Highlights has a bullet" \
  "Highlights" \
  bash "$GATE" --root "$MINOR"
add_highlight "$MINOR" "- **Thing:** operator-facing ship note."
assert_ok "gate passes ship tree after Highlights" \
  bash "$GATE" --root "$MINOR"
GIT_AUTHOR_NAME='Hero Gamer' \
GIT_AUTHOR_EMAIL='Hero-Gamer@users.noreply.github.com' \
GIT_COMMITTER_NAME='Hero Gamer' \
GIT_COMMITTER_EMAIL='Hero-Gamer@users.noreply.github.com' \
  git_c -C "$MINOR" commit -q --allow-empty -m 'external change'
GIT_AUTHOR_NAME='rearden-grok[bot]' \
GIT_AUTHOR_EMAIL='317016512+rearden-grok[bot]@users.noreply.github.com' \
GIT_COMMITTER_NAME='rearden-grok[bot]' \
GIT_COMMITTER_EMAIL='317016512+rearden-grok[bot]@users.noreply.github.com' \
  git_c -C "$MINOR" commit -q --allow-empty -m 'bot change'
GIT_AUTHOR_NAME='Brandon Black' \
GIT_AUTHOR_EMAIL='freedom@reardencode.com' \
GIT_COMMITTER_NAME='Brandon Black' \
GIT_COMMITTER_EMAIL='freedom@reardencode.com' \
  git_c -C "$MINOR" commit -q --allow-empty -m 'maintainer change'
GIT_AUTHOR_NAME='Brandon Black' \
GIT_AUTHOR_EMAIL='freedom@reardencode.com' \
GIT_COMMITTER_NAME='Brandon Black' \
GIT_COMMITTER_EMAIL='freedom@reardencode.com' \
  git_c -C "$MINOR" commit -q --allow-empty -m 'Merge pull request #9 from average-gary/mempool/x'
GIT_AUTHOR_NAME='Brandon Black' \
GIT_AUTHOR_EMAIL='freedom@reardencode.com' \
GIT_COMMITTER_NAME='Brandon Black' \
GIT_COMMITTER_EMAIL='freedom@reardencode.com' \
  git_c -C "$MINOR" commit -q --allow-empty -m 'Merge pull request #8 from reardencode/docs/x'
out="$(bash "$NOTES" --root "$MINOR")"
assert_ok "release-notes include Highlights bullet" \
  grep -q 'operator-facing ship note' <<<"$out"
assert_ok "release-notes without a one-shot file do not thank @otaliptus" \
  bash -c "! grep -q '@otaliptus' <<<'$out'"
assert_ok "release-notes thank another external login" \
  grep -q '@Hero-Gamer' <<<"$out"
assert_ok "release-notes thank the fork on a merge commit" \
  grep -q '@average-gary' <<<"$out"
assert_ok "release-notes keep that thanks inside Highlights" \
  bash -c "! grep -q '^### Thanks$' <<<'$out'"
# Thanks name logins as @login. The image line does not name a maintainer.
assert_ok "release-notes omit the bot and the maintainer" \
  bash -c "! grep -q '@rearden-grok' <<<'$out' && ! grep -q '@reardencode' <<<'$out'"
assert_ok "release-notes name both Linux musl arches" \
  grep -q 'musl x86_64 and aarch64' <<<"$out"
assert_ok "release-notes give the image pull for this version" \
  grep -qF 'docker pull ghcr.io/rbitcoin-org/rbitcoin:0.6.0' <<<"$out"
assert_ok "release-notes omit detailed Unreleased body" \
  bash -c "! grep -q 'ship this' <<<'$out'"
assert_ok "release-notes name CHANGELOG section" \
  grep -q '## \[0.6.0\]' <<<"$out"

# --- one-shot thanks is consumed by the cut ---
ONCE="$WORKDIR/once"
mkdir -p "$ONCE"
seed_dev_tree "$ONCE"
mkdir -p "$ONCE/changelog.d"
printf '%s\n' 'Thanks to @otaliptus for the security review, and to @dergoegge, @rob1ham, and @1440000bytes for earlier findings.' \
  >"$ONCE/changelog.d/thanks.md"
printf '%s\n' 'Fixed' '' '- **Beside thanks:** a normal fragment still folds.' \
  >"$ONCE/changelog.d/fixed-beside-thanks.md"
bash "$CUT" --root "$ONCE" --minor --date 2026-09-06
assert_ok "cut deletes changelog.d/thanks.md" \
  bash -c '! test -e "$ONCE/changelog.d/thanks.md"'
assert_ok "cut folds a normal fragment next to thanks.md" \
  grep -q 'Beside thanks' "$ONCE/CHANGELOG.md"
assert_ok "cut keeps one-shot thanks under the shipped version" \
  grep -q '@dergoegge' "$ONCE/CHANGELOG.md"
assert_ok "cut places one-shot thanks in Highlights" \
  awk '
    /^## \[0\.6\.0\]/ { p = 1 }
    p && /^## / && !/^## \[0\.6\.0\]/ { exit }
    p && /^### Highlights$/ { h = 1; next }
    p && h && /^### / { exit }
    p && h && /@dergoegge/ { found = 1 }
    END { exit !found }
  ' "$ONCE/CHANGELOG.md"
assert_ok "cut does not add a Thanks heading" \
  bash -c "! grep -q '^### Thanks$' '$ONCE/CHANGELOG.md'"
add_highlight "$ONCE" "- **Thing:** operator-facing ship note."
out="$(bash "$NOTES" --root "$ONCE")"
assert_ok "release-notes include the one-shot thanks" \
  bash -c "grep -q '@otaliptus' <<<'$out' && grep -q '@rob1ham' <<<'$out' && grep -q '@1440000bytes' <<<'$out'"
assert_ok "one-shot @otaliptus appears once" \
  bash -c "[[ \$(grep -o '@otaliptus' <<<'$out' | wc -l) -eq 1 ]]"
assert_ok "one-shot thanks stay inside Highlights" \
  bash -c "! grep -q '^### Thanks$' <<<'$out'"

# --- cut: major 0.5.99 → 1.0.0 ---
MAJOR="$WORKDIR/major"
mkdir -p "$MAJOR"
seed_dev_tree "$MAJOR"
bash "$CUT" --root "$MAJOR" --major --date 2026-09-06
assert_ok "cut --major writes 1.0.0" \
  grep -q 'version = "1.0.0"' "$MAJOR/Cargo.toml"
assert_fail_msg "cut --major refused when not a .99 dev version" \
  "patch 99" \
  bash "$CUT" --root "$MAJOR" --major --date 2026-09-07

# --- cut: patch 0.5.2 → 0.5.3 ---
PATCH="$WORKDIR/patch"
mkdir -p "$PATCH"
write_version_files "$PATCH" "0.5.2"
cat >"$PATCH/CHANGELOG.md" <<'EOF'
## [Unreleased]

### Fixed

- **Bug:** cherry-pick this.

## [0.5.2] — 2026-08-28

Patch notes.
EOF
git_c -C "$PATCH" init -q -b v0.5.x
git_c -C "$PATCH" add Cargo.toml nix/rbitcoin.nix CHANGELOG.md
git_c -C "$PATCH" commit -q -m init
bash "$CUT" --root "$PATCH" --patch --date 2026-09-06
assert_ok "cut --patch writes 0.5.3" \
  grep -q 'version = "0.5.3"' "$PATCH/Cargo.toml"
assert_ok "cut --patch adds 0.5.3 heading" \
  grep -qE '^## \[0\.5\.3\] — 2026-09-06' "$PATCH/CHANGELOG.md"

# A workspace member whose lock still names the pre-cut version. A
# third-party package that happens to use that same version string, and a
# nested lock's own package, must stay put.
seed_locked_member() {
  local dest="$1"
  local ver="$2"
  mkdir -p "$dest/crates/rbitcoin-node" "$dest/fuzz"
  cat >"$dest/crates/rbitcoin-node/Cargo.toml" <<'EOF'
[package]
name = "rbitcoin-node"
version.workspace = true
EOF
  cat >>"$dest/Cargo.toml" <<'EOF'

[workspace]
members = ["crates/rbitcoin-node"]
EOF
  cat >"$dest/Cargo.lock" <<EOF
version = 3

[[package]]
name = "chacha20"
version = "${ver}"

[[package]]
name = "rbitcoin-node"
version = "${ver}"
dependencies = [
 "rbitcoin-node ${ver}",
 "rbitcoin-node ${ver} (registry+https://github.com/rust-lang/crates.io-index)",
 "chacha20 ${ver}",
]
EOF
  cat >"$dest/fuzz/Cargo.lock" <<EOF
version = 3

[[package]]
name = "rbitcoin-fuzz"
version = "0.0.0"

[[package]]
name = "rbitcoin-node"
version = "${ver}"
EOF
}

assert_lock_moved() {
  local dest="$1"
  local new="$2"
  local old="$3"
  grep -A1 'name = "rbitcoin-node"' "$dest/Cargo.lock" | grep -q "version = \"${new}\"" \
    && grep -A1 'name = "chacha20"' "$dest/Cargo.lock" | grep -q "version = \"${old}\"" \
    && grep -q "\"rbitcoin-node ${new}\"" "$dest/Cargo.lock" \
    && grep -q "\"rbitcoin-node ${new} (registry+" "$dest/Cargo.lock" \
    && grep -q "\"chacha20 ${old}\"" "$dest/Cargo.lock" \
    && grep -A1 'name = "rbitcoin-node"' "$dest/fuzz/Cargo.lock" | grep -q "version = \"${new}\"" \
    && grep -A1 'name = "rbitcoin-fuzz"' "$dest/fuzz/Cargo.lock" | grep -q 'version = "0.0.0"'
}

LOCKCUT="$WORKDIR/lockcut"
mkdir -p "$LOCKCUT"
seed_dev_tree "$LOCKCUT"
seed_locked_member "$LOCKCUT" "0.5.99"
mkdir -p "$LOCKCUT/third_party/vendor"
cat >"$LOCKCUT/third_party/vendor/Cargo.lock" <<'EOF'
version = 3

[[package]]
name = "rbitcoin-node"
version = "0.5.99"
dependencies = [
 "rbitcoin-node 0.5.99",
]
EOF
bash "$CUT" --root "$LOCKCUT" --minor --date 2026-09-06
assert_ok "cut leaves a third_party lock alone" \
  bash -c "grep -q 'version = \"0.5.99\"' '$LOCKCUT/third_party/vendor/Cargo.lock' && grep -q '\"rbitcoin-node 0.5.99\"' '$LOCKCUT/third_party/vendor/Cargo.lock'"
assert_ok "cut --minor rewrites workspace versions in Cargo.lock" \
  assert_lock_moved "$LOCKCUT" "0.6.0" "0.5.99"

LOCKDEV="$WORKDIR/lockdev"
mkdir -p "$LOCKDEV"
seed_ship_tree "$LOCKDEV"
seed_locked_member "$LOCKDEV" "0.6.0"
bash "$CUT" --root "$LOCKDEV" --dev-next
assert_ok "cut --dev-next rewrites workspace versions in Cargo.lock" \
  assert_lock_moved "$LOCKDEV" "0.6.99" "0.6.0"

LOCKPATCH="$WORKDIR/lockpatch"
mkdir -p "$LOCKPATCH"
write_version_files "$LOCKPATCH" "0.5.2"
cat >"$LOCKPATCH/CHANGELOG.md" <<'EOF'
## [Unreleased]

### Fixed

- **Bug:** cherry-pick this.

## [0.5.2] — 2026-08-28

Patch notes.
EOF
seed_locked_member "$LOCKPATCH" "0.5.2"
git_c -C "$LOCKPATCH" init -q -b v0.5.x
git_c -C "$LOCKPATCH" add Cargo.toml nix/rbitcoin.nix CHANGELOG.md crates fuzz
git_c -C "$LOCKPATCH" commit -q -m init
bash "$CUT" --root "$LOCKPATCH" --patch --date 2026-09-06
assert_ok "cut --patch rewrites workspace versions in Cargo.lock" \
  assert_lock_moved "$LOCKPATCH" "0.5.3" "0.5.2"

STALE="$WORKDIR/stale-lock"
mkdir -p "$STALE"
write_version_files "$STALE" "0.6.0"
cat >"$STALE/CHANGELOG.md" <<'EOF'
## [Unreleased]

## [0.6.0] — 2026-09-06

### Highlights

- **Pin:** lock must match.
EOF
seed_locked_member "$STALE" "0.5.99"
assert_fail_msg "gate fails when Cargo.lock still names the old workspace version" \
  "Cargo.lock" \
  bash "$GATE" --root "$STALE"

# The lock is not on the pre-cut version. Rewriting only an exact old
# string would leave it stale and still exit 0.
DRIFT="$WORKDIR/lock-drift"
mkdir -p "$DRIFT"
seed_dev_tree "$DRIFT"
seed_locked_member "$DRIFT" "0.4.0"
assert_fail_msg "cut refuses a lock that is not on the pre-cut version" \
  "Cargo.lock" \
  bash "$CUT" --root "$DRIFT" --minor --date 2026-09-06

NOLOCK="$WORKDIR/no-lock"
mkdir -p "$NOLOCK"
seed_dev_tree "$NOLOCK"
seed_locked_member "$NOLOCK" "0.5.99"
rm -f "$NOLOCK/Cargo.lock"
assert_fail_msg "cut refuses a tree with no root Cargo.lock" \
  "Cargo.lock is missing" \
  bash "$CUT" --root "$NOLOCK" --minor --date 2026-09-06

DEV99="$WORKDIR/dev99"
mkdir -p "$DEV99"
seed_dev_tree "$DEV99"
assert_fail_msg "cut --patch refused on .99 dev tree" \
  "patch 99" \
  bash "$CUT" --root "$DEV99" --patch --date 2026-09-06

# --- cut: --dev-next 0.6.0 → 0.6.99 ---
DEVNEXT="$WORKDIR/devnext"
mkdir -p "$DEVNEXT"
seed_ship_tree "$DEVNEXT"
bash "$CUT" --root "$DEVNEXT" --dev-next
assert_ok "cut --dev-next writes Cargo 0.6.99" \
  grep -q 'version = "0.6.99"' "$DEVNEXT/Cargo.toml"
assert_ok "cut --dev-next writes nix 0.6.99" \
  grep -q 'version = "0.6.99"' "$DEVNEXT/nix/rbitcoin.nix"
assert_ok "cut --dev-next notes in-tree toward 0.7.0" \
  grep -q '0.7.0' "$DEVNEXT/CHANGELOG.md"
assert_ok "gate kind is dev after --dev-next" \
  bash -c "[[ \$(bash '$GATE' --root '$DEVNEXT' --kind) == dev ]]"
assert_ok "gate passes .99 tree" \
  bash "$GATE" --root "$DEVNEXT"

assert_fail_msg "cut --dev-next refused unless patch is 0" \
  "patch 0" \
  bash "$CUT" --root "$PATCH" --dev-next

# --- gate: mismatches ---
BAD="$WORKDIR/bad"
mkdir -p "$BAD"
write_version_files "$BAD" "0.6.0"
# nix disagrees
cat >"$BAD/nix/rbitcoin.nix" <<'EOF'
{
  version = "0.6.1";
}
EOF
cat >"$BAD/CHANGELOG.md" <<'EOF'
## [Unreleased]

## [0.6.0] — 2026-09-06

Notes.
EOF
assert_fail_msg "gate fails when nix version disagrees" \
  "nix/rbitcoin.nix" \
  bash "$GATE" --root "$BAD"

NOCL="$WORKDIR/nocl"
mkdir -p "$NOCL"
write_version_files "$NOCL" "0.6.0"
cat >"$NOCL/CHANGELOG.md" <<'EOF'
## [Unreleased]

## [0.5.1] — 2026-08-23

Notes.
EOF
assert_fail_msg "gate fails ship version without changelog heading" \
  "CHANGELOG.md" \
  bash "$GATE" --root "$NOCL"

# --- gate --ci-extra ---
ALL_OK=(CF_RESULT=success OF_RESULT=success WN_RESULT=success)
assert_ok "ci-extra passes ship when all functional gates succeeded" \
  env DETECT_SHIP=true "${ALL_OK[@]}" bash "$GATE" --root "$MINOR" --ci-extra
assert_fail_msg "ci-extra fails ship when core-functional skipped" \
  "core-functional" \
  env DETECT_SHIP=true "${ALL_OK[@]}" CF_RESULT=skipped bash "$GATE" --root "$MINOR" --ci-extra
assert_fail_msg "ci-extra fails ship when overlay-functional failed" \
  "overlay-functional" \
  env DETECT_SHIP=true "${ALL_OK[@]}" OF_RESULT=failure bash "$GATE" --root "$MINOR" --ci-extra
assert_fail_msg "ci-extra fails ship when warnet skipped" \
  "warnet" \
  env DETECT_SHIP=true "${ALL_OK[@]}" WN_RESULT=skipped bash "$GATE" --root "$MINOR" --ci-extra
assert_ok "ci-extra passes dev when functional gates skipped" \
  env DETECT_SHIP=false CF_RESULT=skipped OF_RESULT=skipped WN_RESULT=skipped \
  bash "$GATE" --root "$DEVNEXT" --ci-extra

# --- latest maint branch ---
MAINT="$WORKDIR/maint"
mkdir -p "$MAINT"
seed_dev_tree "$MAINT"
git_c -C "$MAINT" branch v0.4.x
git_c -C "$MAINT" branch v0.5.x
assert_ok "latest maint branch is v0.5.x" \
  bash -c "[[ \$(bash '$CUT' --root '$MAINT' --latest-maint) == v0.5.x ]]"

# --- release.sh refuses .99 ---
assert_fail_msg "tag refuses .99 dev version" \
  "not a ship version" \
  bash "$REL" --root "$DEVNEXT" --no-push --allow-branch master

# --- release.sh allows vX.Y.x without --allow-branch ---
MAINTREL="$WORKDIR/maintrel"
mkdir -p "$MAINTREL"
seed_ship_tree "$MAINTREL"
git_c -C "$MAINTREL" branch -m v0.6.x
git_c clone -q --bare "$MAINTREL" "$WORKDIR/origin-maintrel.git"
git_c -C "$MAINTREL" remote add origin "$WORKDIR/origin-maintrel.git"
out="$(bash "$REL" --root "$MAINTREL" --dry-run)"
assert_ok "dry-run ok on v0.6.x without --allow-branch" \
  grep -q "dry-run ok" <<<"$out"

# --- tag-only prints tag push, not branch ---
TAGONLY="$WORKDIR/tagonly"
mkdir -p "$TAGONLY"
seed_ship_tree "$TAGONLY"
out="$(bash "$REL" --root "$TAGONLY" --no-push --tag-only --allow-branch master)"
assert_ok "tag-only push line is tags only" \
  grep -qx "  git push origin refs/tags/v0.6.0" <<<"$out"
assert_ok "tag-only does not push branch ref" \
  bash -c "! grep -q 'refs/heads/' <<<'$out'"

# --- post: tag + vX.Y.x ---
POSTT="$WORKDIR/post"
mkdir -p "$POSTT"
seed_ship_tree "$POSTT"
git_c clone -q --bare "$POSTT" "$WORKDIR/origin-post.git"
git_c -C "$POSTT" remote add origin "$WORKDIR/origin-post.git"
out="$(bash "$POST" --root "$POSTT" --no-push)"
assert_ok "post creates annotated v0.6.0" \
  git_c -C "$POSTT" show-ref --verify --quiet refs/tags/v0.6.0
assert_ok "post creates v0.6.x at HEAD" \
  bash -c "[[ \$(git -C '$POSTT' rev-parse refs/heads/v0.6.x) == \$(git -C '$POSTT' rev-parse HEAD) ]]"
assert_ok "post prints dev_next=0.6.99" \
  grep -q "dev_next=0.6.99" <<<"$out"
assert_ok "post prints maint=v0.6.x" \
  grep -q "maint=v0.6.x" <<<"$out"

# post on a patch (0.5.3) must not invent v0.5.x from patch!=0? 
# 0.5.3 is a patch ship on existing maint; post should tag but not require creating maint
# (branch already is v0.5.x). Use PATCH tree after cut (0.5.3), retarget.
PPOST="$WORKDIR/ppost"
mkdir -p "$PPOST"
write_version_files "$PPOST" "0.5.3"
cat >"$PPOST/CHANGELOG.md" <<'EOF'
## [Unreleased]

## [0.5.3] — 2026-09-06

### Highlights

- **Fix:** cherry-picked operator note.

EOF
git_c -C "$PPOST" init -q -b v0.5.x
git_c -C "$PPOST" add Cargo.toml nix/rbitcoin.nix CHANGELOG.md
git_c -C "$PPOST" commit -q -m init
out="$(bash "$POST" --root "$PPOST" --no-push)"
assert_ok "post on patch tags v0.5.3" \
  git_c -C "$PPOST" show-ref --verify --quiet refs/tags/v0.5.3
assert_ok "post on patch does not print dev_next" \
  bash -c "! grep -q 'dev_next=' <<<'$out'"

# --- live tree gate (this checkout) ---
assert_ok "gate passes live worktree" \
  bash "$GATE" --root "$ROOT"

if [[ "$FAIL" -ne 0 ]]; then
  echo "release-flow.test.sh: $PASS passed, $FAIL failed"
  exit 1
fi
echo "release-flow.test.sh: $PASS passed"
