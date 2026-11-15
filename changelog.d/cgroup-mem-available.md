Fixed

- **Worker pools respect the cgroup memory limit.** The scripthash extract
  and `tx.head` rebuild sized their workers from host `MemAvailable`, so a
  node under systemd `MemoryMax=`, `docker --memory`, or a Kubernetes limit
  could start more workers than the limit allowed and be OOM-killed during
  the first scripthash build. On Linux the probe now takes the smaller of
  `MemAvailable` and the cgroup v2 headroom. Reclaimable page cache counts as
  headroom, so a cgroup full of file cache after IBD still gets its workers.
