use bitcoin::secp256k1::Secp256k1;
use bitcoin::Network;
use rbitcoin_electrum::silent_scan::*;
use serde_json::json;

/// For a start that is a height: no header is read.
pub fn no_times(height: u32) -> Result<u32, String> {
    Err(format!("unexpected header read at {height}"))
}

// Official cryptographic outputs cannot be mined into a fresh test chain:
// their tweak commits to the fixture's input outpoints and keys.
#[test]
fn official_vectors_labeled_receiving() {
    use bitcoin::secp256k1::{PublicKey, SecretKey};
    use rbitcoin_consensus::{TaprootOut, TxTweak};
    use rbitcoin_primitives::{hex_decode, hex_encode};

    let cases: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../rbitcoin-consensus/tests/fixtures/bip352_send_and_receive_test_vectors.json"
    )))
    .unwrap();
    let secp = Secp256k1::new();
    let mut checked = 0;
    let mut misses = Vec::new();
    for case in cases.as_array().unwrap() {
        for rec in case["receiving"].as_array().unwrap() {
            let given = &rec["given"];
            let labels = given["labels"].as_array().unwrap();
            // The K_max vector specifies a count, not individual expected keys.
            let Some(expected) = rec["expected"]["outputs"].as_array() else {
                continue;
            };
            if labels.is_empty() {
                continue;
            }
            let spend = SecretKey::from_slice(
                &hex_decode(given["key_material"]["spend_priv_key"].as_str().unwrap()).unwrap(),
            )
            .unwrap();
            let spend = hex_encode(PublicKey::from_secret_key(&secp, &spend).serialize());
            let default_sub = parse_sub(
                &json!([given["key_material"]["scan_priv_key"], spend]),
                Network::Regtest,
                Some(0),
                &no_times,
            )
            .unwrap();
            let sub = parse_sub(
                &json!([given["key_material"]["scan_priv_key"], spend, 0, labels]),
                Network::Regtest,
                Some(0),
                &no_times,
            )
            .unwrap();
            let max_labels_sub = parse_sub(
                &json!([
                    given["key_material"]["scan_priv_key"],
                    spend,
                    0,
                    [0, 1, 2, 3, 4, 5, 6, 7, 1337, 1001337]
                ]),
                Network::Regtest,
                Some(0),
                &no_times,
            )
            .unwrap();
            let tweak = hex_decode(rec["expected"]["tweak"].as_str().unwrap())
                .unwrap()
                .try_into()
                .unwrap();
            let mut outputs = Vec::new();
            for output in given["outputs"].as_array().unwrap() {
                let wanted = expected.iter().any(|o| o["pub_key"] == *output);
                let tw = TxTweak {
                    tweak,
                    output_pubkeys: vec![TaprootOut {
                        vout: 0,
                        xonly: hex_decode(output.as_str().unwrap())
                            .unwrap()
                            .try_into()
                            .unwrap(),
                        value: 1,
                    }],
                };
                if tx_matches(&sub, &tw) != wanted {
                    misses.push(format!("{}: {output} expected {wanted}", case["comment"]));
                }
                assert_eq!(
                    tx_matches(&max_labels_sub, &tw),
                    wanted,
                    "ten labels: {}: {output}",
                    case["comment"]
                );
                if case["comment"]
                    .as_str()
                    .unwrap()
                    .starts_with("Receiving with labels:")
                {
                    assert!(!tx_matches(&default_sub, &tw), "unrequested label matched");
                }
                if labels.as_slice() == [json!(0)] && tx_matches(&default_sub, &tw) != wanted {
                    misses.push(format!("implicit change label: {output} expected {wanted}"));
                }
                checked += 1;
                outputs.extend(tw.output_pubkeys);
            }
            let mut tx = TxTweak {
                tweak,
                output_pubkeys: outputs,
            };
            assert!(
                tx_matches(&sub, &tx),
                "whole transaction: {}",
                case["comment"]
            );
            tx.output_pubkeys.reverse();
            assert!(
                tx_matches(&max_labels_sub, &tx),
                "reversed outputs with ten labels: {}",
                case["comment"]
            );
        }
    }
    assert_eq!(checked, 13);
    assert!(misses.is_empty(), "{}", misses.join("\n"));
}

#[test]
fn parse_sub_indexes_a_non_string_param() {
    match parse_sub(&json!([1, "02"]), Network::Regtest, Some(0), &no_times) {
        Err(err) => assert_eq!(err, "param 0 expected string"),
        Ok(_) => panic!("a non-string scan key must fail"),
    }
}

#[test]
fn parse_sub_rejects_short_scan_key() {
    let err = match parse_sub(
        &json!(["00", "02".repeat(33)]),
        Network::Regtest,
        Some(0),
        &no_times,
    ) {
        Err(e) => e,
        Ok(_) => panic!("expected error"),
    };
    assert!(err.contains("32 bytes"), "{err}");
    let _ = Secp256k1::new();
}

#[test]
fn parse_sub_labels_start_and_networks() {
    let scan = "0f694e068028a717f8af6b9411f9a133dd3565258714cc226594b34db90c1f2c";
    let spend = "025cc9856d6f8375350e123978daac200c260cb5b5ae83106cab90484dcd8fcf36";
    let sub = parse_sub(
        &json!([scan, spend, "12-20", [0, 1, 1]]),
        Network::Bitcoin,
        Some(100),
        &no_times,
    )
    .unwrap();
    assert_eq!(sub.start, 12);
    assert_eq!(sub.labels, vec![0, 1]);
    // BIP-352 send-and-receive vector 0: version `q`, then the two keys.
    assert_eq!(
        sub.address,
        "sp1qqgste7k9hx0qftg6qmwlkqtwuy6cycyavzmzj85c6qdfhjdpdjtdgqjuexzk6murw56suy3e0rd2cgqvycxttddwsvgxe2usfpxumr70xc9pkqwv"
    );
    let tsp = parse_sub(&json!([scan, spend]), Network::Signet, Some(3), &no_times).unwrap();
    assert!(tsp.address.starts_with("tsp1"), "{}", tsp.address);
    let t4 = parse_sub(&json!([scan, spend]), Network::Testnet4, Some(3), &no_times).unwrap();
    assert!(t4.address.starts_with("tsp"), "{}", t4.address);
    let tn = parse_sub(&json!([scan, spend]), Network::Testnet, Some(3), &no_times).unwrap();
    assert!(tn.address.starts_with("tsp"), "{}", tn.address);
    let no_tip = parse_sub(&json!([scan, spend, 50]), Network::Regtest, None, &no_times).unwrap();
    // Regtest is a test network: the same payload under `tsp`.
    assert_eq!(
        no_tip.address,
        "tsp1qqgste7k9hx0qftg6qmwlkqtwuy6cycyavzmzj85c6qdfhjdpdjtdgqjuexzk6murw56suy3e0rd2cgqvycxttddwsvgxe2usfpxumr70xc3wk4yh"
    );
    assert_eq!(no_tip.start, 50);
    let null_start = parse_sub(
        &json!([scan, spend, null]),
        Network::Regtest,
        Some(3),
        &no_times,
    )
    .unwrap();
    assert_eq!(null_start.start, 0);
    // The requested history is honored: no start is moved toward the tip.
    let genesis = parse_sub(
        &json!([scan, spend, 0]),
        Network::Regtest,
        Some(1_000),
        &no_times,
    )
    .unwrap();
    assert_eq!(genesis.start, 0);
    let unset = parse_sub(
        &json!([scan, spend]),
        Network::Regtest,
        Some(1_000),
        &no_times,
    )
    .unwrap();
    assert_eq!(unset.start, 0);
    let old = parse_sub(
        &json!([scan, spend, 1]),
        Network::Regtest,
        Some(10_000),
        &no_times,
    )
    .unwrap();
    assert_eq!(old.start, 1);
    assert_eq!(subscribe_result(&old)["start_height"], 1);
    let bad_spend = match parse_sub(&json!([scan, "02"]), Network::Regtest, Some(0), &no_times) {
        Err(e) => e,
        Ok(_) => panic!("spend"),
    };
    assert!(!bad_spend.is_empty(), "{bad_spend}");
    let many = json!([scan, spend, 0, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]]);
    let too = match parse_sub(&many, Network::Regtest, Some(0), &no_times) {
        Err(e) => e,
        Ok(_) => panic!("labels"),
    };
    assert!(too.contains("too many"), "{too}");
    let r = subscribe_result(&sub);
    assert_eq!(r["start_height"], 12);
    let plain = parse_sub(
        &json!([scan, spend, "12", ["x", 2]]),
        Network::Regtest,
        Some(20),
        &no_times,
    )
    .unwrap();
    assert_eq!(plain.start, 12);
    assert_eq!(plain.labels, vec![0, 2]);
    let clamp = parse_sub(
        &json!([scan, spend, 100]),
        Network::Regtest,
        Some(3),
        &no_times,
    )
    .unwrap();
    assert_eq!(clamp.start, 3);
    let at_cap = parse_sub(
        &json!([scan, spend, 500_000_000]),
        Network::Regtest,
        Some(3),
        &no_times,
    )
    .unwrap();
    assert_eq!(at_cap.start, 3);
}

/// A start that is not a height, a time, or a range is refused. Read as 0
/// it would scan from genesis.
#[test]
fn start_that_is_not_a_height_is_refused() {
    let parse = |start: serde_json::Value| {
        parse_sub(
            &json!([SYNTH_SCAN, SYNTH_SPEND, start]),
            Network::Regtest,
            Some(100),
            &no_times,
        )
    };
    for bad in [
        json!("nope"),
        json!(""),
        json!(1.5),
        json!(-3),
        json!(true),
        json!([7]),
        json!({"height": 7}),
    ] {
        match parse(bad.clone()) {
            Err(e) => assert!(e.contains("start must be"), "{bad}: {e}"),
            Ok(sub) => panic!("{bad} parsed to start {}", sub.start),
        }
    }
    assert_eq!(parse(json!("12")).unwrap().start, 12);
    assert_eq!(parse(json!(12)).unwrap().start, 12);
    assert_eq!(parse(json!(null)).unwrap().start, 0);
}

#[test]
fn tx_matches_bip352_simple_send() {
    let scan = "0f694e068028a717f8af6b9411f9a133dd3565258714cc226594b34db90c1f2c";
    let spend = "025cc9856d6f8375350e123978daac200c260cb5b5ae83106cab90484dcd8fcf36";
    let sub = parse_sub(
        &json!([scan, spend, 0, [0, 1]]),
        Network::Regtest,
        Some(0),
        &no_times,
    )
    .unwrap();
    let tweak = rbitcoin_primitives::hex_decode(
        "024ac253c216532e961988e2a8ce266a447c894c781e52ef6cee902361db960004",
    )
    .unwrap();
    let xonly = rbitcoin_primitives::hex_decode(
        "3e9fce73d4e77a4809908e3c3a2e54ee147b9312dc5044a193d1fc85de46e3c1",
    )
    .unwrap();
    let mut tw33 = [0u8; 33];
    tw33.copy_from_slice(&tweak);
    let mut x32 = [0u8; 32];
    x32.copy_from_slice(&xonly);
    let hit = rbitcoin_consensus::TxTweak {
        tweak: tw33,
        output_pubkeys: vec![rbitcoin_consensus::TaprootOut {
            vout: 0,
            xonly: x32,
            value: 1,
        }],
    };
    assert!(tx_matches(&sub, &hit));
    let miss = rbitcoin_consensus::TxTweak {
        tweak: tw33,
        output_pubkeys: vec![rbitcoin_consensus::TaprootOut {
            vout: 0,
            xonly: [0u8; 32],
            value: 1,
        }],
    };
    assert!(!tx_matches(&sub, &miss));
}

/// A pruned scan used to ask for the witness walk, which refuses, and the
/// server turned that error into an empty history. The sealed index still
/// has the payment.
#[test]
fn pruned_scan_returns_a_sealed_payment() {
    use bitcoin::secp256k1::{PublicKey, Scalar, SecretKey};
    use rbitcoin_consensus::silent_payments::tagged_hash;
    use rbitcoin_consensus::ChainParams;
    use rbitcoin_primitives::{hex_encode, Fk, Height};
    use rbitcoin_query::testutil::FixtureChain;
    use rbitcoin_query::tweaks_for_height;
    use rbitcoin_query::TxApply;
    use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};

    let secp = Secp256k1::new();
    let scan_sk = SecretKey::from_slice(&[0x11; 32]).unwrap();
    let spend_pk = PublicKey::from_secret_key(&secp, &SecretKey::from_slice(&[0x22; 32]).unwrap());
    let a = PublicKey::from_secret_key(&secp, &SecretKey::from_slice(&[0x33; 32]).unwrap());
    let tweak = a.serialize();
    let scan_scalar = Scalar::from_be_bytes(scan_sk.secret_bytes()).unwrap();
    let shared = a.mul_tweak(&secp, &scan_scalar).unwrap();
    let mut payload = [0u8; 37];
    payload[..33].copy_from_slice(&shared.serialize());
    let tk = SecretKey::from_slice(&tagged_hash(b"BIP0352/SharedSecret", &payload)).unwrap();
    let output = spend_pk
        .combine(&PublicKey::from_secret_key(&secp, &tk))
        .unwrap();
    let mut script = rbitcoin_primitives::P2TR_PREFIX.to_vec();
    script.extend_from_slice(&output.x_only_public_key().0.serialize());

    let (dir, q) = rbitcoin_query::testutil::tiny_query_labeled("el-scan-hit");
    q.set_sptweaks_enabled(true, Height(0)).unwrap();
    let mut merkle = [0u8; 32];
    merkle[31] = 0xcb;
    let header = HeaderRecord {
        prev_fk: Fk::NULL,
        version: 1,
        timestamp: 1,
        bits: 0x207fffff,
        nonce: 0,
        merkle_root: merkle,
        hash: merkle,
        size: 0,
        weight: 0,
    };
    let fk = q
        .connect_block(
            Height(0),
            &header,
            &[TxApply {
                tx: TxRecord {
                    txid: merkle,
                    version: 1,
                    locktime: 0,
                    input_count: 1,
                    output_count: 1,
                },
                inputs: vec![InputRecord::coinbase(u32::MAX, vec![0x00], vec![])],
                outputs: vec![OutputRecord::unspent(50_0000_0000, script)],
            }],
        )
        .unwrap();
    q.put_sp_tweaks_block(Height(0), fk, &[Some(tweak)])
        .unwrap();
    q.set_prune_seqsigwit(true).unwrap();
    let chain = ChainParams::regtest();
    let walked = tweaks_for_height(&q, &chain, Height(0)).unwrap_err();
    assert!(
        walked.to_string().contains("unavailable"),
        "the witness walk must stay refused, got {walked}"
    );
    let sub = parse_sub(
        &json!([
            hex_encode(scan_sk.secret_bytes()),
            hex_encode(spend_pk.serialize())
        ]),
        Network::Regtest,
        Some(0),
        &no_times,
    )
    .unwrap();
    let hits = scan_hits(&q, &chain, &sub, 0, 0).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0]["height"], 0);
    drop(q);
    let _ = std::fs::remove_dir_all(&dir);
}

const SYNTH_SCAN: &str = "0f694e068028a717f8af6b9411f9a133dd3565258714cc226594b34db90c1f2c";
const SYNTH_SPEND: &str = "025cc9856d6f8375350e123978daac200c260cb5b5ae83106cab90484dcd8fcf36";

/// Sparrow sends the wallet birthday as seconds since the epoch. The start
/// is the last block at least two hours older than that, as Bitcoin Core's
/// rescan picks it.
#[test]
fn timestamp_start_resolves_to_a_height() {
    const T0: u32 = 1_600_000_000;
    let time_at = |h: u32| Ok::<u32, String>(T0 + h * 600);
    let start_for = |start: serde_json::Value| {
        parse_sub(
            &json!([SYNTH_SCAN, SYNTH_SPEND, start]),
            Network::Regtest,
            Some(1_000),
            &time_at,
        )
        .unwrap()
        .start
    };
    // Block 500 carries the birthday. Two hours is twelve blocks.
    assert_eq!(start_for(json!(T0 + 500 * 600)), 488);
    // One second short of block 488's window keeps block 487.
    assert_eq!(start_for(json!(T0 + 500 * 600 - 1)), 487);
    assert_eq!(start_for(json!((T0 + 500 * 600).to_string())), 488);
    let sub = parse_sub(
        &json!([SYNTH_SCAN, SYNTH_SPEND, T0 + 500 * 600]),
        Network::Regtest,
        Some(1_000),
        &time_at,
    )
    .unwrap();
    assert_eq!(subscribe_result(&sub)["start_height"], 488);
    // Older than genesis scans everything. Newer than the tip scans the tip.
    assert_eq!(start_for(json!(500_000_001)), 0);
    assert_eq!(start_for(json!(T0 + 1_000 * 600 + 7_200)), 1_000);
    assert_eq!(start_for(json!(u64::from(u32::MAX) + 5)), 1_000);
    // The nLockTime boundary itself is still a height.
    let at_boundary = parse_sub(
        &json!([SYNTH_SCAN, SYNTH_SPEND, 500_000_000]),
        Network::Regtest,
        Some(1_000),
        &no_times,
    )
    .unwrap();
    assert_eq!(at_boundary.start, 1_000);
    // No chain yet: nothing to resolve against.
    let empty = parse_sub(
        &json!([SYNTH_SCAN, SYNTH_SPEND, T0]),
        Network::Regtest,
        None,
        &no_times,
    )
    .unwrap();
    assert_eq!(empty.start, 0);
}

/// Header times are not sorted. A block mined after the birthday may carry
/// an earlier time, inside the two hour window.
#[test]
fn timestamp_start_covers_a_block_with_an_early_time() {
    const T0: u32 = 1_600_000_000;
    let birthday = T0 + 500 * 600;
    // Block 500 is mined at the birthday with a clock 90 minutes slow.
    let time_at = |h: u32| {
        let t = T0 + h * 600;
        Ok::<u32, String>(if h == 500 { t - 5_400 } else { t })
    };
    let sub = parse_sub(
        &json!([SYNTH_SCAN, SYNTH_SPEND, birthday]),
        Network::Regtest,
        Some(1_000),
        &time_at,
    )
    .unwrap();
    assert!(sub.start <= 500, "start {} skips block 500", sub.start);
    assert!(sub.start >= 480, "start {} is before the window", sub.start);
}

#[test]
fn timestamp_start_reports_a_header_that_cannot_be_read() {
    let err = match parse_sub(
        &json!([SYNTH_SCAN, SYNTH_SPEND, 1_600_000_000]),
        Network::Regtest,
        Some(10),
        &no_times,
    ) {
        Err(e) => e,
        Ok(sub) => panic!("resolved to {}", sub.start),
    };
    assert!(err.contains("unexpected header read"), "{err}");
}

/// One synthetic receiver on an [`SpChain`].
struct SpWallet {
    scan_hex: String,
    spend_hex: String,
    paid_script: Vec<u8>,
}

/// A synthetic chain with silent payments to synthetic key pairs. Block
/// `h` carries the time `SP_CHAIN_T0 + h * 600`.
pub struct SpChain {
    _dir: rbitcoin_query::testutil::TempDir,
    pub q: std::sync::Arc<rbitcoin_query::Query>,
    pub scan_hex: String,
    pub spend_hex: String,
    wallets: Vec<SpWallet>,
    tweak: [u8; 33],
    tip_fk: rbitcoin_primitives::Fk,
    tip_hash: Option<[u8; 32]>,
    next: u32,
}

pub const SP_CHAIN_T0: u32 = 1_600_000_000;
/// Receivers an [`SpChain`] can pay. Wallet 0 is the default.
pub const SP_CHAIN_WALLETS: u8 = 20;

impl SpChain {
    /// Heights `0..=last`. Each height in `paid` has one payment to wallet 0.
    pub fn new(label: &str, last: u32, paid: &[u32]) -> Self {
        use bitcoin::secp256k1::{PublicKey, Scalar, SecretKey};
        use rbitcoin_consensus::silent_payments::tagged_hash;
        use rbitcoin_primitives::{hex_encode, Fk, Height};

        let secp = Secp256k1::new();
        let a = PublicKey::from_secret_key(&secp, &SecretKey::from_slice(&[0x77; 32]).unwrap());
        let wallets: Vec<SpWallet> = (0..SP_CHAIN_WALLETS)
            .map(|w| {
                let scan_sk = SecretKey::from_slice(&[0x11 + w; 32]).unwrap();
                let spend_pk = PublicKey::from_secret_key(
                    &secp,
                    &SecretKey::from_slice(&[0x41 + w; 32]).unwrap(),
                );
                let scan_scalar = Scalar::from_be_bytes(scan_sk.secret_bytes()).unwrap();
                let shared = a.mul_tweak(&secp, &scan_scalar).unwrap();
                let mut payload = [0u8; 37];
                payload[..33].copy_from_slice(&shared.serialize());
                let tk =
                    SecretKey::from_slice(&tagged_hash(b"BIP0352/SharedSecret", &payload)).unwrap();
                let output = spend_pk
                    .combine(&PublicKey::from_secret_key(&secp, &tk))
                    .unwrap();
                let mut paid_script = rbitcoin_primitives::P2TR_PREFIX.to_vec();
                paid_script.extend_from_slice(&output.x_only_public_key().0.serialize());
                SpWallet {
                    scan_hex: hex_encode(scan_sk.secret_bytes()),
                    spend_hex: hex_encode(spend_pk.serialize()),
                    paid_script,
                }
            })
            .collect();

        let (dir, q) = rbitcoin_query::testutil::tiny_query_labeled(label);
        q.set_sptweaks_enabled(true, Height(0)).unwrap();
        let mut chain = Self {
            _dir: dir,
            q: std::sync::Arc::new(q),
            scan_hex: wallets[0].scan_hex.clone(),
            spend_hex: wallets[0].spend_hex.clone(),
            wallets,
            tweak: a.serialize(),
            tip_fk: Fk::NULL,
            tip_hash: None,
            next: 0,
        };
        chain.extend(last, paid);
        chain
    }

    /// Connect heights through `last`. Each height in `paid` pays wallet 0.
    pub fn extend(&mut self, last: u32, paid: &[u32]) {
        let paid: Vec<(u32, u8)> = paid.iter().map(|h| (*h, 0)).collect();
        self.extend_paying(last, &paid);
    }

    /// Connect heights through `last`. `(height, wallet)` is one payment.
    pub fn extend_paying(&mut self, last: u32, paid: &[(u32, u8)]) {
        use rbitcoin_primitives::Height;
        use rbitcoin_query::testutil::FixtureChain;
        use rbitcoin_query::TxApply;
        use rbitcoin_store::{HeaderRecord, InputRecord, OutputRecord, TxRecord};

        for h in self.next..=last {
            let mut merkle = [0u8; 32];
            merkle[..4].copy_from_slice(&h.to_le_bytes());
            merkle[31] = 0xcb;
            let timestamp = SP_CHAIN_T0 + h * 600;
            let hash = match self.tip_hash {
                None => merkle,
                Some(parent) => {
                    rbitcoin_store::block_header_hash(1, &parent, &merkle, timestamp, 0x207fffff, 0)
                }
            };
            self.tip_hash = Some(hash);
            let header = HeaderRecord {
                prev_fk: self.tip_fk,
                version: 1,
                timestamp,
                bits: 0x207fffff,
                nonce: 0,
                merkle_root: merkle,
                hash,
                size: 0,
                weight: 0,
            };
            let payee = paid.iter().find(|(height, _)| *height == h).map(|p| p.1);
            let script = match payee {
                Some(w) => self.wallets[usize::from(w)].paid_script.clone(),
                None => vec![rbitcoin_primitives::OP_TRUE],
            };
            self.tip_fk = self
                .q
                .connect_block(
                    Height(h),
                    &header,
                    &[TxApply {
                        tx: TxRecord {
                            txid: merkle,
                            version: 1,
                            locktime: 0,
                            input_count: 1,
                            output_count: 1,
                        },
                        inputs: vec![InputRecord::coinbase(u32::MAX, vec![0x00], vec![])],
                        outputs: vec![OutputRecord::unspent(50_0000_0000, script)],
                    }],
                )
                .unwrap();
            self.q
                .put_sp_tweaks_block(Height(h), self.tip_fk, &[payee.map(|_| self.tweak)])
                .unwrap();
        }
        self.next = last + 1;
    }

    pub fn sub(&self, start: serde_json::Value) -> serde_json::Value {
        self.sub_for(0, start)
    }

    pub fn sub_for(&self, wallet: u8, start: serde_json::Value) -> serde_json::Value {
        let w = &self.wallets[usize::from(wallet)];
        json!([w.scan_hex, w.spend_hex, start])
    }
}

/// The fixture itself: a payment far below the tip is found by a direct scan.
#[test]
fn sp_chain_fixture_pays_the_synthetic_keys() {
    let chain = SpChain::new("el-sp-fixture", 300, &[7]);
    let params = rbitcoin_consensus::ChainParams::regtest();
    let sub = parse_sub(&chain.sub(json!(0)), Network::Regtest, Some(300), &no_times).unwrap();
    assert_eq!(sub.start, 0);
    let hits = scan_hits(&chain.q, &params, &sub, 0, 300).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0]["height"], 7);
}

/// Frigate `"FROM-TO"`: a closed range of heights.
#[test]
fn range_start_is_a_closed_range() {
    let parse = |start: &str| {
        parse_sub(
            &json!([SYNTH_SCAN, SYNTH_SPEND, start]),
            Network::Regtest,
            Some(100),
            &no_times,
        )
    };
    let err = |start: &str| match parse(start) {
        Err(e) => e,
        Ok(sub) => panic!("{start} parsed to {}..={:?}", sub.start, sub.end),
    };
    let range = parse("12-20").unwrap();
    assert_eq!((range.start, range.end), (12, Some(20)));
    assert_eq!(subscribe_result(&range)["start_height"], 12);
    let one = parse("7-7").unwrap();
    assert_eq!((one.start, one.end), (7, Some(7)));
    let to_tip = parse("0-100").unwrap();
    assert_eq!((to_tip.start, to_tip.end), (0, Some(100)));
    assert!(err("20-12").contains("from <= to"), "{}", err("20-12"));
    assert!(err("5-101").contains("exceeds tip"), "{}", err("5-101"));
    assert!(err("a-9").contains("integer"), "{}", err("a-9"));
    assert!(err("3-").contains("integer"), "{}", err("3-"));
    // A height alone runs to the tip and on.
    let open = parse_sub(
        &json!([SYNTH_SCAN, SYNTH_SPEND, 12]),
        Network::Regtest,
        Some(100),
        &no_times,
    )
    .unwrap();
    assert_eq!((open.start, open.end), (12, None));
    // No chain: no tip to exceed.
    let empty = parse_sub(
        &json!([SYNTH_SCAN, SYNTH_SPEND, "3-9"]),
        Network::Regtest,
        None,
        &no_times,
    )
    .unwrap();
    assert_eq!((empty.start, empty.end), (3, Some(9)));
}
