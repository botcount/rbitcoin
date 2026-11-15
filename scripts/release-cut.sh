#!/usr/bin/env bash
# Cut a ship version (or the post-release X.Y.99 bump) in Cargo.toml,
# nix/rbitcoin.nix, and CHANGELOG.md. Does not commit, tag, or push.
set -euo pipefail

ROOT=""
MODE=""
PRINT_PLAN=0
DRY=0
DATE=""
HERE="$(cd "$(dirname "$0")" && pwd)"

usage() {
  echo "usage: $0 --minor|--major|--patch|--dev-next|--set X.Y.Z|--latest-maint [--print-plan] [--dry-run] [--date YYYY-MM-DD] [--root DIR]" >&2
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --minor|--major|--patch|--dev-next|--latest-maint)
      [[ -z "$MODE" ]] || usage
      MODE="${1#--}"
      ;;
    --set)
      [[ $# -ge 2 && -z "$MODE" ]] || usage
      MODE="set"
      SET_VER="$2"
      shift
      ;;
    --print-plan) PRINT_PLAN=1 ;;
    --dry-run) DRY=1 ;;
    --date)
      [[ $# -ge 2 ]] || usage
      DATE="$2"
      shift
      ;;
    --root)
      [[ $# -ge 2 ]] || usage
      ROOT="$2"
      shift
      ;;
    -h|--help) usage ;;
    *) usage ;;
  esac
  shift
done

[[ -n "$MODE" ]] || usage

if [[ -z "$ROOT" ]]; then
  ROOT="$(cd "$HERE/.." && pwd)"
fi
cd "$ROOT"
# shellcheck source=release-lib.sh
source "$HERE/release-lib.sh"

if [[ "$MODE" == "latest-maint" ]]; then
  release_latest_maint_branch || release_die "no vX.Y.x maintenance branch found"
  exit 0
fi

ver="$(release_cargo_workspace_version)"
[[ "$ver" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || release_die "Cargo.toml workspace version is not X.Y.Z: ${ver:-empty}"

case "$MODE" in
  minor)
    release_plan_minor "$ver" || release_die "cut --minor requires a .99 dev version (got $ver); patch 99 is the in-tree sentinel"
    ;;
  major)
    release_plan_major "$ver" || release_die "cut --major requires a .99 dev version (got $ver); patch 99 is the in-tree sentinel"
    ;;
  patch)
    release_plan_patch "$ver" || release_die "cut --patch requires a published X.Y.Z with patch < 98 (got $ver); not a .99 tree (patch 99 is master)"
    ;;
  dev-next)
    release_plan_dev_next "$ver" || release_die "cut --dev-next requires a just-shipped X.Y.0 (patch 0); got $ver"
    ;;
  set)
    release_parse_semver "$SET_VER" || release_die "--set $SET_VER is not X.Y.Z"
    REL_SHIP="$SET_VER"
    REL_MAINT="v${REL_MAJOR}.${REL_MINOR}.x"
    if [[ "$REL_PATCH" == "0" ]]; then
      REL_DEV_NEXT="${REL_MAJOR}.${REL_MINOR}.99"
    else
      REL_DEV_NEXT=""
    fi
    ;;
esac

print_plan() {
  if [[ "$MODE" == "dev-next" ]]; then
    echo "dev_next=${REL_DEV_NEXT}"
    echo "maint=${REL_MAINT}"
    echo "toward=${REL_TOWARD}"
    return 0
  fi
  echo "ship=${REL_SHIP}"
  echo "maint=${REL_MAINT}"
  if [[ -n "${REL_DEV_NEXT:-}" ]]; then
    echo "dev_next=${REL_DEV_NEXT}"
  fi
  return 0
}

if [[ "$PRINT_PLAN" -eq 1 ]]; then
  print_plan
  exit 0
fi

[[ -n "$DATE" ]] || DATE="$(date -u +%Y-%m-%d)"

if [[ "$DRY" -eq 1 ]]; then
  print_plan
  echo "release-cut: dry-run (no files written)"
  exit 0
fi

if [[ "$MODE" == "dev-next" ]]; then
  release_set_cargo_version "$REL_DEV_NEXT"
  release_set_nix_version "$REL_DEV_NEXT"
  release_set_lock_versions "$ver" "$REL_DEV_NEXT"
  release_check_lock_versions "$REL_DEV_NEXT"
  release_cut_changelog_dev_next "$REL_DEV_NEXT" "$REL_TOWARD" "$ver" "$REL_MAINT"
  echo "release-cut: $ver → $REL_DEV_NEXT (dev)"
  print_plan
  exit 0
fi

release_set_cargo_version "$REL_SHIP"
release_set_nix_version "$REL_SHIP"
release_set_lock_versions "$ver" "$REL_SHIP"
release_check_lock_versions "$REL_SHIP"
release_cut_changelog_ship "$REL_SHIP" "$DATE"
notes="$(release_changelog_notes "$REL_SHIP")"
[[ -n "$(printf '%s\n' "$notes" | grep -v '^[[:space:]]*$')" ]] || \
  release_die "CHANGELOG.md ## [$REL_SHIP] section is empty after cut (Unreleased had no notes)"
echo "release-cut: $ver → $REL_SHIP"
print_plan
