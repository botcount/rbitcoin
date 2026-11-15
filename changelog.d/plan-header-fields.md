Changed

- **A parent header's time and bits are read without copying that block's transaction ids.**
  Confirm still caches those ids for the blocks above the tip. The median-time
  and bits reads share that cache instead of cloning the list.
