# 108 — Bobberb store-integrity index

Review by bobberb and muse, 2026-10-18. This index is the status board
for the store file findings. Each fixed row names the regression. No
reproduction steps.

| Id | Severity | Topic | Status | Regression |
|----|----------|--------|--------|------------|
| G1 | high | fuse8 body length wraps the size check | fixed | `fuse8_wrapped_body_length_is_corrupt` ([102](./102-fuse8-body-length.md)) |
| G2 | high | fuse8 segment count need not divide the array | fixed | `fuse8_segment_count_must_be_a_multiple_of_segment_length` ([103](./103-fuse8-segment-multiple.md)) |
| G3 | high | A loc stride allocates before the published-end check | fixed | `body_read_above_the_cap_is_refused`, `index_read_above_the_cap_is_refused` ([104](./104-body-read-cap.md)) |
| G4 | high | Scripthash page walk has no cycle stop | fixed | `scripthash_page_cycle_is_corrupt`, `scripthash_page_free_cycle_is_corrupt`, `scripthash_page_free_revisit_is_corrupt` ([105](./105-scripthash-page-cycle.md)) |
| G5 | medium | No checksum on historical bodies | accepted | Documented. Bit-rot outside the tip window is a reindex ([106](./106-historical-bitrot.md)) |
| G6 | medium | Torn `spent.body` page below `spend_durable` | fixed | The page log serves the slot while `spend.pages.commit` exists ([107](./107-torn-spend-page.md), `historical_page_stays_unwritten_until_spend_checkpoint`) |
