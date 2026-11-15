# 115 — Silent-payment scan dropped history older than 256 blocks

**Severity:** medium
**Status:** fixed
**Found by:** Sparrow against a deployed v0.9.2, 2026-11-13

Follows [065](./065-sp-scan-window.md). That finding had two parts: a
silent-payment scan with a nonzero start walked to the tip in one piece,
and it kept running after the client disconnected. Its fix kept every scan
inside the last 256 heights.

The window bounded the work by dropping the request. Sparrow restores a
wallet from its birthday. The start came back moved up to the tip minus
256, and a payment below that was never scanned. A start sent as a Unix
time, which is what Sparrow sends, was refused with `timestamp start not
supported`.

**Fix:** the scan starts where the client asked. A start above 500,000,000
is a Unix time and resolves to the last block whose header time is at
least two hours before it. What 065 guarded against is bounded another
way:

- A scan holds one of three process-wide permits for at most 256 heights,
  then releases it. Scans waiting for a permit take turns.
- The scan runs beside the session. It stops before its next chunk when
  the connection ends, when the client unsubscribes, or when a new
  subscribe to the same address replaces it. A connection has at most
  16 addresses by default (`--electrum-max-sp-subs`), each with one scan.
- The scan reads one chunk ahead of what the client has taken and no
  further.
- The process runs at most 32 history scans at once, across connections.
  A subscribe past that is refused in its reply until a scan ends.

What is not bounded: a connected client may ask for the whole chain, and
ask again. A subscribe to an address it already holds starts that scan
over, as Frigate requires, and a reorg restarts running scans above its
fork. The node scans for as long as such a client stays connected, in at
most 32 scans, three chunks at a time. Each chunk takes a store read
permit like any other API read and gives it back after 256 heights. That is the
request a restoring wallet makes.

The 065 regression `sp_scan_stops_when_the_client_hangs_up` is replaced by
`sp_scan_stops_when_the_session_drops_it`. The window asserts in
`parse_sub_labels_start_and_networks` now assert the requested start.

**Regression:** `rbitcoin-electrum`
`sp_timestamp_start_finds_a_payment_older_than_one_chunk`,
`timestamp_start_resolves_to_a_height`,
`sp_scan_stops_when_the_session_drops_it`,
`sp_unsubscribe_drops_a_running_scan`,
`sp_subscriptions_are_capped_per_connection`,
`sp_scans_are_capped_across_connections`,
`sp_chunk_end_covers_one_height_and_stops_at_the_tip`.
