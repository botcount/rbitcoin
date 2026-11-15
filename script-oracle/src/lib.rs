//! Bitcoin Core `VerifyScript` for the in-tree script corpus.
//!
//! Not a workspace member: the main graph must not link `bitcoinconsensus`
//! (that crates.io crate is Core 26). This crate compiles `VerifyScript`
//! from `third_party/bitcoin` at its pin. The Rust side only runs
//! as tests; the C++ wrapper is what the build script links.

#[cfg(test)]
mod tests {
    use bitcoin::consensus::encode::serialize;
    use bitcoin::script::ScriptBuf;
    use bitcoin::{Amount, Transaction, Witness};
    use rbitcoin_consensus::core_script::{credit_and_spend, spend_shape_from_json};

    const P2SH: u64 = 1 << 0;
    const STRICTENC: u64 = 1 << 1;
    const DERSIG: u64 = 1 << 2;
    const LOW_S: u64 = 1 << 3;
    const NULLDUMMY: u64 = 1 << 4;
    const SIGPUSHONLY: u64 = 1 << 5;
    const MINIMALDATA: u64 = 1 << 6;
    const DISCOURAGE_NOPS: u64 = 1 << 7;
    const CLEANSTACK: u64 = 1 << 8;
    const CLTV: u64 = 1 << 9;
    const CSV: u64 = 1 << 10;
    const WITNESS: u64 = 1 << 11;
    const DISCOURAGE_WITNESS: u64 = 1 << 12;
    const MINIMALIF: u64 = 1 << 13;
    const NULLFAIL: u64 = 1 << 14;
    const WITNESS_PUBKEYTYPE: u64 = 1 << 15;
    const CONST_SCRIPTCODE: u64 = 1 << 16;
    const TAPROOT: u64 = 1 << 17;
    const DISCOURAGE_TAPROOT: u64 = 1 << 18;
    const DISCOURAGE_SUCCESS: u64 = 1 << 19;
    const DISCOURAGE_PUBKEY: u64 = 1 << 20;

    extern "C" {
        fn rbtc_core_verify(
            tx: *const u8,
            tx_len: usize,
            spk: *const u8,
            spk_len: usize,
            amount: i64,
            input_index: u32,
            flags: u64,
        ) -> i32;
    }

    fn flags_from_core(text: &str) -> u64 {
        if text.is_empty() || text.eq_ignore_ascii_case("NONE") {
            return 0;
        }
        let mut flags = 0u64;
        for part in text.split(',') {
            match part.trim().to_ascii_uppercase().as_str() {
                "" => {}
                "P2SH" => flags |= P2SH,
                "STRICTENC" => flags |= STRICTENC | DERSIG,
                "DERSIG" => flags |= DERSIG,
                "LOW_S" => flags |= LOW_S | DERSIG,
                "NULLDUMMY" => flags |= NULLDUMMY,
                "SIGPUSHONLY" => flags |= SIGPUSHONLY,
                "MINIMALDATA" => flags |= MINIMALDATA,
                "DISCOURAGE_UPGRADABLE_NOPS" => flags |= DISCOURAGE_NOPS,
                "CLEANSTACK" => flags |= CLEANSTACK,
                "CHECKLOCKTIMEVERIFY" => flags |= CLTV,
                "CHECKSEQUENCEVERIFY" => flags |= CSV,
                "WITNESS" => flags |= WITNESS,
                "MINIMALIF" => flags |= MINIMALIF,
                "NULLFAIL" => flags |= NULLFAIL,
                "WITNESS_PUBKEYTYPE" => flags |= WITNESS_PUBKEYTYPE,
                "CONST_SCRIPTCODE" => flags |= CONST_SCRIPTCODE,
                "TAPROOT" | "TAPSCRIPT" => flags |= TAPROOT | WITNESS,
                "DISCOURAGE_UPGRADABLE_WITNESS_PROGRAM" => flags |= DISCOURAGE_WITNESS,
                "DISCOURAGE_UPGRADABLE_TAPROOT_VERSION" => flags |= DISCOURAGE_TAPROOT,
                "DISCOURAGE_OP_SUCCESS" => flags |= DISCOURAGE_SUCCESS,
                "DISCOURAGE_UPGRADABLE_PUBKEYTYPE" => flags |= DISCOURAGE_PUBKEY,
                other => panic!("unknown script flag {other}"),
            }
        }
        flags
    }

    fn core_accepts(spend: &Transaction, spk: &[u8], amount: i64, flags: u64) -> Result<bool, i32> {
        let raw = serialize(spend);
        let rc = unsafe {
            rbtc_core_verify(
                raw.as_ptr(),
                raw.len(),
                spk.as_ptr(),
                spk.len(),
                amount,
                0,
                flags,
            )
        };
        match rc {
            1 => Ok(true),
            0 => Ok(false),
            other => Err(other),
        }
    }

    fn witness_from_cell(cell: &serde_json::Value) -> (Witness, Amount, Option<[u8; 32]>) {
        let arr = cell.as_array().expect("witness cell");
        let mut amount = Amount::ZERO;
        let mut stack: Vec<Vec<u8>> = Vec::new();
        let mut tap_out = None;
        for (i, item) in arr.iter().enumerate() {
            if let Some(n) = item.as_f64() {
                if i + 1 == arr.len() || item.as_str().is_none() {
                    let sats = (n * 100_000_000.0).round() as i64;
                    amount = Amount::from_sat(sats.max(0) as u64);
                    continue;
                }
            }
            if let Some(s) = item.as_str() {
                rbitcoin_consensus::core_script::push_core_witness_token(
                    &mut stack,
                    &mut tap_out,
                    s,
                )
                .unwrap_or_else(|e| panic!("witness token: {e}"));
            } else if let Some(n) = item.as_f64() {
                let sats = (n * 100_000_000.0).round() as i64;
                amount = Amount::from_sat(sats.max(0) as u64);
            }
        }
        let refs: Vec<&[u8]> = stack.iter().map(|v| v.as_slice()).collect();
        let witness = Witness::from_slice(&refs);
        (witness, amount, tap_out)
    }

    #[test]
    fn script_corpus_matches_core_verify_script() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../crates/rbitcoin-consensus/tests/fixtures/script_corpus/script_tests.json");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        let rows: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
        assert!(!rows.is_empty());
        for row in &rows {
            let id = row["id"].as_str().unwrap();
            let cells = row["row"].as_array().unwrap();
            let (witness, amount, sig_s, pk_s, flag_text, expect, tap_out) = if cells[0].is_array()
            {
                assert!(cells.len() >= 5, "{id}");
                let (witness, amount, tap_out) = witness_from_cell(&cells[0]);
                (
                    witness,
                    amount,
                    cells[1].as_str().unwrap(),
                    cells[2].as_str().unwrap(),
                    cells[3].as_str().unwrap(),
                    cells[4].as_str().unwrap(),
                    tap_out,
                )
            } else {
                assert!(cells.len() >= 4, "{id}");
                (
                    Witness::new(),
                    Amount::ZERO,
                    cells[0].as_str().unwrap(),
                    cells[1].as_str().unwrap(),
                    cells[2].as_str().unwrap(),
                    cells[3].as_str().unwrap(),
                    None,
                )
            };
            let sig = rbitcoin_consensus::core_script::assemble_script(sig_s)
                .unwrap_or_else(|e| panic!("{id} scriptSig: {e}"));
            let pk = rbitcoin_consensus::core_script::assemble_script_pubkey(pk_s, tap_out)
                .unwrap_or_else(|e| panic!("{id} scriptPubKey: {e}"));
            let shape = spend_shape_from_json(row.get("spend"))
                .unwrap_or_else(|e| panic!("{id} spend: {e}"));
            let (_credit, spend) = credit_and_spend(
                ScriptBuf::from_bytes(pk.clone()),
                ScriptBuf::from_bytes(sig),
                witness,
                amount,
                shape,
            );
            let spk = pk;
            let got = core_accepts(
                &spend,
                &spk,
                amount.to_sat() as i64,
                flags_from_core(flag_text),
            )
            .unwrap_or_else(|rc| panic!("{id}: Core verify status {rc}"));
            assert_eq!(got, expect == "OK", "{id}");
        }
    }
}
