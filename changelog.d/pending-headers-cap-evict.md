Fixed

- **A full pending-header map still accepts the next header.** A connecting
  batch that the store accepts drops only enough headers to fit. One repeated
  new header takes one slot. A header this call does not write takes none,
  including when the message ends on a header already in the store. Failed
  proof of work goes first, then headers that do not connect, then forks with
  less or equal work. A fork with more work stays. A header the store rejects
  is not inserted, and block asks move only for a header the store kept.
