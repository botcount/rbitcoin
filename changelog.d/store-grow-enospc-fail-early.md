Fixed

- **A full disk fails the store grow on Linux, not a later page write.** When
  Linux `fallocate` returns ENOSPC or EDQUOT, the file grow used to fall back
  to a sparse `set_len` and report success, so the out-of-space error surfaced
  on a later page write, possibly in the middle of a scripthash rewrite that
  had already released its old pages. The grow now returns the error at the
  allocation, before any caller frees or overwrites anything. A failed grow
  leaves the file length and the recorded capacity unchanged, including when
  `fallocate` had already extended the file; the undo truncate is retried when
  a signal interrupts it, and if it still fails that error is returned and the
  file may stay longer than the recorded capacity. When the doubled or headroom size
  does not fit but the size the caller needs does, the grow succeeds at that
  smaller size. A signal-interrupted `fallocate` is retried.

  Known limit: only EOPNOTSUPP, ENOSYS and EINVAL (and platforms without
  `fallocate`: macOS and Windows) still extend the file sparsely. There a full
  disk can still show up on a later page write. A tail left sparse by an
  earlier version also stays writable after `open`.
