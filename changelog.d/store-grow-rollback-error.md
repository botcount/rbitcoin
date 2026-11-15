Fixed

- **A failed store grow no longer hides a failed undo truncate.** When
  `fallocate` fails and the grow rolls the file length back, that truncate is
  retried if a signal interrupts it, and if it still fails its error is
  returned instead of the allocation error (the file may then stay longer than
  the recorded capacity). Follow-up to the full-disk grow fix.
