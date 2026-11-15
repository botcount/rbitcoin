Fixed

- **An outspend lookup for a `vout` past the transaction's outputs answers
  unspent.** Esplora's `GET /tx/{txid}/outspend/{vout}` and
  `POST /internal/txs/outspends/by-outpoint` returned HTTP 500, and Electrum's
  `blockchain.outpoint.get_status` and `subscribe` returned `corrupt record:
  spent slot OOB`, for any `vout` at or beyond the output count. The spender
  read now bounds `vout` by the output count it already reads and reports no
  spender past it, so these answer `{"spent": false}` as electrs does.
