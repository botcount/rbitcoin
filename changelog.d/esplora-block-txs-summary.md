Added

- **Esplora `GET /block/:hash/txs/summary`.** A dialect route (not in
  Blockstream's API) that returns one `[txid, fee, weight, value]` array per
  transaction in block order, coinbase first with fee `0`: fee and output
  value in sats, BIP141 weight. It reads stamped `txstat` and `txout` values,
  so it is a small fraction of `getblock` verbosity 2 for the same block, and
  a stamped block still answers on a `--prune-seqsigwit` node. A malformed,
  unknown, or off-best-chain hash is 404, as on `/block/:hash/txids`.
