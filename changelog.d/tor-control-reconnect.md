Fixed

- **Onion services come back after tor restarts.** The node added its onions once at start. When tor restarted, the onions were gone, the node kept announcing a dead P2P onion until it was restarted, and a later Electrum or Esplora onion stopped the node because the control connection was closed. The node now notices the closed control connection, stops announcing its onions, reconnects with Core's backoff, and re-adds every onion with its saved key. An onion asked for while tor is away is added on reconnect instead of stopping the node.
