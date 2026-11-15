# 116 — A reconnected block lost the outputs it spent inside itself

**Severity:** critical — consensus split
**Status:** fixed
**Found by:** Nitesh Balusu, 2026-11-14

Core rejects a block that spends an output a connected block already spent
(`bad-txns-inputs-missingorspent`).

After block X leaves the active chain and the same X connects again, every
output that X both created and spent read unspent, and a later block that
spent one again was accepted. It needs a spend checkpoint while X is active
and another after the disconnect, before X returns. A clean shutdown, open
replay after a crash, or the 10-minute `rbtc-spend-sync` tick each supply
one. Dropping either checkpoint keeps the spend.

A same-block spend has a null create fk, so structural does not annotate
it. The first connect writes that slot only in the Class A pre-fill, and
only when the body is appended. A checkpoint copies the whole 4 KiB page.
The checkpoint after the disconnect writes eight zero bytes over a slot
whose spender is no longer strong, which is required so a rejected spend
is not folded back onto `spent.body`. Readers trust the page log, and a
null field is unspent. The reconnect does not append the body, so Class A
does not pre-fill the slot again.

First bad commit is `6e0082737` ("store: keep a racing spend out of the
lower checkpoint"), inside the spend page log (PR 1501, merge
`89610561d`). The parent `ee216c05a` is clean. No tag contains
`6e0082737`. v0.9.2 is unaffected.

**Fix:** a connect whose body this wave did not append annotates those
same-block spends from the archived parent fk, the same way open replay
does. The zero in the checkpoint stays.

**Existing stores:** a node that already connected such a block again
keeps the wrong spends until `store/spend_durable` is deleted and the
node is restarted. Open then revalidates the whole chain and replays
every height's spends. A later block does not heal the slot.

**Regression:** `rbitcoin-query`
`reconnected_block_keeps_its_in_block_spend_across_checkpoints`,
`reconnected_block_in_block_spend_without_checkpoints`,
`reconnected_block_in_block_spend_checkpoint_while_active`,
`reconnected_block_in_block_spend_checkpoint_after_disconnect`.
Consensus matrix row C38.
