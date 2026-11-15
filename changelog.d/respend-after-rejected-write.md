Fixed

- **IBD no longer connects a block that re-spends a same-block spend after
  a rejected write.** When IBD merged two load batches into one write and
  that write rejected a block that spent an output a lower block in the
  write had already spent in its own block, the one-block retry connected
  the bad block. If that block was later disconnected, the output read as
  unspent (`gettxout`, Electrum, Esplora, mempool admission) and another
  re-spend connected. The archive's spent slot for an output spent inside
  the write now names the first spender in chain order, not whichever map
  entry came last. A store that already connected such a block keeps the
  wrong slot; re-sync it.
