# Instructions for ban-rpcs, round 2 (exact text: copy from here)

Three earlier chat messages did not reach you (the app showed "Couldn't send."). Everything you need is here.

1. Upstream PR #2167: CI is all green, but CodeRabbit requested changes. Read `upstream.md` on this branch (muse-ci/ban-rpcs) and fix all four points; they are real:
   - normalize IPv4-mapped IPv6 targets (`::ffff:a.b.c.d`, and mapped subnets) to IPv4 on parse, or reject mapped subnets; test it;
   - a failed banlist.json write must not report success: return a typed persistence error to the RPC caller (Core-style error), and leave the in-memory list unchanged (roll back) so a retry works, including `clearbanned`; test with an unwritable path or an injected write failure (no root-only tricks: CI runs as non-root and our CI as root);
   - re-check the ban after an outbound connect completes and before the session is published, so a ban added during the dial cannot leave a live session; test it through the real path;
   - apply manual bans to IBD outbound dials in `spawn_peer` (crates/rbitcoin-node/src/run.rs), the same check as the generic path.
2. Keep the two-commit shape (amend into the matching commit) and reword the second commit subject to exactly:

       rpc: add setban, listbanned, and clearbanned

3. PATCH the PR title to exactly:

       rpc, net: add setban, listbanned, and clearbanned

4. Push to muse/ban-rpcs; our CI runs first and its report.md must be PASS. Then reply in chat with one line per CodeRabbit point (fixed, or why not). Do not post comments on GitHub.
