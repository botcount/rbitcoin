#!/usr/bin/env bash
# clang for libFuzzer. Ubuntu 24.04 runner images already ship it. The fuzz
# nightly was failing in `apt-get update` when azure.archive.ubuntu.com did
# not resolve, before any fuzz target compiled. Install only when clang is
# missing, and retry through archive.ubuntu.com after that DNS failure.
set -euo pipefail

rewrite_one() {
  local f="$1"
  [[ -f "$f" ]] || return 0
  grep -q 'azure\.archive\.ubuntu\.com' "$f" || return 0
  sed -i 's/azure\.archive\.ubuntu\.com/archive.ubuntu.com/g' "$f"
}

# APT_MIRROR_ROOT is the apt directory (default /etc/apt). Tests point it
# at a temp tree. apt_install is overridden the same way.
rewrite_azure_mirrors() {
  local root="${APT_MIRROR_ROOT:-/etc/apt}"
  local f
  rewrite_one "$root/sources.list"
  rewrite_one "$root/apt-mirrors.txt"
  if [[ -d "$root/sources.list.d" ]]; then
    for f in "$root/sources.list.d"/*; do
      [[ -e "$f" ]] || continue
      rewrite_one "$f"
    done
  fi
}

if ! declare -F apt_install >/dev/null 2>&1; then
  apt_install() {
    sudo apt-get update && sudo apt-get install -y clang
  }
fi

ensure_clang() {
  local bin="${FUZZ_CLANG_BIN:-clang}"
  if command -v "$bin" >/dev/null 2>&1; then
    return 0
  fi
  if apt_install; then
    return 0
  fi
  rewrite_azure_mirrors
  apt_install
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  ensure_clang
fi
