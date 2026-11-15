Fixed

- **Functional tests see a ping-timeout line as the peer drops.** The node
  appends that line to `debug.log` and flushes it before the log call
  returns. A path that cannot be opened is remembered, so later lines
  do not retry that open. The harness used to copy the same bytes off a pipe, and
  `p2p_ping.py` reads the file with no wait. An early exit also prints
  a timestamped `ERROR` line from that file, so the harness is not left
  with an empty status 1.
- **The fuzz nightly does not apt-install clang when it is already on
  PATH.** A DNS failure for `azure.archive.ubuntu.com` was failing every
  matrix job before a target compiled. A missing clang still installs, and
  retries through `archive.ubuntu.com` when that install fails.
