Fixed

- **Nightly mutants gets past its workspace baseline.** Test datadirs that two tests in one process could share — a clock sample, or two counters that both start at 0 on `rbitcoin-var-pub` — collided, and one could delete the other's store. Each call now takes a path from one counter.
- **A failed mutants baseline names the test.** The job log prints the failure lines and the last 80 lines of the baseline log, and still exits with cargo's status when that log has no failure line. The full log stays in the artifact.
