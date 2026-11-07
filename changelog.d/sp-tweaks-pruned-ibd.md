Fixed

- **`--sp-tweaks` can run with `--prune-seqsigwit` when it is enabled before taproot.** Confirm seals each tweak from the wire block. A restart whose watermark is still above the prune height repairs from `seqsigwit.window`. Enabling the index after that height has been dropped is refused. Turning prune on while tweaks are already enabled stays refused. Unpruned nodes still backfill a later enable from `seqsigwit.body`.
