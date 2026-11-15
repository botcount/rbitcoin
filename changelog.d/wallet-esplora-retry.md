Fixed

- **The test wallet retries one slow Esplora read.**
  `sendtoaddress` used to fail the RPC on a single timed-out
  `GET /address/:addr/utxo`. A read that never answers now
  stops in about 15 seconds, inside the harness client timeout.
  `gettransaction` uses that same bound for `/tx/:txid/status`.
- **The test harness binds Esplora above the ephemeral port range.**
  `rpcport+20000` sat around 36000-41000, so a busy runner could
  already own that port and the node failed to start.
