use bitcoin::blockdata::constants::genesis_block;
use bitcoin::consensus::serialize;
use bitcoin::p2p::message::NetworkMessage;
use bitcoin::p2p::Magic;
use bitcoin::Network;
use rbitcoin_net::codec::*;

fn signet_magic() -> Magic {
    Magic::from(Network::Signet)
}

#[test]
fn frame_decode_verack_ignores_checksum() {
    let frame = FramedMessage {
        magic: signet_magic(),
        command: *b"verack\0\0\0\0\0\0",
        payload: Vec::new(),
    };
    assert!(!frame.decode_is_cpu_heavy());
    assert!(matches!(frame.decode().payload(), NetworkMessage::Verack));
}

#[test]
fn block_hash_from_header_matches_full_block() {
    let magic = Magic::from(Network::Bitcoin);
    let genesis = genesis_block(Network::Bitcoin);
    let want = genesis.block_hash();
    let payload = serialize(&genesis);
    let frame = FramedMessage {
        magic,
        command: *b"block\0\0\0\0\0\0\0",
        payload,
    };
    assert!(frame.is_block());
    assert_eq!(frame.block_hash_from_header().expect("header hash"), want);
    match frame.decode().payload() {
        NetworkMessage::Block(b) => assert_eq!(b.block_hash(), want),
        _ => panic!("expected block"),
    }
}

/// Post-IBD `FramedMessage::decode` rejects a trailing byte. The same
/// bytes without it are a block. Decode does not touch the invalid-hash set.
#[test]
fn trailing_byte_on_a_regtest_block_is_unknown() {
    let genesis = genesis_block(Network::Regtest);
    let payload = serialize(&genesis);
    let clean = FramedMessage {
        magic: Magic::from(Network::Regtest),
        command: *b"block\0\0\0\0\0\0\0",
        payload: payload.clone(),
    };
    match clean.decode().payload() {
        NetworkMessage::Block(b) => assert_eq!(b.block_hash(), genesis.block_hash()),
        other => panic!("expected block, got {other:?}"),
    }
    let mut extra = payload;
    extra.push(0);
    let trailed = FramedMessage {
        magic: Magic::from(Network::Regtest),
        command: *b"block\0\0\0\0\0\0\0",
        payload: extra,
    };
    match trailed.decode().payload() {
        NetworkMessage::Unknown { command, .. } => {
            assert_eq!(command.to_string(), "block");
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[test]
fn command_bytes_ok_accepts_null_padded() {
    assert!(command_bytes_ok(b"version\0\0\0\0\0"));
    assert!(command_bytes_ok(b"ping\0\0\0\0\0\0\0\0"));
    // Digits: BIP155 sendaddrv2 / addrv2 (long form).
    assert!(command_bytes_ok(b"sendaddrv2\0\0"));
    assert!(command_bytes_ok(b"addrv2\0\0\0\0\0\0"));
    assert!(!command_bytes_ok(b"\xff\xfe\0\0\0\0\0\0\0\0\0\0"));
    assert!(!command_bytes_ok(b"ping\0x\0\0\0\0\0\0\0")); // non-zero after null
    assert!(!command_bytes_ok(b"\x01ping\0\0\0\0\0\0\0")); // control char
}

#[test]
fn core_limits_documented() {
    assert_eq!(MAX_PROTOCOL_MESSAGE_LENGTH, 4_000_000);
    assert_eq!(MAX_INV_SIZE, 50_000);
    let mut payload = vec![0xfd];
    payload.extend_from_slice(&50_001u16.to_le_bytes());
    let frame = FramedMessage {
        magic: Magic::from(bitcoin::Network::Regtest),
        command: *b"inv\0\0\0\0\0\0\0\0\0",
        payload: payload.clone(),
    };
    match frame.try_decode() {
        Err(rbitcoin_net::error::NetError::MessageTooLarge(n)) => assert_eq!(n, 50_001),
        other => panic!("oversize inv must fail decode, got {other:?}"),
    }
    let mut at_cap = vec![0xfd];
    at_cap.extend_from_slice(&50_000u16.to_le_bytes());
    let at_cap = FramedMessage {
        magic: Magic::from(bitcoin::Network::Regtest),
        command: *b"inv\0\0\0\0\0\0\0\0\0",
        payload: at_cap,
    };
    assert!(
        !matches!(
            at_cap.try_decode(),
            Err(rbitcoin_net::error::NetError::MessageTooLarge(_))
        ),
        "exactly 50_000 inv entries is still a message"
    );
    for command in [*b"getdata\0\0\0\0\0", *b"notfound\0\0\0\0"] {
        let over = FramedMessage {
            magic: Magic::from(bitcoin::Network::Regtest),
            command,
            payload: payload.clone(),
        };
        match over.try_decode() {
            Err(rbitcoin_net::error::NetError::MessageTooLarge(n)) => assert_eq!(n, 50_001),
            other => panic!("oversize {command:?} must fail decode, got {other:?}"),
        }
    }
    let tx = FramedMessage {
        magic: Magic::from(bitcoin::Network::Regtest),
        command: *b"tx\0\0\0\0\0\0\0\0\0\0",
        payload,
    };
    assert!(
        !matches!(
            tx.try_decode(),
            Err(rbitcoin_net::error::NetError::MessageTooLarge(_))
        ),
        "a tx payload is not an inv count"
    );
    assert_eq!(MAX_HEADERS_RESULTS, 2_000);
    assert_eq!(MAX_LOCATOR_SZ, 101);
    // Stricter than rust-bitcoin's 5MB
    const {
        assert!(MAX_PROTOCOL_MESSAGE_LENGTH < bitcoin::p2p::message::MAX_MSG_SIZE);
    }
}

#[test]
fn headers_count_over_2000_is_message_too_large() {
    use bitcoin::consensus::encode::{Encodable, VarInt};
    use rbitcoin_net::error::NetError;

    fn count_payload(n: u64) -> Vec<u8> {
        let mut v = Vec::new();
        VarInt(n).consensus_encode(&mut v).unwrap();
        v
    }

    let over = FramedMessage {
        magic: signet_magic(),
        command: *b"headers\0\0\0\0\0",
        payload: count_payload((MAX_HEADERS_RESULTS as u64) + 1),
    };
    match over.try_decode() {
        Err(NetError::MessageTooLarge(n)) => assert_eq!(n, MAX_HEADERS_RESULTS + 1),
        other => panic!("expected MessageTooLarge, got {other:?}"),
    }

    let at_cap = FramedMessage {
        magic: signet_magic(),
        command: *b"headers\0\0\0\0\0",
        payload: count_payload(MAX_HEADERS_RESULTS as u64),
    };
    assert!(
        !matches!(at_cap.try_decode(), Err(NetError::MessageTooLarge(_))),
        "n=2000 must not be MessageTooLarge"
    );
}

#[test]
fn frame_helpers_ping_headers_notfound_and_encode_cost() {
    let magic = signet_magic();
    let nonce: u64 = 0x1122_3344_5566_7788;
    let payload = nonce.to_le_bytes().to_vec();
    let ping = FramedMessage {
        magic,
        command: *b"ping\0\0\0\0\0\0\0\0",
        payload: payload.clone(),
    };
    assert!(ping.is_ping());
    assert_eq!(ping.ping_nonce(), Some(nonce));
    assert!(!ping.decode_is_cpu_heavy());

    let short = FramedMessage {
        magic,
        command: *b"ping\0\0\0\0\0\0\0\0",
        payload: vec![1, 2, 3],
    };
    assert!(short.ping_nonce().is_none());

    let headers = FramedMessage {
        magic,
        command: *b"headers\0\0\0\0\0",
        payload: vec![],
    };
    assert!(headers.is_headers());
    assert!(headers.decode_is_cpu_heavy());
    assert!(headers.block_hash_from_header().is_none());

    let nf = FramedMessage {
        magic,
        command: *b"notfound\0\0\0\0",
        payload: vec![],
    };
    assert!(nf.is_notfound());
    assert!(nf.decode_is_cpu_heavy());

    // Corrupt payload → Unknown path (not a panic).
    let bad = FramedMessage {
        magic,
        command: *b"block\0\0\0\0\0\0\0",
        payload: vec![0u8; 10],
    };
    match bad.decode().payload() {
        NetworkMessage::Unknown { command, .. } => {
            assert_eq!(command.to_string(), "block");
        }
        other => panic!("expected Unknown, got {other:?}"),
    }

    assert!(!encode_is_cpu_heavy(&NetworkMessage::Verack));
    let one = genesis_block(Network::Regtest).header;
    assert!(!encode_is_cpu_heavy(&NetworkMessage::Headers(vec![])));
    assert!(!encode_is_cpu_heavy(&NetworkMessage::Headers(vec![one; 8])));
    assert!(encode_is_cpu_heavy(&NetworkMessage::Headers(vec![one; 9])));
    assert!(encode_is_cpu_heavy(&NetworkMessage::Headers(vec![
        one;
        MAX_HEADERS_RESULTS
    ])));
    assert!(encode_is_cpu_heavy(&NetworkMessage::Block(genesis_block(
        Network::Regtest
    ))));
    use bitcoin::hashes::Hash as _;
    use bitcoin::p2p::message_blockdata::Inventory;
    let small = NetworkMessage::Inv(vec![Inventory::Block(bitcoin::BlockHash::from_byte_array(
        [0; 32],
    ))]);
    assert!(!encode_is_cpu_heavy(&small));
    let large = NetworkMessage::Inv(vec![
        Inventory::Block(bitcoin::BlockHash::from_byte_array(
            [0; 32]
        ));
        65
    ]);
    assert!(encode_is_cpu_heavy(&large));
}

fn padded(name: &str) -> [u8; 12] {
    let mut c = [0u8; 12];
    c[..name.len()].copy_from_slice(name.as_bytes());
    c
}

fn decoded(name: &str, payload: Vec<u8>) -> NetworkMessage {
    frame(padded(name), payload).decode().payload().clone()
}

/// addr, compact filters, reject, and alert stay distinct from an unknown
/// command. Feefilter accepts 0 through max money. A trailing byte is not
/// verack. Sixty-four invs stay off the heavy path.
#[test]
#[allow(clippy::cognitive_complexity)] // one codec, many rare-frame arms
fn peer_frames_keep_rare_commands_and_feefilter_bounds() {
    use bitcoin::bip158::{FilterHash, FilterHeader};
    use bitcoin::hashes::Hash;
    use bitcoin::p2p::address::Address;
    use bitcoin::p2p::message_filter::{CFCheckpt, CFHeaders, CFilter};
    use bitcoin::p2p::message_network::{Reject, RejectReason};
    use bitcoin::p2p::ServiceFlags;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    let zero = [0u8; 32];
    let block = bitcoin::BlockHash::from_byte_array(zero);
    let addr = Address::new(
        &SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 8333),
        ServiceFlags::NONE,
    );
    assert!(matches!(
        decoded("addr", serialize(&vec![(1u32, addr)])),
        NetworkMessage::Addr(v) if v.len() == 1
    ));
    assert!(matches!(
        decoded(
            "cfilter",
            serialize(&CFilter {
                filter_type: 0,
                block_hash: block,
                filter: vec![0x01],
            })
        ),
        NetworkMessage::CFilter(f) if f.filter == vec![0x01]
    ));
    assert!(matches!(
        decoded(
            "cfheaders",
            serialize(&CFHeaders {
                filter_type: 0,
                stop_hash: block,
                previous_filter_header: FilterHeader::from_byte_array(zero),
                filter_hashes: vec![FilterHash::from_byte_array(zero)],
            })
        ),
        NetworkMessage::CFHeaders(h) if h.filter_hashes.len() == 1
    ));
    assert!(matches!(
        decoded(
            "cfcheckpt",
            serialize(&CFCheckpt {
                filter_type: 0,
                stop_hash: block,
                filter_headers: vec![FilterHeader::from_byte_array(zero)],
            })
        ),
        NetworkMessage::CFCheckpt(c) if c.filter_headers.len() == 1
    ));
    assert!(matches!(
        decoded(
            "reject",
            serialize(&Reject {
                message: "tx".into(),
                ccode: RejectReason::Malformed,
                reason: "bad".into(),
                hash: bitcoin::hashes::sha256d::Hash::from_byte_array(zero),
            })
        ),
        NetworkMessage::Reject(_)
    ));
    assert!(matches!(
        decoded("alert", serialize(&vec![0xAAu8])),
        NetworkMessage::Alert(b) if b == vec![0xAA]
    ));

    let upper: i64 = bitcoin::Amount::MAX_MONEY.to_sat().try_into().unwrap();
    assert!(matches!(
        decoded("feefilter", serialize(&0i64)),
        NetworkMessage::FeeFilter(0)
    ));
    assert!(matches!(
        decoded("feefilter", serialize(&upper)),
        NetworkMessage::FeeFilter(fee) if fee == upper
    ));
    for bad in [-1i64, upper.saturating_add(1)] {
        assert!(
            matches!(
                decoded("feefilter", serialize(&bad)),
                NetworkMessage::Unknown { .. }
            ),
            "feefilter {bad} must not become a fee"
        );
    }

    // A trailing byte is not verack.
    assert!(matches!(
        decoded("verack", vec![0]),
        NetworkMessage::Unknown { .. }
    ));

    use bitcoin::p2p::message_blockdata::Inventory;
    let exact = NetworkMessage::Inv(vec![
        Inventory::Block(bitcoin::BlockHash::from_byte_array(
            zero
        ));
        64
    ]);
    assert!(
        !encode_is_cpu_heavy(&exact),
        "64 invs stay on the async worker; 65 is the heavy side"
    );
}

#[test]
fn ping_nonce_reads_eight_and_ignores_short() {
    let nonce = 0x1122_3344_5566_7788u64;
    let full = frame(*b"ping\0\0\0\0\0\0\0\0", nonce.to_le_bytes().to_vec());
    assert_eq!(full.ping_nonce(), Some(nonce));
    let short = frame(*b"ping\0\0\0\0\0\0\0\0", vec![1, 2, 3]);
    assert_eq!(short.ping_nonce(), None);
}

fn frame(command: [u8; 12], payload: Vec<u8>) -> FramedMessage {
    FramedMessage {
        magic: signet_magic(),
        command,
        payload,
    }
}

/// version, marker, flag, one empty input, one empty output, then a witness
/// stack count of 4_000_000 and no element bytes.
fn short_huge_witness_tx() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&1i32.to_le_bytes());
    p.push(0x00);
    p.push(0x01);
    p.push(1);
    p.extend_from_slice(&[0u8; 36]);
    p.push(0);
    p.extend_from_slice(&0xffff_ffffu32.to_le_bytes());
    p.push(1);
    p.extend_from_slice(&0u64.to_le_bytes());
    p.push(0);
    p.push(0xfe);
    p.extend_from_slice(&4_000_000u32.to_le_bytes());
    p
}

fn assert_witness_count_too_large(command: [u8; 12], payload: Vec<u8>) {
    assert!(
        payload.len() < 512,
        "the frame must stay short so the failure is the count, not the byte cap"
    );
    match frame(command, payload).try_decode() {
        Err(rbitcoin_net::error::NetError::MessageTooLarge(n)) => assert_eq!(n, 4_000_000),
        other => panic!("huge witness count must be misbehavior, got {other:?}"),
    }
}

#[test]
fn short_merkleblock_is_unknown_without_decoding() {
    use bitcoin::consensus::serialize;
    use bitcoin::MerkleBlock;

    let genesis = genesis_block(Network::Regtest);
    let mb = MerkleBlock::from_block_with_predicate(&genesis, |_| true);
    let payload = serialize(&mb);
    match frame(*b"merkleblock\0", payload).try_decode() {
        Ok(msg) => match msg.payload() {
            NetworkMessage::Unknown { command, .. } => {
                assert_eq!(command.to_string(), "merkleblock");
            }
            other => panic!("a merkleblock must be ignored, got {other:?}"),
        },
        Err(e) => panic!("merkleblock must be Unknown, not {e}"),
    }

    let mut huge = vec![0u8; 80];
    huge.extend_from_slice(&1u32.to_le_bytes());
    huge.push(1);
    huge.extend_from_slice(&[0x11; 32]);
    huge.push(0xfe);
    huge.extend_from_slice(&4_000_000u32.to_le_bytes());
    assert!(huge.len() < 200);
    match frame(*b"merkleblock\0", huge).try_decode() {
        Ok(msg) => match msg.payload() {
            NetworkMessage::Unknown { command, .. } => {
                assert_eq!(command.to_string(), "merkleblock");
            }
            other => panic!("huge bit-count must not decode, got {other:?}"),
        },
        Err(e) => panic!("huge merkleblock must be Unknown, not {e}"),
    }

    match frame(*b"zzzzzzzzzzzz", Vec::new()).try_decode() {
        Ok(msg) => assert!(matches!(msg.payload(), NetworkMessage::Unknown { .. })),
        Err(e) => panic!("unrecognized command stays Unknown, got {e}"),
    }

    match frame(*b"tx\0\0\0\0\0\0\0\0\0\0", vec![1, 0, 0, 0]).try_decode() {
        Ok(msg) => assert!(
            matches!(msg.payload(), NetworkMessage::Unknown { .. }),
            "a truncated tx is not the witness-count reject"
        ),
        Err(e) => panic!("truncated tx must stay Unknown, got {e}"),
    }
}

#[test]
fn short_tx_witness_count_is_message_too_large() {
    assert_witness_count_too_large(*b"tx\0\0\0\0\0\0\0\0\0\0", short_huge_witness_tx());
}

#[test]
fn short_cmpctblock_witness_count_is_message_too_large() {
    let mut payload = vec![0u8; 88];
    payload.push(0);
    payload.push(1);
    payload.push(0);
    payload.extend(short_huge_witness_tx());
    assert_witness_count_too_large(*b"cmpctblock\0\0", payload);
}

#[test]
fn short_blocktxn_witness_count_is_message_too_large() {
    let mut payload = vec![0u8; 32];
    payload.push(1);
    payload.extend(short_huge_witness_tx());
    assert_witness_count_too_large(*b"blocktxn\0\0\0\0", payload);
}

#[test]
fn short_block_witness_count_is_message_too_large() {
    let mut payload = vec![0u8; 80];
    payload.push(1);
    payload.extend(short_huge_witness_tx());
    assert_witness_count_too_large(*b"block\0\0\0\0\0\0\0", payload);
}

#[test]
fn witness_tx_cmpctblock_and_blocktxn_still_decode() {
    use bitcoin::bip152::{BlockTransactions, HeaderAndShortIds, PrefilledTransaction, ShortId};
    use bitcoin::consensus::serialize;
    use bitcoin::hashes::Hash as _;
    use bitcoin::p2p::message_compact_blocks::{BlockTxn, CmpctBlock};

    let tx = bitcoin::Transaction {
        version: bitcoin::transaction::Version::ONE,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![bitcoin::TxIn {
            previous_output: bitcoin::OutPoint::null(),
            script_sig: bitcoin::ScriptBuf::new(),
            sequence: bitcoin::Sequence::MAX,
            witness: bitcoin::Witness::from_slice(&[b"abcd"]),
        }],
        output: vec![bitcoin::TxOut {
            value: bitcoin::Amount::from_sat(1),
            script_pubkey: bitcoin::ScriptBuf::new(),
        }],
    };
    match frame(*b"tx\0\0\0\0\0\0\0\0\0\0", serialize(&tx))
        .try_decode()
        .expect("witness tx")
        .payload()
    {
        NetworkMessage::Tx(got) => assert_eq!(got, &tx),
        other => panic!("expected tx, got {other:?}"),
    }

    let genesis = genesis_block(Network::Regtest);
    let compact = CmpctBlock {
        compact_block: HeaderAndShortIds {
            header: genesis.header,
            nonce: 1,
            short_ids: vec![ShortId::with_siphash_keys(
                &tx.compute_txid().to_byte_array(),
                (1, 2),
            )],
            prefilled_txs: vec![PrefilledTransaction {
                idx: 0,
                tx: tx.clone(),
            }],
        },
    };
    match frame(*b"cmpctblock\0\0", serialize(&compact))
        .try_decode()
        .expect("cmpctblock")
        .payload()
    {
        NetworkMessage::CmpctBlock(got) => {
            assert_eq!(got.compact_block.prefilled_txs.len(), 1);
            assert_eq!(got.compact_block.prefilled_txs[0].tx, tx);
        }
        other => panic!("expected cmpctblock, got {other:?}"),
    }

    let blocktxn = BlockTxn {
        transactions: BlockTransactions {
            block_hash: bitcoin::BlockHash::from_byte_array([7; 32]),
            transactions: vec![tx.clone()],
        },
    };
    match frame(*b"blocktxn\0\0\0\0", serialize(&blocktxn))
        .try_decode()
        .expect("blocktxn")
        .payload()
    {
        NetworkMessage::BlockTxn(got) => assert_eq!(got.transactions.transactions, vec![tx]),
        other => panic!("expected blocktxn, got {other:?}"),
    }

    let block_payload = serialize(&genesis);
    match frame(*b"block\0\0\0\0\0\0\0", block_payload)
        .try_decode()
        .expect("genesis block")
        .payload()
    {
        NetworkMessage::Block(got) => assert_eq!(got.block_hash(), genesis.block_hash()),
        other => panic!("expected block, got {other:?}"),
    }
}

#[test]
fn skip_consumes_an_exact_tail() {
    let mut data: &[u8] = &[1, 2, 3, 4];
    assert!(skip(&mut data, 4));
    assert!(data.is_empty());
}

#[test]
fn take_len_bytes_exact_fit_is_fits_and_over_is_too_large() {
    let mut exact: &[u8] = &[4, 1, 2, 3, 4];
    assert!(matches!(take_len_bytes(&mut exact), Bound::Fits));
    assert!(exact.is_empty());
    let mut over: &[u8] = &[5, 1, 2, 3, 4];
    assert!(
        matches!(take_len_bytes(&mut over), Bound::TooLarge(_)),
        "a length past the buffer is too large"
    );
}

#[test]
fn walk_witness_allows_an_empty_element_that_fills_the_tail() {
    let mut data: &[u8] = &[1, 0];
    assert!(
        matches!(walk_witness(&mut data), Bound::Fits),
        "one empty element uses the last byte"
    );
}

#[test]
fn walk_cmpctblock_rejects_one_prefill_in_twelve_bytes() {
    let mut data = vec![0u8; 88];
    data.push(0);
    data.push(1);
    data.extend_from_slice(&[0u8; 12]);
    let mut cur: &[u8] = &data;
    assert!(
        matches!(walk_cmpctblock(&mut cur), Bound::TooLarge(_)),
        "one prefill needs more than twelve bytes"
    );
}
