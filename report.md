# muse-ci ban-rpcs @ fa10a4a98: FAIL (qc scripts)
2026-11-27 06:09Z, 1 min. Branch muse/ban-rpcs, base master~0.

- ok fetch (1s) 1 commit(s) on master~0, 7 files
- ok fmt (4s)
- ok metadata --locked (0s)
- FAIL qc scripts (17s)
    [unused-pub] crates/rbitcoin-net/src/ban.rs:15: `pub const DEFAULT_BAN_SECS` is named nowhere else. Delete it, or drop `pub` if an attribute is its only user (CONTRIBUTING.m
- warn changelog (0s)
    no changelog.d/*.md entry (CONTRIBUTING: one file per pull)
- ok clippy -D warnings (0s)
- ok doc (0s)
- ok crap (cc of new/grown fns) (0s)
- ok cargo test --workspace (68s) 5312 passed, 2 failed, 5 ignored (2 known root-only chmod failures ignored)

Rounds: fa10a4a fail, fa10a4a fail
Full log: log.txt.gz on this branch (gzip, tail of each step).
