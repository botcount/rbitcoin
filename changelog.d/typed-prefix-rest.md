Changed

- **Fixed-width reads no longer have a second failure arm.** Spender records, multi-list rows, BDZ headers, and the spend-durable marker are a typed prefix. A short buffer still returns the same corrupt error. The decoded values are unchanged.
