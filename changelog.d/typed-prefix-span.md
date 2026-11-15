Changed

- **Fixed-width reads no longer have a second failure arm.** The txid range, the mempool meta counts, the packed live prefix, the posts-spill key count, and each delta-loc window are a typed width. A short spill, a bad magic, a short packed record, a zero slot cap, or a txid past the count still returns the same error. The decoded values are unchanged.
