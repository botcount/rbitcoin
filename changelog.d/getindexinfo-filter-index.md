Fixed

- **`getindexinfo` lists the block filter index.** With `--block-filter-index`, it
  now reports `basic block filter index` beside `txindex`, as Core does with
  `-blockfilterindex`: `synced` once the committed filters reach the tip, and
  `best_block_height` the last filtered height. `getindexinfo "basic block filter
  index"` answers that entry alone. Clients that check it before using
  `getblockfilter` or BIP157 peers no longer see the index as missing.
