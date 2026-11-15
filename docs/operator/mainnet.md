# Mainnet

Run mainnet. Agreement with Bitcoin Core is close enough that a consensus
split is vanishingly unlikely. The version still has a leading zero. **1.0**
is the release planned for further hardening, a support window, and signed
release binaries. Operators who want those can wait. This page is the
runbook. Flags and config keys are the other pages in this directory.
[`OPERATOR.md`](../../OPERATOR.md) is the task index. The on-disk format
stays in [`SCHEMA.md`](../../SCHEMA.md).

In-tree is **0.9.99**, toward 0.10.0. The published GitHub Release is
**0.9.0**. The patch line is **`v0.9.x`**. The container image and the
Linux aarch64 binaries ship with 0.9 ([`setup.md`](setup.md)).

## What you are running

The default store is a full archive. Plan a 1 TB-class disk. One reference
mainnet store, counted on 2026-08-13 at height 962,298, was about **200 GiB**
hot and about **730 GiB** with the cold files
([`SCHEMA.md`](../../SCHEMA.md) census). That count is not a quota. The tip
has moved.

The default mainnet milestone is block **966143**, anchored to
`00000000000000000000748969ec33043c0e52a763c6dd5193861f559f2c72e3`
(Bitcoin Core 32.1 `defaultAssumeValid`).
Script and signature checks at or below that height are skipped only when
that hash and the block being connected are the header-path occupants at
their heights. The check is two height lookups. It does not walk parent
headers, and stored minimum chain work is not a gate for the skip.
Previous-output, double-spend, maturity, and fee checks still run. An output
whose script starts with `OP_RETURN`, or is longer than 10,000 bytes, is
still not a coin. An explicit `--milestone HEIGHT` skips by height alone,
with no anchor hash. `--milestone 0` checks every script. Pick that before
the first sync. Changing it later does not recheck blocks already stored.
How this differs from Bitcoin Core’s assumevalid is in
[`COMPAT.md`](../../COMPAT.md).

Signet, testnet4, and testnet5 already check every script. Use them to try a
wallet or to see that a binary connects. They are not a step you have to
finish before mainnet. `--network testnet4` is Bitcoin Core’s testnet4: BIP94,
peer port 48333, RPC port 48332. `--network testnet5` is BIP 95’s Testnet 5:
BIP 54 from block 1, no minimum-difficulty blocks, peer port 18353, RPC port
18352. It has no seeds yet, so name a peer with `--connect`.

A 0.7 directory (schema 24) or a 0.8 directory (schema 26) opens in place.
Schema 22 through 27 rewrite `meta` to 28 on open. That includes a datadir
that never pruned witnesses. A schema-27 binary refuses 28 `meta`, and a 0.9.0 binary does too. A directory
from 0.6 or earlier is refused: wipe that datadir and sync again. Do not
point an older binary at a directory this release has opened.

## Catch-up

```bash
./target/release/rbitcoin-node \
  --datadir /path/to/datadir-mainnet \
  --network mainnet \
  --listen 0.0.0.0:8333 \
  --log-level info
```

Use a datadir that is not the signet, testnet4, or testnet5 directory.
`--listen 0.0.0.0:8333` is what accepts inbound peers. Without it the node
dials out and does not accept them.

Stop with Ctrl+C or SIGTERM. The node flushes and exits 0. Start again with
the same `--datadir`. There is no resume flag. `kill -9` can drop the last
unflushed pages ([`crash-recovery.md`](../crash-recovery.md)).

One run of 0.9.99 on a ThinkPad X1 Carbon Gen 13 (Intel Core Ultra 7 258V,
32 GiB RAM, Crucial T500 2 TB) started at 2026-11-09T17:52Z with the default
milestone 966143 and the scripthash index on. Block catch-up reached height
975252 at 2026-11-10T03:43Z (9 h 51 min). The node left IBD at
2026-11-10T04:03Z after the scripthash seal (`sh_tip_ready`), 10 h 12 min
from process start. On the `ibd: progress` line the body-queue heap peaked
at 1023 MiB (`RAM=` in `progress.rs` is queue heap, not process RSS). At
scripthash collect start, `MemAvailable` was 20.1 GiB (`free_gib_label`).
That is one measured run, not a warranty. A 16 GiB machine or a slow disk
wants the knobs in [`field-notes.md`](field-notes.md). A tight uplink wants
`--max-outbound 8`, also in that file. The default outbound count is 16,
and the IBD floor is 8.

## Wallets

Leave Electrum, Esplora, and RPC off until catch-up has finished. During
IBD the history those servers would return is incomplete. After the node is
at the tip, add the listeners you want. Address history needs `--sh-index`.
Electrum and Esplora start without it and then answer
`scripthash index disabled` for those methods. Which wallets connect is
[`wallets.md`](../wallets.md). Lightning backends are
[`lightning.md`](../lightning.md).

Keep those listeners on loopback unless TLS and access control sit in front
of them.

## What this is not

This is not a full Bitcoin Core RPC clone, and it is not Fulcrum. The
default milestone is not a claim that every historical script was checked.
Release binaries are not signed. Further hardening, a support window, and
signed binaries are what 1.0 is for.

Compact-block relay and package accept are in
[`operations.md`](operations.md). Product differences, including
`--trust-peer`, are in [`COMPAT.md`](../../COMPAT.md).
`--trust-peer` stores a chain this node considers invalid. Do not point a
wallet at that node unless you accept that peer.
