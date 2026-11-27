# muse-ci ban-rpcs @ a2634774f: FAIL (qc scripts)
2026-11-27 06:28Z, 2 min. Branch muse/ban-rpcs, base master~0.

- ok fetch (0s) 5 commit(s) on master~0, 15 files
- ok fmt (4s)
- ok metadata --locked (0s)
- FAIL qc scripts (17s)
    [ast-grep]     │         ^^^^^^^^^^^^ |     │ |     = Every file flush goes through one helper (crates/rbitcoin-store/src/durability.rs). An async file flushes through it after into_std(). | Error: 1 error(s) found in code. | Help: Scan succeeded and found error level diagnostics in the codebase.
- ok changelog (0s)
- ok clippy -D warnings (17s)
- ok doc (8s)
- ok crap (cc of new/grown fns) (0s)
- ok cargo test --workspace (94s) 5316 passed, 2 failed, 5 ignored (2 known root-only chmod failures ignored)

Rounds: fa10a4a fail, fa10a4a fail, 2ab7370 pass, a263477 fail
Full log: log.txt.gz on this branch (gzip, tail of each step).
