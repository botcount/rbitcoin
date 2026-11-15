Changed

- **A block presence check no longer copies the cached block.**
  `ChainHub::has_block` and the stale-tip `getheaders` answer cloned the
  whole body from the tip cache to test that it was there. They now ask the
  cache for the key.
