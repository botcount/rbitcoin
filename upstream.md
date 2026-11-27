# Upstream PR #2167 @ 904febfc2
Checks: 24/24 done, 0 failed


## New review comments (address each; reply only through code/PR text, not as comments unless told)
- [coderabbitai[bot] CHANGES_REQUESTED] **Actionable comments posted: 1**

---

- [ ]  Ask CodeRabbit to fix comments on this review

**Included review availability:** This review used your included allowance. Your plan provides up to 10 included reviews per hour; 9 remain after this review.
- [coderabbitai[bot] crates/rbitcoin-ibd/src/ibd/dial.rs:641] ** Major** · **Stability & Availability** · **Heavy lift**

**Release a banned slot's block requests before removing the slot.**

`drop_banned_slots` removes banned slots with `retain` and does not release their block requests. Those requests stay in `st.inflight` under the removed peer id. The disconnect path does not recover them, because `release_peer_block_work` returns no hashes when the slot is gone. If these entries fill the getdata window, `assign_densify` computes zero room and returns. Healthy peers then receive no new block requests, and catch-up can stop.

Before the sweep removes a banned slot, call `release_peer_block_work` for that peer. Pass the freed hashes to `reopen_for_densify`. Requests that another peer still holds stay in `st.inflight`, because `release_block_requests` removes only the released peer from each entry. Apply the same release before every call site of 
