Fixed

- Esplora serves archived blocks disconnected during the current process,
  including header, raw block, transaction lists, and stamped summaries.
  Status reports the original height with `in_best_chain: false` and
  `next_best: null`. Stale height lookup prepares the active height index
  once and walks at most 10,000 headers; `asof` stays best-chain-only.
- Header-only, never-connected bodies, and reopened stale blocks return 404.
  Stale JSON summaries require stamped transaction statistics and do not
  reconstruct witnesses. A restart forgets disconnected membership.
