Fixed

- **Three `rbitcoin-net` tests no longer fail when CI is busy.** The
  below-floor header test pins the hub clock before it checks the
  announcement stamp, so a second boundary cannot move it. The descriptor
  reserve test takes the process-wide fd limit before it builds its
  runtime. The tip-burst test mines and runs both sessions on one thread,
  so the follower is not flooded with per-block traffic it cannot decode in
  time.
