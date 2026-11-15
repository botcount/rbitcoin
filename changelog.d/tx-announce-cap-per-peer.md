Security

- **One group of peers can no longer stop the node from requesting other
  peers' transactions.** The transaction-request tracker had a
  process-wide cap of 200,000 announcements. While it was full, a new
  announcement from any peer was skipped and never requested, so peers
  that announced many transactions and never served them could keep the
  node from fetching honest peers' transactions. The process-wide cap is
  removed. Each peer keeps its own cap of 5,000, as Core
  `MAX_PEER_TX_ANNOUNCEMENTS`, and a session's rows are dropped when it
  disconnects, so the table is bounded by that cap times the live
  sessions. A row one peer announced now takes about 310 bytes instead
  of 415. An orphan's other announcer that is already disconnecting no
  longer takes a parent row that would never be freed. Every peer can
  now wait on the same transactions, so the tracker removes an index
  entry without a scan, and a heartbeat passes over a transaction
  another peer is being asked for without reading its rows.
