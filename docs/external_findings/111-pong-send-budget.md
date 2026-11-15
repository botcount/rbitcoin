# 111 — Pongs were uncharged, and IBD pings were not rate-limited

**Severity:** medium
**Status:** fixed
**Found by:** @MrHodl, 2026-11-10

Every tip-follow ping queued a pong that the send budget did not count.
The writer then subtracted that size anyway, so the counter could fall
below the bytes still queued. IBD answered every ping with no message
cap and no send budget.

Tip-follow still queues one charged pong. Later pings replace the stored
nonce, including nonce 0, and do not queue another message. The write
subtracts that one charge. The writer sends the nonce on that queued
pong. A newer nonce stored before the write is a second pong, queued
after that write, behind replies already waiting. IBD still keeps one
pong carrying the latest nonce. IBD pings use the existing per-peer
message window. A requested block still skips it.

**Regression:** `rbitcoin-net` `peer::one_charged_pong_keeps_the_queued_nonce`, `peer::writer_sends_the_queued_pong_then_a_later_nonce_behind_the_replies`, `rbitcoin-ibd` `ibd::peer_io::ibd_pings_coalesce_and_overflow_the_message_window`.
