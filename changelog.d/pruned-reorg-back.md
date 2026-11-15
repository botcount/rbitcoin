Fixed

- **A pruned node reorgs back onto a block it left.** With
  `--prune-seqsigwit`, a node that had restarted and then reorged away from
  a block kept saying it already had that block. It dropped the body a peer
  sent back and stayed on the lighter branch until the next restart. Core
  keeps the last 288 blocks on a pruned node and completes that reorg. The
  in-memory block cache now records the real height of each block, so a
  disconnect removes exactly the blocks it took off the chain, and a later
  connect no longer leaves the cache stuck on the old branch. Each
  disconnected block also leaves the set of best-chain hashes, not only the
  old tip.
