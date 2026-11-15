Fixed

- **Overlapping regtest `generate` calls no longer mine the same transaction twice.** Each call used to copy the pool before taking the generate lock, so a second call could put a transaction the first call had just confirmed into its block and fail with `bad-txns-BIP30`. The pool is read while that lock is held. `generatetoaddress` and `generatetodescriptor` do the same. `generateblock` still uses the transactions the caller named.
