Fixed

- **A standard P2SH multisig spend is no longer refused for "legacy sighash
  work".** The unconfirmed legacy sighash cap charged every CHECKSIG the whole
  transaction against a flat 400,000 bytes, so a 2-of-3 spend with 22 inputs
  was rejected on RPC, Electrum, Esplora and P2P relay while Core accepts it.
  Each base hash is now charged the bytes its preimage serializes (the other
  scriptSigs are blank in it), a P2PKH input's hash included, against one
  fixed 32,000,000 byte total per transaction. An input that hashes nothing
  adds nothing to it. A repeated hash type and scriptCode of one input is
  hashed and charged once. A standard spend of any input count and any mix of
  hash types fits (about 23 MB is the most a 100 kvB spend of P2PKH or P2SH
  multisig inputs serializes), and a transaction that asks for more is
  refused. Block connect is unchanged.
- **Esplora answers 404 for a transaction that is neither confirmed nor in the
  mempool.** After a reorg that dropped a transaction (a conflicting
  transaction took its input), `/tx/:txid`, `/hex`, `/raw`, `/status`,
  `/outspend` and `/outspends` still served it as `confirmed: false`, the
  answer for a mempool transaction. They now answer 404 unless the mempool
  holds it, as electrs and mempool.space do. The bulk routes
  (`/internal/txs`, `/internal/txs/outspends/by-txid`, `GET /txs/outspends`)
  stay 200 and leave the row out or return an empty array for it. When the
  mempool holds the txid, the body is the mempool's copy, not the stored row,
  whose witness can differ. While a connect, disconnect or reorg holds the
  pool, such a row answers 503 `chain view moved` instead, because the
  disconnected blocks' transactions are not back in the mempool until the
  reorg ends. `?asof=` is unchanged.
