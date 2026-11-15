Changed

- **The body queue remembers how many rows are promoted.**
  A sizes snapshot and an in-flight prune used to walk every queued row
  to count them. The count now changes when a row is promoted or removed.
