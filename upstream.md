# Upstream PR #2167 @ 0bb3a0dde
Checks: 24/24 done, 1 failed

## FAILED macos-test
    (log unavailable: HTTP Error 401: Server failed to authenticate the request. Please refer to the information in the www-authenticate header.)

## New review comments (address each; reply only through code/PR text, not as comments unless told)
- [coderabbitai[bot] CHANGES_REQUESTED] **Actionable comments posted: 1**

---

- [ ]  Ask CodeRabbit to fix comments on this review

**Included review availability:** This review used your included allowance. Your plan provides up to 10 included reviews per hour; 9 remain after this review.
- [coderabbitai[bot] crates/rbitcoin-ibd/tests/ibd/dial.rs:2368] ** Minor** · **Functional Correctness** · **Quick win**

**Assert that the ban sweep rewinds the densify cursors.**

The test passes when the `reopen_for_densify` call is removed from `drop_banned_slots`. The test never records a height for `sole`, and both densify cursors start at zero. `reopen_for_densify` only lowers a cursor, so it cannot change either cursor from zero. The assertions therefore cannot observe the call. The test checks the inflight removal and the Missing state, but not the densify reopen that the sweep is meant to perform.

Extend the test to cover this effect. Record a height for `sole`. Move `densify_scan_lo` and `densify_ready_through` above that height. After `drop_banned_slots` runs, assert that both cursors equal that height. This protects request reassignment, not only inflight removal.
