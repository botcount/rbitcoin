#!/usr/bin/env python3
"""Classify every Core functional *.py; fail if the inventory is incomplete.

See docs/core-functional.md. No network, no node.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

try:
    import tomllib
except ImportError:  # pragma: no cover
    print("check_inventory: need Python 3.11+ (tomllib)", file=sys.stderr)
    sys.exit(2)

VALID_REASONS = frozenset(
    {
        "no-wallet",
        "no-mining-product",
        "no-prune",
        "no-utxo-set",
        "no-zmq",
        "no-ipc",
        "no-qt",
        "no-tool",
        "no-core-rest",
        "v1-only",
        "core-log",
        "core-internal",
        "core-net-policy",
        "policy-libre",
        "rpc-missing",
        "rpc-dialect",
        "node-behavior",
        "core-cpp-unit",
        "prev-release",
        "harness",
        # "unknown" is listed in the design as illegal — do not add it here.
    }
)

HERE = Path(__file__).resolve().parent
DEFAULT_INVENTORY = HERE / "inventory.toml"
DEFAULT_NAMES = HERE / "d206226b-tests.txt"
ANALOG_RS = HERE.parents[1] / "crates/rbitcoin-test/tests/core_analogs.rs"


def load_names_from_dir(tests_dir: Path) -> list[str]:
    names = sorted(p.name for p in tests_dir.glob("*.py"))
    if not names:
        raise SystemExit(f"no *.py in {tests_dir}")
    return names


def load_names_from_file(path: Path) -> list[str]:
    names = [
        line.strip()
        for line in path.read_text().splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    if not names:
        raise SystemExit(f"no names in {path}")
    return names


def check(inventory_path: Path, disk_names: list[str]) -> list[str]:
    raw = inventory_path.read_bytes()
    data = tomllib.loads(raw.decode())
    rows = data.get("test")
    if not isinstance(rows, list):
        return ["inventory: missing [[test]] array"]

    analog_src = ANALOG_RS.read_text() if ANALOG_RS.is_file() else ""
    errors: list[str] = []
    seen: dict[str, int] = {}
    for i, row in enumerate(rows):
        if not isinstance(row, dict):
            errors.append(f"test[{i}]: not a table")
            continue
        name = row.get("name")
        status = row.get("status")
        reason = row.get("reason")
        if not name or not isinstance(name, str):
            errors.append(f"test[{i}]: missing name")
            continue
        if name in seen:
            errors.append(f"duplicate: {name}")
        seen[name] = i
        if status == "run":
            if reason:
                errors.append(f"run must not set reason: {name}")
        elif status == "skip":
            if not reason:
                errors.append(f"skip without reason: {name}")
            elif reason == "unknown":
                errors.append(f"illegal reason: unknown ({name})")
            elif reason not in VALID_REASONS:
                errors.append(f"illegal reason: {reason} ({name})")
            elif reason in (
                "no-prune",
                "core-internal",
                "no-utxo-set",
                "rpc-missing",
                "rpc-dialect",
            ):
                analog = row.get("analog")
                if not analog or not str(analog).strip():
                    errors.append(f"missing analog: {name}")
            elif reason == "node-behavior":
                if not re.search(r"#\d+", str(row.get("analog") or "")):
                    errors.append(f"node-behavior needs a #issue: {name}")
        else:
            errors.append(f"{name}: status must be run or skip")

        analog = str(row.get("analog") or "").strip()
        if analog.startswith("core_analogs::"):
            fn = analog.split("::", 1)[1].strip()
            if f"fn {fn}(" not in analog_src:
                errors.append(f"dangling analog: {name} -> {analog}")

    inv_names = set(seen)
    disk = set(disk_names)
    for n in sorted(inv_names - disk):
        errors.append(f"missing on disk: {n}")
    for n in sorted(disk - inv_names):
        errors.append(f"not in inventory: {n}")
    return errors


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--inventory", type=Path, default=DEFAULT_INVENTORY)
    p.add_argument(
        "--tests-dir",
        type=Path,
        default=None,
        help="Core test/functional directory (*.py on disk)",
    )
    p.add_argument(
        "--names-file",
        type=Path,
        default=None,
        help="Fallback list of *.py names (used when --tests-dir omitted)",
    )
    args = p.parse_args(argv)

    if args.tests_dir is not None:
        disk = load_names_from_dir(args.tests_dir)
    else:
        names_file = args.names_file or DEFAULT_NAMES
        disk = load_names_from_file(names_file)

    if not args.inventory.is_file():
        print(f"missing inventory: {args.inventory}", file=sys.stderr)
        return 1

    errors = check(args.inventory, disk)
    if errors:
        for e in errors:
            print(e, file=sys.stderr)
        print(f"check_inventory: {len(errors)} error(s)", file=sys.stderr)
        return 1
    print(f"check_inventory: ok ({len(disk)} tests, pin file {args.inventory.name})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
