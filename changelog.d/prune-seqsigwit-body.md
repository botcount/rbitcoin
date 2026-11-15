Changed

- **`--prune-seqsigwit` keeps scriptSig and witness for the last 288 blocks.**
  Turning the flag on removes the rest of `seqsigwit.body`, and later blocks
  do not grow that file. That cut logs the size it freed. Opening a datadir that 0.9.0 already pruned rewrites
  those 288 blocks into the new window and then frees the old body. Opening
  any datadir, pruned or not, updates the store to schema 28. A 0.9.0 binary
  will not open it.
