Fixed

- **The Electrum accept-retry test now runs on macOS and checks the retry.** It counted descriptors through `/proc/self/fd`, which macOS lacks, so it set the limit below the open count and failed with "Too many open files". On every platform, reaping the helper process freed two descriptors before `accept` ran, so `accept` never saw EMFILE and the test passed with the retry removed.
- **The P2P descriptor-reserve test runs on macOS.** It bound each client to a different `127.0.0.x` address to stay under the per-address cap on unfinished handshakes, and macOS configures only `127.0.0.1`. The test now leaves room for fewer connections than that cap, so every client can come from `127.0.0.1`. It fails when the listener skips the reserve check.
