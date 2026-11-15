Fixed

- **No template past the two-hour future limit.** When six of the last
  eleven blocks sit at the node's future-time limit, the next block's
  earliest time is past it. `getblocktemplate` and the `generate*` RPCs now
  fail as Core does (`-1`, `TestBlockValidity failed: time-too-new, block
  timestamp too far in the future`) instead of handing out or mining a block
  the node would reject, and the Stratum V2 provider holds templates without
  dropping the session. One second later all of them work again
  (bitcoin/bitcoin#36192).
