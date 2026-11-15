//! Classify standard `scriptPubKey` templates.

use bitcoin::script::Script;
use rbitcoin_primitives::{OP_CHECKSIG, OP_DUP, OP_EQUALVERIFY, OP_HASH160, OP_PUSHBYTES_20};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptKind<'a> {
    /// OP_0 OP_PUSHBYTES_20 <20>
    P2wpkh,
    /// OP_0 OP_PUSHBYTES_32 <32>
    P2wsh,
    /// OP_1 OP_PUSHBYTES_32 <32>
    P2tr,
    /// OP_DUP OP_HASH160 OP_PUSHBYTES_20 <20> OP_EQUALVERIFY OP_CHECKSIG
    P2pkh(&'a [u8; 25]),
    /// OP_HASH160 OP_PUSHBYTES_20 <20> OP_EQUAL
    P2sh,
    /// Bare (P2PK, custom, empty spk) — run interpreter
    Bare,
}

/// Anyone-can-spend templates only. **Empty** `scriptPubKey` is **not** ACS
/// (empty stack → fail without TRUE). Only explicit `OP_TRUE`.
///
/// **Not** a short-circuit past scriptSig: scriptSig still runs (CLTV/CSV may
/// live there). Callers must evaluate scriptSig first or use the bare path.
pub fn is_anyone_can_spend(script: &Script) -> bool {
    script.as_bytes() == [rbitcoin_primitives::OP_TRUE]
}

/// BIP141 witness program: `OP_0`/`OP_1`..`OP_16` + a single push of 2..=40 bytes.
/// Returns `(version, program)` where version is 0 for OP_0 and 1..=16 for OP_1..OP_16.
pub fn witness_program(script: &Script) -> Option<(u8, &[u8])> {
    let version = script.witness_version()?;
    Some((version.to_num(), &script.as_bytes()[2..]))
}

/// Program length as a type. A 20-byte or 32-byte program cannot be another length.
pub enum ProgramLen<'a> {
    N20(&'a [u8; 20]),
    N32(&'a [u8; 32]),
    Other(&'a [u8]),
}

pub fn program_len(program: &[u8]) -> ProgramLen<'_> {
    if let Ok(program) = <&[u8; 20]>::try_from(program) {
        ProgramLen::N20(program)
    } else if let Ok(program) = <&[u8; 32]>::try_from(program) {
        ProgramLen::N32(program)
    } else {
        ProgramLen::Other(program)
    }
}

/// P2PKH template as the 25-byte script. The keyhash is bytes 3..23.
pub fn p2pkh_script(spk: &[u8]) -> Option<&[u8; 25]> {
    let raw: &[u8; 25] = spk.try_into().ok()?;
    if raw[0] == OP_DUP
        && raw[1] == OP_HASH160
        && raw[2] == OP_PUSHBYTES_20
        && raw[23] == OP_EQUALVERIFY
        && raw[24] == OP_CHECKSIG
    {
        Some(raw)
    } else {
        None
    }
}

pub fn classify(script: &Script) -> ScriptKind<'_> {
    if script.is_p2wpkh() {
        ScriptKind::P2wpkh
    } else if script.is_p2wsh() {
        ScriptKind::P2wsh
    } else if script.is_p2tr() {
        ScriptKind::P2tr
    } else if let Some(spk) = p2pkh_script(script.as_bytes()) {
        ScriptKind::P2pkh(spk)
    } else if script.is_p2sh() {
        ScriptKind::P2sh
    } else {
        ScriptKind::Bare
    }
}
