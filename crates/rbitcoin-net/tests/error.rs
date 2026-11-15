use rbitcoin_net::error::*;
use std::error::Error;
use std::io;

#[test]
fn display_and_source_surface() {
    let cases: Vec<(NetError, &str)> = vec![
        (NetError::Io(io::Error::other("x")), "io:"),
        (NetError::Encode("e".into()), "encode: e"),
        (NetError::Protocol("p"), "protocol: p"),
        (NetError::V1Peer, "peer does not speak BIP324 v2"),
        (NetError::Bip324("b".into()), "bip324: b"),
        (NetError::Timeout, "timeout"),
        (NetError::Disconnected, "peer disconnected"),
        (NetError::MessageTooLarge(9), "message too large (9 bytes)"),
        (
            NetError::InvalidV2Type { contents_len: 3 },
            "invalid v2 message type (3 bytes contents)",
        ),
        (NetError::BadMagic, "wrong network magic"),
        (NetError::Consensus("c".into()), "consensus: c"),
        (
            NetError::Rules(rbitcoin_consensus::ConsensusError::BadHeader(
                "timestamp <= median-time-past",
            )),
            "consensus: bad header: timestamp <= median-time-past",
        ),
        (NetError::Store("s".into()), "store: s"),
        (NetError::Cancelled, "confirm cancelled"),
        (
            NetError::Mutated("bad-txnmrklroot".into()),
            "consensus: bad-txnmrklroot",
        ),
        (NetError::BadPrev, "consensus: unexpected previous header"),
        (
            NetError::SideBlock,
            "protocol: side block; use accept_branch for reorg",
        ),
        (NetError::UnknownParent, "protocol: unknown parent"),
        (
            NetError::ConnectFailed {
                hash: [1u8; 32],
                msg: "bad spend".into(),
                rules: None,
            },
            "consensus: bad spend",
        ),
    ];
    for (err, needle) in cases {
        let s = err.to_string();
        assert!(s.contains(needle), "display={s:?} needle={needle}");
        match &err {
            NetError::Io(_) => assert!(err.source().is_some()),
            _ => assert!(err.source().is_none()),
        }
    }
    let from_io: NetError = io::Error::other("z").into();
    assert!(matches!(from_io, NetError::Io(_)));

    let mutated = NetError::Consensus("bad-txnmrklroot".into());
    assert!(mutated.is_mutated(), "{mutated}");
    let connect = NetError::ConnectFailed {
        hash: [2u8; 32],
        msg: "witness commitment".into(),
        rules: None,
    };
    assert!(connect.is_mutated(), "{connect}");
    let ordinary = NetError::Consensus("bad-txns-inputs-missingorspent".into());
    assert!(!ordinary.is_mutated(), "{ordinary}");
}
