# SV2 template provider

Plans A–C have landed. [`quality.md`](./quality.md) keeps Q-64 off the open list.

| Plan | Outcome | Flags |
|------|---------|-------|
| A | `getblocktemplate` and `generate` call `MempoolHub::select_block_template`. | none |
| B | A Job Declarator Client mines a block through this node's template provider. | `--sv2-tp-listen`, `--sv2-tp-authority-sec`, `--sv2-tp-authority-sec-file`, `--sv2-tp-cert-validity`, `--sv2-tp-stale-grace` |
| C | Templates refresh on a fee gain with the tip unchanged. | `--sv2-tp-fee-delta`, `--sv2-tp-template-interval` |

Session caps, deadlines, retained templates, and the testnet minimum-difficulty bits are [`operator/interfaces.md`](./operator/interfaces.md#stratum-v2-template-provider). The wire row is [`COMPAT.md`](../COMPAT.md). The crate is [`CRATES.md`](./CRATES.md).

`noise_sv2` 2.1.0 links `secp256k1` 0.28.2. The workspace links `secp256k1` 0.29.1. The two builds use distinct symbol prefixes.
