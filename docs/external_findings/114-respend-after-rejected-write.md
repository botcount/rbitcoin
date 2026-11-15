# 114 — A re-spend of a same-block spend connected after a merged write rejected

**Severity:** critical — consensus split
**Status:** fixed
**Found by:** consensus audit against Core `d206226b`, 2026-11-10 (two independent repros)

Core rejects a block that spends an output a lower block already spent
(`bad-txns-inputs-missingorspent`, `CheckTxInputs` in
`src/consensus/tx_verify.cpp`, called per tx from `ConnectBlock`).

Repro on regtest: block 102 creates `P` and spends `P:0` in the same
block (`S1`). Block 103 spends `P:0` again (`S2`). IBD loads 102 and 103
as separate load batches and the write stage merges them into one write.
That write rejects 103 (`PrevoutSpent`) and IBD retries one block at a
time. 102 connected, and 103 alone then connected too. With 103 on the
chain, `S2` is strong and `P:0` reads spent by `S2`. Once 103 is
disconnected, `P:0` reads unspent although 102 spent it, and another
block that re-spends it connects.

A same-block spend has no annotate slot. Its only record is the Class A
spent slot that the write fills from the plan's spend edges
(`ArchiveWritePlan::same_batch_spent_overlay`). The merged plan had two
edges for `P:0`. The edges map iterates in hash order and the slot
encoder keeps the last entry for a vout, so the slot usually named `S2`.
The rejected write keeps its Class A, and the retry does not append it
again, and the retry of 102 does not annotate `S1`: a same-block spend
has no create fk, so structural queues no slot for it. `S2` was not
strong, so `P:0` read as unspent.

**Fix:** the overlay keeps one entry per vout, the lowest spender fk.
Plans append in height order, so that is the first spender in chain
order. The merged write still rejects; the retry then reads `S1` strong
and rejects 103 alone as `PrevoutSpent`.

**Existing stores:** a store that already connected such a block keeps
the wrong slot and needs a re-sync. On mainnet this needs a peer to
serve a block with valid proof of work that double-spends, on the best
header chain, during IBD.

**Regression:** consensus matrix row C38;
`rbitcoin-query` `archive::spent_overlay_names_the_first_spender_of_a_vout`;
`rbitcoin-test` `consensus_rules::respend_of_a_same_block_spend_rejects_on_single_block_retry`;
`rbitcoin-ibd` `ibd::events::confirm_reject_tests::merged_respend_of_a_same_block_spend_is_rejected_alone`
