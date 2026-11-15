Fixed

- **A release cut updates `Cargo.lock`.** `--minor`, `--major`, `--patch`,
  and `--dev-next` rewrite each workspace crate's version in the root lock
  and in the `fuzz` and `script-oracle` locks. A crate that is not a
  workspace member keeps its version, including one that happens to use the
  same version string. A quoted `name version` dependency spec moves with
  that member. `third_party` locks are left alone. The cut fails when the
  lock was not already on the pre-cut version, so a stale lock is not
  committed as the new version. `./scripts/release.sh` refuses the tag
  unless the lock matches. The 0.9.0 cut left the workspace crates at
  `0.8.99`, so `cargo build --locked` failed. `release-gate` rejects a lock
  that still names the old version.
