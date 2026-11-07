Fixed

- **`--sp-tweaks` can run with `--prune-seqsigwit` when it is enabled before taproot.** Confirm seals each tweak from the wire block. A restart whose watermark is still above the prune height repairs from `seqsigwit.window`. Enabling the index after the tip has reached taproot is refused before prune cuts the witness stem, unless the index already covers every dropped height. Turning prune on while tweaks are already enabled stays refused, including a restart that has not loaded the flag yet. Unpruned nodes still backfill a later enable from `seqsigwit.body`.
