# Upstream PR #2167 @ 2285527eb
Checks: 24/24 done, 0 failed


## New review comments (address each; reply only through code/PR text, not as comments unless told)
- [coderabbitai[bot] CHANGES_REQUESTED] **Actionable comments posted: 2**

---

- [ ]  Ask CodeRabbit to fix comments on this review

**Included review availability:** This review used your included allowance. Your plan provides up to 10 included reviews per hour; 9 remain after this review.
- [coderabbitai[bot] crates/rbitcoin-ibd/src/ibd/peer_io.rs:1265] ** Major** · **Security & Privacy** · ** Detected with Advanced Tier** · **Heavy lift**

**Authorization Bypass**

**Reachability:** External  
**Exploitability:** Moderate  
**CWE:** [CWE-863](https://cwe.mitre.org/data/definitions/863.html) — Incorrect Authorization

[View Security blast radius](https://app.coderabbit.ai/change-stack/rbitcoin-org/rbitcoin/pull/2167/blast-radius?scope=ghh_OusAJGA4HG8ljoOV0wJvVx8NmnZIjc2DdjmxOu0xZWA&toCommitId=2285527eb7e771a326f9643b139497d543cd4bcb&reviewRunId=c6ca5dbe-9280-4165-807a-13494cfa5e1a&securityFinding=b632130ee3108eb4953a223b)

**A manual ban does not disconnect IBD peer sessions that are already connected.**

`ban_add` disconnects only the sessions in `PeerHub.live` that match the new ban. IBD catch-up sessions are not in that table. The only IBD check of the ban list runs once, inside `spawn_peer`, before the connection opens. A peer that 
- [coderabbitai[bot] crates/rbitcoin-net/tests/service.rs:1445] ** Trivial** · **Maintainability & Code Quality** · **Quick win**

**Use the shipped Tiny query fixture.**

This setup duplicates temporary-directory ownership. Cleanup at Line 1498 runs only when the test completes normally. Use `rbitcoin_query::testutil::tiny_query_labeled` and retain its directory guard for the test lifetime.

Based on learnings, “Do not roll your own `std::env::temp_dir()` + `create_dir_all` for a Tiny store/query.”

_Source: Learnings_
