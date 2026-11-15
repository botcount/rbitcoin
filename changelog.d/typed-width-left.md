Changed

- **Fixed-width reads no longer have a second failure arm.** Slot rows, spend-commit words, spill headers, head records, phase-done counts, an occupancy sidecar, a page entry, a ping nonce, a signet magic prefix, a mempool.dat word, and a header-adopt word are a typed width. A short file, a bad magic, a short commit, a short spill row, a short packed record, or a nonce mismatch still returns the same error. The decoded values are unchanged.
