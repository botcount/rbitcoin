Changed

- **A header rewind drops queued bodies above the cut in one pass.**
  It used to copy every queued height, then take the queue lock again for
  each height above the cut. The cut and everything below it stay queued.
