# Errata

Known limitations that are **not** treated as current must-fix bugs.
Durable confirm still uses TipOnly (connected instance). See
[`invariants.md`](./invariants.md) leftover identity and
[`017-duplicate-txid-unconnected-instance.md`](./external_findings/017-duplicate-txid-unconnected-instance.md).

## RAM leftover maps: one fk per txid

In-flight `creates` and pipeline pin `by_txid` are `txid → one fk` (last
write). A second `create_fk` for the same txid **clobbers**
the first in that map. Do not stall the pipeline on a second fk (forget
cannot run).

**Pre-BIP30:** clobber is correct enough. The maps are a thin write-behind
/ pipeline identity, not a BIP30 instance list. Newest/last entry is an
acceptable single identity; durable TipOnly still prefers connected.

Stamp/load may bind a parent `create_fk` from in-flight that is **ahead of**
or **not** the write-batch’s create (same txid at two live pack heights, or
a spend whose in-flight identity is the later overwrite). Write rejects that
bind fail-closed (spentness / missing create / `SpansTip`). That is not a
fork.

**Post-BIP30:** a second *connected* create of the same txid is invalid.
The only realistic overlap is a **disconnected** Class A sibling (reorg,
same tx on the new tip) still sitting in a RAM map while the new fk is
noted. Last-write could hide one of them — a *possible* identity
visibility hole. Unlikely: both rows stay on disk; TipOnly still picks
connected; n−1 is held in in-flight until after the child pin.

Do not grow these maps to `Vec<Fk>` unless a mainnet miss is shown to
be this case.

## A block that looks commitment-free is served unverified

Reconstruction checks a rebuilt block against the witness commitment in its
coinbase ([`reconstruct.rs`](../crates/rbitcoin-query/src/reconstruct.rs)). The
check runs only when the coinbase carries a commitment **and** a reserved value
of exactly one 32-byte witness item. Without both there is nothing to compare:
every pre-segwit block is in that state, and a block stored before segwit
activation can carry a commitment with no coinbase witness at all.

Corruption in the six magic bytes (`6a24aa21a9ed`) of the commitment script,
or one that leaves the coinbase with no reserved value or the wrong size,
therefore leaves a block unverified. A flipped byte anywhere else in the
witness table, or in the commitment digest, is refused on the rebuilt block and
on the encoded bytes full-block `getdata` returns. Not checked: `getblock`
verbosity 1, whose only witness bytes are the coinbase's reserved value and
whose body read would be reachable unauthenticated through REST
`block/notxdetails`; `getrawtransaction`; and Electrum. A refused block still
needs a reindex: the check stops bad data being served, it does not repair it
([106](./external_findings/106-historical-bitrot.md)).

Closing this needs the height, so a commitment can be required at or above
segwit activation. Do not key the check on witness presence instead: storage
tests store witness-carrying blocks that were never valid, and those must keep
reconstructing.

## Retired confirm dual paths

A missed fact the pipeline promised is
`StoreError::Corrupt("invariant: …")` ([`invariants.md`](./invariants.md)).
The old dual-path names are gone.

`tx_apply_to_tx`, `connect_block`, and `commit_class_a_only` exist only on
`rbitcoin_query::testutil::FixtureChain`.
