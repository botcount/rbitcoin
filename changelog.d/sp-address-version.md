Fixed

- **`blockchain.silentpayments.subscribe` returns a valid address.** The
  `address` in the result left out the BIP-352 version character, so it
  was one character short and did not decode. It now starts `sp1q` on
  mainnet and `tsp1q` on every test network. Regtest used `sprt`, which
  is not a BIP-352 prefix.
