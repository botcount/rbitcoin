Fixed

- **Esplora merkleblock proofs use the stored header.** `GET /tx/:txid/merkleblock-proof` no longer writes a new merkle root onto the header. A txid list that does not match the stored root fails the proof check. A connected block's proof is unchanged.
