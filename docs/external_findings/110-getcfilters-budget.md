# 110 — `getcfilters` kept queueing after the send budget

**Severity:** medium
**Status:** fixed
**Found by:** @MrHodl, 2026-11-10

The send-budget gate ran once, before the handler. `getcfilters` could
then queue every filter in the request, each one past the cap.
`getcfcheckpt` also read one filter-header row per checkpoint height.

`getcfilters` now returns after the filter that crosses the budget.
`getcfcheckpt` reads the checkpoint range once and still returns the
header at every 1,000th height.

**Regression:** `rbitcoin-net` `peer::getcfilters_stops_after_the_filter_that_crosses_the_send_budget`, `peer::getcfcheckpt_returns_a_header_every_thousand_heights`.
