# 089 — IBD halted on a block that spends an unknown txid

**Severity:** high — liveness (IBD halt), no verdict on an invalid block
**Status:** fixed
**Found by:** consensus audit, 2026-10-06 (follow-up to 087)

A block on the most-work header chain, with a valid header and a body
that matches its merkle root, spends a txid that no connected block
created. Core rejects it with `bad-txns-inputs-missingorspent`, marks it
failed, and follows another valid chain.

In IBD the load stamp binds parents from the pipeline's own view: the
in-flight map of batches not yet written and the load-batch skeleton
that lookup read ahead of the tip. A miss there is
`Corrupt("parent create_fk unresolved")`, which maps to
`MissingPrevout`. A batched miss is isolated, and the block is stamped
alone. The lone miss was then an engine fault (requeue once, then halt)
because that view has missed a real parent before: an in-flight prune
raced the `tx.head` drain and blacklisted a valid mainnet block. So the
hash was never marked invalid, and IBD halted with "engine fault
repeated" with no way to move to another chain.

Two more defects sat on that path. The load thread sent the lone miss as
`ConsensusInvalid` and dropped its body, while apply read the reject
line as an engine fault and expected a requeue. Nothing asked for the
body again, and IBD stalled with no halt. And after a restart the
`tx.head` drain watermark started at 0, so any check that waits on it
could not pass until the first write.

**The body is not in doubt.** Load runs
`validate_block_structure_with_pres` (merkle root, CVE-2012-2459
duplicate tail, 64-byte tx layout, witness commitment) before the stamp
binds a parent. Txids, and so prevouts, are committed by the header. A
peer cannot make an honest block miss a parent. The open question is
whether this node's read of the chain is complete.

**Fix:**

- A lone stamp miss goes back to the body queue as an engine fault, and
  apply no longer overrides the sender's class from the reject text. A
  batched miss is isolated as before.
- When a lone block's stamp misses a parent, load waits up to 10 s for
  the block's parent to be the tip and for `tx.head` to hold every
  connected create (`Query::head_covers_fence`: the drain has passed the
  highest fence fk). It then reads the block's parents with one fresh
  TipOnly read, the read the tip path trusts. Txids the block creates
  are left out. If a parent is still missing, the tip did not move, and
  no disconnect ran from the wait through the read, the block is
  `ConsensusInvalid`. If every parent is there, it stays an engine
  fault. If the parent does not become the tip, it is a cascade retry.
  The check runs under its own load timer, `reject=`, not `prune=`, and
  stops when confirm stops; a stopped check is a retry, not a verdict.
- Load samples the tip once more for a lone reject, and both its rewind
  and the reject event use that sample: a lone `ConsensusInvalid` event
  means load dropped the body, as scripts and write already do. Apply
  checks again that the tip is the block's parent before it marks the
  hash invalid. If the tip moved in between (an IBD rewind), apply
  treats the reject as a cascade and asks for the body again. Before,
  that body was gone from the queue and stayed pending, so it was not
  asked for until the pending mark went stale.
- `tx.head` insert syncs `meta` but not the open tail's slot pages. After
  a power loss the tail can miss a connected create that `meta` still
  counts, and open does not see it. A roll syncs the pages it closes,
  holes included, and probes read them until the background seal
  publishes. So a lost row can sit in any unsealed segment: the open
  tail or the in-flight seal. Before a verdict, the read scans
  `txid.body` from the first fk of the oldest unsealed segment, taken
  before the head read, to the last create. A missing txid found there
  at a connected fk is `Corrupt("invariant: tx.head misses a connected
  create")`, an engine fault. The range is complete: a sealed segment
  is rebuilt from `txid.body` and its files are synced before `meta`
  names it sealed, open seals a leftover non-tail from `txid.body`, the
  first unsealed fk never falls, and open revalidates `txid.body` above
  the last device flush. The scan reads up to two segments of txids
  (about 1.7 GiB on mainnet, 2 MiB at a time), only on this reject path.
- Open starts the drain watermark at the head's last create. Open
  already rebuilds or backfills `tx.head` through every Class A create.
- A write that rejects after its Class A append has still inserted the
  batch's creates into `tx.head`, and moves the drain watermark like a
  write that connects. Before, only a connecting write moved it. An
  archived retry of the same blocks queues no insert, so after a valid
  block connected on its retry the drain stayed behind the fence until a
  fresh write. A later block of that rejected write whose parent was
  missing then waited 10 s twice, was an engine fault both times, and
  halted IBD (found 2026-11-10).

**Ways a read can miss a parent that exists, and why each is safe:**

| Case | Result |
|------|--------|
| Parent in an earlier batch still in scripts or write | Wait for the parent to be the tip and the drain to cover the fence. The read hits: engine fault, retry |
| In-flight prune or leftover race in the stamp | The read is fresh and does not use in-flight or the skeleton |
| Parent only in a disconnected or rejected block (015, 017) | TipOnly skips unconnected rows. Core has no coin either: verdict is correct |
| Parent later in the same block | Left out of the read; 005 rejects the order at write |
| IBD rewind during the wait or read | Disconnect generation and tip are checked after the read: cascade |
| IBD rewind after load drops the body, before apply | Apply's tip check fails: cascade, and the body is asked for again |
| Drain behind the tip, or a failed drain re-queued | `head_covers_fence` is false: wait, then engine fault |
| Write rejected after its append, valid block retried alone from the archive | The rejected write's insert moved the drain past its creates: the read runs |
| Restart | Open backfills `tx.head`; the drain starts at its last create |
| Power loss drops slot pages of the open tail, which then rolls and seals | `txid.body` scan of every unsealed segment finds the connected create: engine fault |
| IBD stops during the scan | The scan ends as `Cancelled`: retry, not a verdict |
| `--prune-seqsigwit`, milestone | Prune drops seqsigwit only; milestone skips scripts only. `tx.head` and `txid.body` are complete |
| Mutated body | Rejected as mutated before the stamp; never reaches the read |

The tip path trusts a TipOnly miss the same way and does not scan the
open tail. A power loss there can still mark a valid tip block invalid
until restart. That is not changed here.

**Regression:** `rbitcoin-net`
`ibd::events::confirm_reject_tests::ibd_spend_of_an_unknown_txid_is_invalid`,
`spend_of_an_unknown_txid_after_restart_is_invalid`,
`spend_of_a_reorged_out_tx_is_invalid`,
`lone_spend_of_the_block_just_written_connects`,
`spend_of_a_create_the_head_lost_is_not_invalid`,
`spend_of_a_create_the_head_lost_before_a_roll_is_not_invalid`,
`verdict_applied_after_the_tip_moved_asks_for_the_body_again`,
`batched_spend_of_a_later_block_is_rejected_alone`,
`forward_spend_behind_a_valid_block_in_one_write_is_invalid`,
`forward_spend_after_a_valid_block_connected_alone_is_invalid`;
`rbitcoin-test`
`consensus_rules::archived_retry_after_a_rejected_write_keeps_head_covering_the_fence`
