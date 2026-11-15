Fixed

- **Esplora `POST /txs/test` rounds vsize up.** The answer divided the
  weight by four and dropped the remainder, one vbyte short of
  `testmempoolaccept` when the weight is not a multiple of four.
