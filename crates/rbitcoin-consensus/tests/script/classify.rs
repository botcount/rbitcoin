use bitcoin::script::ScriptBuf;
use rbitcoin_consensus::script::classify::*;

#[test]
fn classify_standard_templates() {
    let p2wpkh = ScriptBuf::from_bytes({
        let mut v = vec![0x00, 0x14];
        v.extend_from_slice(&[0u8; 20]);
        v
    });
    assert_eq!(classify(p2wpkh.as_script()), ScriptKind::P2wpkh);

    let p2wsh = ScriptBuf::from_bytes({
        let mut v = vec![0x00, 0x20];
        v.extend_from_slice(&[0u8; 32]);
        v
    });
    assert_eq!(classify(p2wsh.as_script()), ScriptKind::P2wsh);

    let p2tr = ScriptBuf::from_bytes({
        let mut v = vec![0x51, 0x20];
        v.extend_from_slice(&[0u8; 32]);
        v
    });
    assert_eq!(classify(p2tr.as_script()), ScriptKind::P2tr);

    let p2pkh = ScriptBuf::from_bytes({
        let mut v = vec![0x76, 0xa9, 0x14];
        v.extend_from_slice(&[0u8; 20]);
        v.extend_from_slice(&[0x88, 0xac]);
        v
    });
    match classify(p2pkh.as_script()) {
        ScriptKind::P2pkh(raw) => assert_eq!(&raw[3..23], &[0u8; 20]),
        other => panic!("p2pkh template, got {other:?}"),
    }

    let p2sh = ScriptBuf::from_bytes({
        let mut v = vec![0xa9, 0x14];
        v.extend_from_slice(&[0u8; 20]);
        v.push(0x87);
        v
    });
    assert_eq!(classify(p2sh.as_script()), ScriptKind::P2sh);

    assert!(is_anyone_can_spend(
        ScriptBuf::from_bytes(vec![0x51]).as_script()
    ));
    // Empty is bare consensus-eval, not anyone-can-spend (Core parity).
    assert!(!is_anyone_can_spend(ScriptBuf::new().as_script()));
    assert_eq!(classify(ScriptBuf::new().as_script()), ScriptKind::Bare);
    // OP_2..OP_15 bare branch (not ACS).
    assert_eq!(
        classify(ScriptBuf::from_bytes(vec![0x52]).as_script()),
        ScriptKind::Bare
    );

    // BIP141 witness programs.
    let mut v16 = vec![0x60u8, 0x14];
    v16.extend_from_slice(&[0x4cu8; 20]);
    assert_eq!(
        witness_program(ScriptBuf::from_bytes(v16).as_script()).map(|(v, p)| (v, p.len())),
        Some((16, 20))
    );
    let mut v0 = vec![0x00u8, 0x14];
    v0.extend_from_slice(&[0u8; 20]);
    assert_eq!(
        witness_program(ScriptBuf::from_bytes(v0).as_script()).map(|(v, p)| (v, p.len())),
        Some((0, 20))
    );
    assert!(witness_program(ScriptBuf::from_bytes(vec![0x51]).as_script()).is_none());

    // BIP141 maximum program is 40 bytes (script length 42). One byte past
    // that is not a witness program.
    let mut v40 = vec![0x00u8, 40];
    v40.extend_from_slice(&[0xabu8; 40]);
    assert_eq!(
        witness_program(ScriptBuf::from_bytes(v40).as_script()).map(|(v, p)| (v, p.len())),
        Some((0, 40))
    );
    let mut too_long = vec![0x00u8, 40];
    too_long.extend_from_slice(&[0xabu8; 40]);
    too_long.push(0x00);
    assert!(witness_program(ScriptBuf::from_bytes(too_long).as_script()).is_none());
}
