Fixed

- **`full_lane_parks_instead_of_spinning` passes in the full `rbitcoin-net`
  suite.** The parked-sender signal used `notify_one`, which kept a permit
  when a sender in another test parked with no listener. The test's listener
  then fired before its own sender tried, and it failed with `saw 0`. The
  signal now uses `notify_waiters`, which keeps no permit (#1470).
