Fixed

- **A second launch in the same process no longer sees its own datadir lock.**
  Dropping the lock guard calls `LOCK_UN`, so a duplicated descriptor does
  not leave `.lock` busy. `flock` retries when a signal interrupts it, and
  a few times when the lock is busy for a millisecond.
