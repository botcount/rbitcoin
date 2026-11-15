#!/usr/bin/env bash
# Version-file gate. Exit 0 when Cargo.toml, nix/rbitcoin.nix, and CHANGELOG
# agree for the current workspace version. --kind prints ship|dev.
# --ci-extra enforces core-functional, overlay-functional, and warnet on ship
# PRs (DETECT_SHIP / CF_RESULT / OF_RESULT / WN_RESULT).
set -euo pipefail

ROOT=""
KIND_ONLY=0
CI_EXTRA=0
HERE="$(cd "$(dirname "$0")" && pwd)"

usage() {
  echo "usage: $0 [--kind] [--ci-extra] [--root DIR]" >&2
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --kind) KIND_ONLY=1 ;;
    --ci-extra) CI_EXTRA=1 ;;
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

if [[ -z "$ROOT" ]]; then
  ROOT="$(cd "$HERE/.." && pwd)"
fi
cd "$ROOT"
# shellcheck source=release-lib.sh
source "$HERE/release-lib.sh"

if [[ "$CI_EXTRA" -eq 1 ]]; then
  ship="${DETECT_SHIP:-}"
  if [[ "$ship" == "true" ]]; then
    for gate in core-functional:CF_RESULT overlay-functional:OF_RESULT warnet:WN_RESULT; do
      name="${gate%%:*}"
      var="${gate#*:}"
      result="${!var:-}"
      [[ "$result" == "success" ]] || \
        release_die "ship version PR requires $name success (got $var=${result:-empty})"
    done
  fi
  echo "release-gate: ci-extra ok (DETECT_SHIP=${ship:-} CF_RESULT=${CF_RESULT:-} OF_RESULT=${OF_RESULT:-} WN_RESULT=${WN_RESULT:-})"
  exit 0
fi

if [[ "$KIND_ONLY" -eq 1 ]]; then
  release_kind
  exit 0
fi

ver="$(release_cargo_workspace_version)"
[[ "$ver" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || release_die "Cargo.toml workspace version is not X.Y.Z: ${ver:-empty}"
release_parse_semver "$ver" || release_die "Cargo.toml workspace version is not X.Y.Z: ${ver:-empty}"

nix_ver="$(release_nix_package_version)"
[[ "$nix_ver" == "$ver" ]] || release_die "nix/rbitcoin.nix version=$nix_ver != Cargo.toml $ver"
release_check_lock_versions "$ver"

grep -qE '^## \[Unreleased\]' "$ROOT/CHANGELOG.md" || \
  release_die "CHANGELOG.md has no ## [Unreleased] heading"

if release_is_ship; then
  release_changelog_has_heading "$ver" || \
    release_die "CHANGELOG.md has no ## [$ver] heading"
  notes="$(release_changelog_notes "$ver")"
  [[ -n "$(printf '%s\n' "$notes" | grep -v '^[[:space:]]*$')" ]] || \
    release_die "CHANGELOG.md ## [$ver] section is empty"
  release_require_highlights "$ver"
fi

echo "release-gate: ok version=$ver kind=$(release_is_ship && echo ship || echo dev)"
