# 053 — Parent-request tracker cap

**Severity:** critical
**Status:** fixed
**Found by:** Stephan Livera, 2026-10-02 (C1)

The parent-request tracker grew with every in-flight parent on every peer. It now stops at a per-peer cap, and a peer's rows leave with its session. Taking a due key on one peer does not drop another peer's keys. A process-wide cap was removed later: while a few peers held the table full, other peers' announcements were skipped.

**Regression:** `rbitcoin-net` `parent_req_stops_at_per_peer_cap`, `rows_are_bounded_per_live_peer`, `second_peer_take_due_does_not_drop_other_peers_keys`.
