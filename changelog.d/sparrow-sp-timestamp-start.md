Fixed

- **Sparrow Silent Payments wallets restore their history.**
  `blockchain.silentpayments.subscribe` refused a `start` that is a Unix
  time with `timestamp start not supported`, which is what Sparrow sends for
  a wallet birthday. A start above 500,000,000 is now resolved to the last
  block whose header time is at least two hours earlier. Every start was
  also moved up to the last 256 blocks, so an older payment was never found;
  the scan now starts where the client asked. It runs beside the session in
  steps of 256 heights, so requests are answered while it runs, and it stops
  when the client disconnects or unsubscribes. History arrives as it is
  found with `progress` below 1.0. Notification `params` are the positional
  `[subscription, progress, history]` of the Frigate specification; they
  were an object. A block connected during the scan, or one whose tip event
  was missed, is scanned.
- **Silent Payments subscriptions follow more of the Frigate
  specification.** A connection keeps up to 16 addresses
  (`--electrum-max-sp-subs N`, conf `electrum_max_sp_subs=`, NixOS
  `services.rbitcoin.electrum.maxSpSubs`), each with its own history scan
  and tip notes; it kept one, so a second wallet replaced the
  first. A subscribe to an address the connection already has keeps the
  earlier start. A `"FROM-TO"` start scans that closed range instead of
  running to the tip. A reorg during a history scan scans the replaced
  heights again. A start the node cannot read, or one
  that is not a height, a time or a range, is the error of the subscribe
  request. The node runs at most 32 history scans at once and refuses a
  subscribe past that.
