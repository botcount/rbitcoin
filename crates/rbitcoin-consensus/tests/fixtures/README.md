# Consensus test fixtures

| File | Origin | License |
|------|--------|---------|
| `*.bin` / `*.hex` / `*.txt` | Captured mainnet/signet blocks for regression | project |
| `bip352_send_and_receive_test_vectors.json` | BIP-352 official send/receive vectors | BSD-2-Clause (BIP) |
| `bip54/coinbases.json`, `bip54/txsize.json`, `bip54/sigops.json.zst`, `bip54/timestamps.json.zst` | BIP 54 official test vectors, `bip-0054/test_vectors/` in bitcoin/bips at `b58f8ef8`. The two large files are the same JSON under `zstd -19` | CC0-1.0 (BIP) |
| `block_866342/raw.zst` | Floresta `crates/floresta-chain/testdata/block_866342/raw.zst` (mainnet height 866342, hash `000000000000000000014ce9ba7c6760053c3c82ce6ab43d60afb101d3c8f1f1`) | MIT OR Apache-2.0 |
| `block_866342/spent_utxos.zst` | Same Floresta pack: JSON array of `{txout, is_coinbase, creation_height, creation_time}` in non-coinbase input order | MIT OR Apache-2.0 |

Core JSON under `src/test/data/` (`script_tests.json`, `tx_valid.json`,
`tx_invalid.json`, `sighash.json`, `bip341_wallet_vectors.json`, …) is
**not** checked in here. Each `cargo test` run hard-links or copies named
files from Bitcoin Core master **`d206226b`**, after v32.1
(`d206226b4993f0330d809ca81851fb6976574ee7`) at
`third_party/bitcoin/src/test/data/` into `$CARGO_TARGET_DIR/core-data/`.

```bash
./scripts/core-functional/init-submodule.sh   # also invoked by cargo test / coverage.sh if missing
./scripts/core-functional/sync-core-fixtures.sh --check   # submodule present; no copies here
```

Do **not** curl from `master` and do **not** add rows to those JSON files —
add a rust unit instead.

## Harness

| Corpus | Test (lib) |
|--------|------------|
| `script_tests.json` | `cargo test -p rbitcoin-consensus --test suite core_script_tests_all_rows -- --nocapture` |
| `tx_valid.json` | `cargo test -p rbitcoin-consensus --test suite core_tx_valid_all_rows -- --nocapture` (flag subsets: `core_tx_valid_flag_subsets`) |
| `tx_invalid.json` | `cargo test -p rbitcoin-consensus --test suite core_tx_invalid_all_rows -- --nocapture` (restriction supersets: `core_tx_invalid_flag_supersets`) |
| `sighash.json` | `cargo test -p rbitcoin-consensus --test suite core_sighash_all_rows -- --nocapture` |
| `bip341_wallet_vectors.json` | `cargo test -p rbitcoin-consensus --test suite core_bip341 -- --nocapture` |
| `block_866342/` (vendored zstd) | `cargo test -p rbitcoin-consensus --test suite block_866342 -- --nocapture` |
| `bip54/` | `cargo test -p rbitcoin-consensus --test suite bip54_vectors -- --nocapture` |

Success requires **fail == 0** with no allowlist (see `docs/consensus-tests.md`). Soft majority pass rates are not used.
