Added

- **`getpeerinfo.last_block_announcement`.** Report the Unix timestamp when a
  peer last first announced a block unknown to the node via `headers` or
  `cmpctblock`, after the accepted header's chainwork exceeds the active tip.
  The field is `0` until then. The Core functional test `p2p_block_times.py`
  is enabled.
