Changed

- **The IBD status tick counts ready bodies without copying each queue row.**
  Every five seconds it allocated one metadata record per queued body and
  dropped those records after the count. The count now walks the queue
  index under its lock. A restart that still has queued bodies reads the
  rows.
