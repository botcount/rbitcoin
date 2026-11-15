# 112 — Stall treated unsolicited socket bytes as block progress

**Severity:** medium
**Status:** fixed
**Found by:** @MrHodl, 2026-11-10

An IBD peer with block getdata on the wire could avoid a withholding
disconnect by sending pings or headers. The check counted every socket
byte since the last block-progress stamp. A peer that sent nothing stayed
the quiet case, but chat looked like a body still arriving. Relative-slow
zero-throughput sampling used the same socket total.

Withholding and that zero-throughput sample now use solicited block bytes
since the stamp, plus the block frame still on the socket when that frame
is longer than a headers message. A ping-only peer is withholding.
Finished or in-flight block bytes of one 64 KiB step keep the peer. Quiet
detection is unchanged.

**Regression:** `rbitcoin-ibd` `ibd::dial::disconnect_stalled_drops_a_peer_sending_only_unsolicited_bytes`, `ibd::dial::disconnect_stalled_keeps_a_peer_still_sending_a_block`, `ibd::dial::relative_slow_zero_throughput_uses_solicited_bytes`, `ibd::peer_io::block_progress_stamp_records_solicited_bytes`.
