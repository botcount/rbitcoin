# 024 — Talip review index

Coordinated review (2026-09-21 and 2026-09-22). This index is the status
board. Each fixed row names the regression. No reproduction steps.

| Id | Severity | Topic | Status | Regression |
|----|----------|--------|--------|------------|
| C-1 | critical | Tapleaf `0x50` rejected before commitment | fixed | `script_path_accepts_leaf_0x50_with_annex_and_0xc2` ([025](./025-tapleaf-annex-prefix.md)) |
| H-1 | high | False witness program accepted | fixed | `false_witness_program_is_eval_false` ([026](./026-false-witness-program.md)) |
| H-2 / #12 | critical | Script skip keyed by txid | fixed | `tip_script_pres_skips_only_matching_wtxid` ([027](./027-script-skip-wtxid.md)) |
| H-3 | high | Witness padding caches block hash | fixed | `hostile_peer_session` ([034](./034-witness-padding-not-cached.md)) |
| #08 / #01 / #04 | critical | Header error discarded; chain wipe; work wrap | fixed | `hostile_peer_session` ([030](./030-header-accept.md)) |
| H-5 / H-6 | high | Spend annotations after tip commit | fixed | `zeroed_spend_slot_after_tip_seal_rejects_respend` ([046](./046-spend-durability.md)) |
| H-4 | high | Unbounded per-peer send queue | fixed | `hostile_peer_session` ([043](./043-peer-send-buffer.md)) |
| #17A / C02 / L-10 | high | Signet solution bounds | fixed | `compact_size_and_solution_parse_errors` ([031](./031-signet-solution.md)) |
| C01 | critical | Untrusted tx-count allocation | fixed | `decode_block_precomputes_rejects_tx_count_past_payload` ([032](./032-block-tx-count-alloc.md)) |
| C06 / C04 | critical | Compact partial count, one partial, pending-header cap | fixed | `reconstruct_rejects_tx_count_above_weight_ratio`, `hostile_peer_session` ([029](./029-compact-tx-count.md)) |
| C03 / C05 | high | Header explore and body-queue bytes | fixed | `rejected_header_batch_does_not_grow_path_or_explore` ([033](./033-ibd-intake-bounds.md)) |
| C09 / C07 / C08 / C10 | critical | Public Electrum scan, subs, scripthash join | fixed | `sh_history_caps` ([036](./036-electrum-public-surface.md)) |
| M-2 / M-5 / M-6 / L-8 | medium | Mempool policy and orphanage | fixed | `mempool_under_pressure` ([047](./047-orphan-reserve.md)–[050](./050-cluster-once.md)) |
| L-7 | low | RBFR direct conflict set | won't-fix | `mempool_accept_life` ([051](./051-rbfr-direct-set.md)) |
| C12 / #09 / #10 / C13 | high | RPC body, waits, client id | fixed | `unread_body_is_read_before_the_answer_up_to_the_cap` ([037](./037-rpc-esplora-limits.md)) |
| C11 / M-1 | high | Async tip-accept lifetime | fixed | `owned_job_finishes_after_waiter_abort` ([038](./038-tip-accept-lifetime.md)) |
| #11 / L-6 | high | I/O buffer lifetime | fixed | `enter_failure_with_pending_matches_the_hard_cap` ([039](./039-io-lifetimes.md)) |
| C15 / #17B / #18 / L-4 | medium | Corrupt store and mmap | fixed | `read_packed_zero_modulus_or_vertices_is_corrupt` ([040](./040-corrupt-bounds.md)) |
| M-7 | medium | Addr relay fanout | fixed | `hostile_peer_session` ([045](./045-addr-relay.md)) |
| #14 / C17 / #19 | low | Flag parity, token, inv cap | fixed | `peer_command_logs_have_no_raw_newline` ([041](./041-hygiene.md)) |
| C14 / M-4 | medium | Milestone hash and signet default | fixed | `low_work_fork_does_not_skip_even_at_the_milestone_height` ([042](./042-milestone-anchor.md)) |
| L-1 / L-3 / L-12 | low | Local auth and datadir mode | fixed | `tor_control_onion_lifecycle` ([044](./044-local-auth.md)) |
| L-9 | low | BIP30 after the exception window | fixed | `buried_rules_and_a_lying_header_path` ([035](./035-bip30-bip34-ancestry.md)) |
| C16 | low | Regtest BIP34 height | fixed | `s7_regtest_rejects_bip34_missing_at_height_1` ([021](./021-regtest-activation-heights.md)). Regtest `bip34_height` is 1 like Core. Production networks are unaffected. |
| #03 / #13 / #16 | — | Witness reserved value, RBFR, admin RPC | rejected | not defects |

Owner: [`quality.md`](../quality.md) **Q-71**.
