# 036 — Public Electrum surface

**Severity:** critical
**Status:** fixed
**Found by:** coordinated review, 2026-09-21 and 2026-09-22 (C07, C08, C09, C10)

An unpaged scripthash join above `--max-sh-creates` is refused. The
default is 10000. **0** is unlimited. A history request that names a page
is served even above the cap, and Class A expand stops once that page is
full. A `from_height` of 1 or more scans that window and does not use the
create cap. The clipped span is refused above 8,064 blocks.

Silent-payment subscribe does not write the scan secret to the API log.
The log file is owner-only. A start of 0 is the last 256 blocks. The
historical scan runs on the blocking pool, 256 heights per hold of a
3-permit process semaphore, one scan per connection. It uses the tweak
index (`tweaks_for_height`), not the scripthash index.

Outpoint subscriptions stop at the same per-connection cap as scripthash
subscriptions. Tip restatus looks up those outpoints off the connection task.
A mempool accept looks up only the subscribed outpoints its announce lists,
also off the connection task.

**Regression:** `rbitcoin-query` `tests::sh_history_caps`,
`rbitcoin-test` `electrum_scripthash_sub_cap_unsubscribe_frees_slot`,
`surface_tests::silent_payment_log_drops_the_scan_secret`,
`silent_scan::tests::parse_sub_labels_start_and_networks`,
`rbitcoin-log` `api_log::tests::api_log_file_records_json_line`.
