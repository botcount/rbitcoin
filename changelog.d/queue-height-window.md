Changed

- **The body queue looks up a height in a window, and a hash keeps one height until a second one is queued.**
  The queue still holds one body per height. A hole stays empty. Dropping the
  lowest height moves the window up.
