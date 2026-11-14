Fixed

- **A standard P2SH multisig spend is no longer refused for "legacy sighash
  work".** The unconfirmed legacy sighash cap charged every CHECKSIG the whole
  transaction, so a 2-of-3 spend with 22 inputs was rejected on RPC, Electrum,
  Esplora and P2P relay while Core accepts it. A repeated hash type and
  scriptCode of one input is now hashed and charged once, and each input has
  one transaction-sized hash free. The bytes a transaction may hash stay
  bounded at that free hash per input plus 400,000, and block connect is
  unchanged.
- **Esplora answers 404 for a transaction that is neither confirmed nor in the
  mempool.** After a reorg that dropped a transaction (a conflicting
  transaction took its input), `/tx/:txid`, `/hex`, `/raw`, `/status`,
  `/outspend`, `/outspends` and the bulk outspends and `/internal/txs` routes
  still served it as `confirmed: false`, the answer for a mempool
  transaction. They now answer 404 unless the mempool holds it, as electrs and
  mempool.space do. `?asof=` is unchanged.
