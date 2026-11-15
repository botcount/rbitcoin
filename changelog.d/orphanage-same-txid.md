Fixed

- **The orphanage keeps two witnesses of one txid.** It is keyed by wtxid, as
  in Core, so a second witness of a parked child no longer replaces the first.
  A package or a local submit leaves the orphanage alone and validates a
  parked tx like any other, as Core's ATMP does. A tx that is only parked no
  longer counts as one the node has, and an inv of a pooled tx adds no orphan
  announcer. A peer's parent resent after its txid is in the mempool erases
  that peer's orphans whose txid is pooled too (Core `Find1P1CPackage`).
  `p2p_orphan_handling.py` now runs.
- **A reparked orphan is no longer evicted first.** Removing an orphan left its
  wtxid in the eviction order. Parked again, the stale entry made it the
  oldest, so the next eviction dropped it instead of the oldest orphan.
