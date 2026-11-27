# Upstream PR #2167 @ 4dcc35b67
Checks: 24/24 done, 0 failed


## New review comments (address each; reply only through code/PR text, not as comments unless told)
- [coderabbitai[bot] CHANGES_REQUESTED] **Actionable comments posted: 4**

---

- [ ]  Ask CodeRabbit to fix comments on this review

**Included review availability:** This review used your included allowance. Your plan provides up to 10 included reviews per hour; 9 remain after this review.
- [coderabbitai[bot] crates/rbitcoin-net/src/ban.rs:173] ** Minor** · **Functional Correctness** · **Quick win**

**Normalize IPv4-mapped IPv6 targets before storing them.**

`parse_ban_target("::ffff:192.0.2.1")` produces a `V6` target. However, `matches_ip` converts that same address to `V4`, so the ban never matches. Inbound admission also canonicalizes mapped addresses to IPv4.

Normalize mapped targets and their prefix consistently, or reject unsupported mapped subnet forms. Keep native IPv6 targets unchanged.
- [coderabbitai[bot] crates/rbitcoin-net/src/ban.rs:305] ** Major** · **Data Integrity & Integration** · **Heavy lift**

**Return persistence failures to ban-management callers.**

If writing `banlist.json` fails, `add`, `remove`, and `clear` still report success through the RPC handlers. A restart then restores the previous disk state.

A failed `clear` also cannot be retried: memory is already empty, so the next `clear` skips saving even after storage recovers. Preserve a retryable dirty state or roll back the mutation, and propagate a typed persistence error to the RPC caller.
- [coderabbitai[bot] crates/rbitcoin-net/src/service.rs:754] ** Minor** · **Functional Correctness** · **Quick win**

**Close the race between an outbound dial and a manual ban.**

A ban added while an outbound dial is connecting does not stop that dial. The check at `service.rs:752` runs before the connect. `ban_add` disconnects only rows already in `live_peers()`, and no row exists until `register_connecting_net` runs after the connect. The handshake does not consult bans, and `register_handshaken_net` publishes the session without a ban check. The session therefore stays connected after `setban` reports success. The window lasts for the connect timeout. A session published under this race stays until another disconnect, such as `disconnectnode`.

Publication of an outbound session must re-check the ban after the connect completes and before the session becomes visible. A session whose target matches a ban added during the dial must not be publi
- [coderabbitai[bot] crates/rbitcoin-node/src/run.rs:794] ** Major** · **Functional Correctness** · **Quick win**

**Apply manual bans to IBD outbound dials in `spawn_peer`.**

IBD catch-up dials bypass the manual ban list. `spawn_peer` checks only `net_discouraged` and `trust_allows_net`, then calls `connect_net`. A peer banned with `setban` can still receive a new IBD session. This happens during any catch-up dial, including after a restart when the ban file is loaded. The generic connect path in `service.rs` already refuses banned targets. Add the same check before the dial.
