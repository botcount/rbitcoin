# 109 — Filter replies and compact blocks under-counted the send budget

**Severity:** medium
**Status:** fixed
**Found by:** @MrHodl, 2026-11-10

`cfilter`, `cfheaders`, and `cfcheckpt` took the 64-byte fallback.
`cmpctblock` took a fixed 1,024 bytes. A peer with the block-filter index
on could queue far more than the 4 MiB send budget. Served compact blocks
had the same gap.

The estimate is now the consensus payload length. One reply may still
cross the cap.

**Regression:** `rbitcoin-net` `peers::filter_and_compact_block_replies_charge_their_consensus_payload`.
