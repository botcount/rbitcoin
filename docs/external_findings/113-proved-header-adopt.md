# 113 — A proved header walk rewrote `header.adopt` on every short batch

**Severity:** medium
**Status:** fixed
**Found by:** @MrHodl, 2026-11-10

After the header walk proved minimum chain work, every connecting reply
rewrote `header.adopt`, including one-header batches. The file holds every
checkpoint, so those rewrites grew with the walk. An unproved walk already
left the file alone.

A proved short batch still advances the in-memory tip and still records a
checkpoint, so a lighter fork remains visible. The file is written when the
walk first proves the floor and when the tip is 2,000 headers past the last
saved checkpoint. A competing chain does not rewrite the file until it is
promoted, and that write's tip is the promoted chain. An unproved walk
still does not rewrite the file.

**Regression:** `rbitcoin-ibd` `ibd::header_walk::proved_short_batches_do_not_rewrite_header_adopt`, `ibd::header_walk::promoting_a_challenger_persists_that_chains_tip`.
