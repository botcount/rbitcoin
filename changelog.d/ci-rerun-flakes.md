Fixed

- **A few tests no longer fail when CI is slow or lookup runs ahead.**
  A one-block confirm reject counts when the batch cascade was not emitted.
  The script-stage wake test keeps a wake that arrives before the stage
  parks. The redial, isolated-broadcast, and outbound-refill tests wait
  until the task they started has actually finished.
