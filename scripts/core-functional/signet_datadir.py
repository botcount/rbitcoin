"""Core's per-network datadir name for the test shims. Not the node.

The node never derives a datadir. Core puts a custom signet under
`signet_<message start hex>` (`GetSignetDataDir`), so the shims map
`-datadir=DIR` the same way before handing the node `--datadir`.
"""

from __future__ import annotations

import hashlib

SIGNET_DEFAULT_CHALLENGE = bytes.fromhex(
    "512103ad5e0edad18cb1f0fc0d28a3d4f1f3e445640337489abb10404f2d1e086be43021"
    "0359ef5021964fe22d6f8e05b2463c9540ce96883fe3b278760f048f5189f2e6c452ae"
)

# Core `IsSpace`.
_SPACE = " \f\n\r\t\v"
_HEX = "0123456789abcdefABCDEF"


def parse_hex(raw: str) -> bytes | None:
    """Core `TryParseHex`: whitespace between bytes is skipped. Empty is valid."""
    out = bytearray()
    i = 0
    while i < len(raw):
        if raw[i] in _SPACE:
            i += 1
            continue
        pair = raw[i : i + 2]
        if len(pair) != 2 or pair[0] not in _HEX or pair[1] not in _HEX:
            return None
        out.append(int(pair, 16))
        i += 2
    return bytes(out)


def compact_size(n: int) -> bytes:
    """Bitcoin CompactSize, as Core serializes a script's length."""
    if n < 0xFD:
        return bytes([n])
    if n <= 0xFFFF:
        return b"\xfd" + n.to_bytes(2, "little")
    if n <= 0xFFFFFFFF:
        return b"\xfe" + n.to_bytes(4, "little")
    return b"\xff" + n.to_bytes(8, "little")


def message_start(challenge: bytes) -> str:
    """Core `GetSignetMessageStart`: sha256d of the serialized script, first 4 bytes."""
    ser = compact_size(len(challenge)) + challenge
    return hashlib.sha256(hashlib.sha256(ser).digest()).digest()[:4].hex()


def chain_dir(network: str, challenge: str | None) -> str:
    """`regtest`, `signet`, or `signet_<magic>` for a custom challenge."""
    if network != "signet" or challenge is None:
        return network
    script = parse_hex(challenge)
    if script is None or script == SIGNET_DEFAULT_CHALLENGE:
        return network
    return f"signet_{message_start(script)}"
