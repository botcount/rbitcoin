Changed

- **Removing a mempool transaction no longer rescans every accept time.**
  The relay index took the minimum over the whole pool after each removal
  and each restamp, so a block that confirmed R pool transactions out of M
  visited R x M rows. It now reads the first key of the expiry order.
