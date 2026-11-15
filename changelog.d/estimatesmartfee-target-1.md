Fixed

- **`estimatesmartfee 1` and Electrum `blockchain.estimatefee 1` answer as
  target 2, like Core, until mempool flow is warm.** Before flow fullness
  reaches 1, the 1-block quote still blends in the cold 99.9% history: on
  signet, where flow rarely warms, it was ~76x the 2-block rate.
  `estimatesmartfee 1` then reports `"blocks": 2`. At full flow, target 1
  is the warm 1-block quote. Electrum target 0 stays the 1-block quote.
  `estimaterawfee 1` and Esplora `/fee-estimates` key `1` keep the 1-block
  quote, as Core's `estimaterawfee` does (#1437).
