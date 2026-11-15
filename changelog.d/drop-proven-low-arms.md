Changed

- **Fixed-width reads no longer have a second failure arm.** Fuse geometry, multi-rel lists, txid prefixes, v17 version words, and key-spill counts are a typed prefix. A short buffer still returns the same corrupt error. The decoded values are unchanged.
