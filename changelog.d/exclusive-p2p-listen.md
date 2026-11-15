Changed

- **A held P2P port fails startup with Core's bind error.** When another process already holds a `--listen` port, the node exits with `Error: Unable to bind to <addr> on this computer. rbitcoin is probably already running.` instead of a `configuration error` log line (bitcoin/bitcoin#36305). Core's `feature_port.py` runs again.
