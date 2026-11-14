Fixed

- **A full disk fails the store grow, not a later page write.** When
  `fallocate` returned ENOSPC or EDQUOT, the file grow fell back to a sparse
  `set_len` and reported success, so the out-of-space error surfaced on a
  later page write, possibly in the middle of a scripthash rewrite that had
  already released its old pages. The grow now returns the error at the
  allocation, before any caller frees or overwrites anything.
