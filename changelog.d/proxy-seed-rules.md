Fixed

- **`--proxy` no longer asks seeds it should not.** With `--connect` or on a custom signet, the node asked the default DNS seeds through the proxy; Core asks none there. On regtest, which has no seed in this node, it waited 3 minutes for an answer that could not come, with RPC off. The proxy path now uses the same seed rules as a direct start.
