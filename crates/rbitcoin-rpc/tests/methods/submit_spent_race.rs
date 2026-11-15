//! `submitblock` input pre-check when a competing block connects mid-check.
//!
//! B and X are both children of tip T, or X reorgs T out. The pre-check
//! reads the tip, then X connects, then it reads B's inputs. Core checks
//! inputs only in `ConnectBlock`, against B's own parent: B is an equal-work
//! side block, `inconclusive`, and never cached invalid.

use super::{block_hex, ctx_from_mature_pad};
use bitcoin::absolute::LockTime;
use bitcoin::transaction::Version as TxVersion;
use bitcoin::{Amount, Block, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness};
use rbitcoin_primitives::Height;
use rbitcoin_rpc::methods::mine::{during_next_submit_inputs, SubmitInputsPoint};
use rbitcoin_rpc::methods::*;
use rbitcoin_store::testutil::TempDir;
use serde_json::{json, Value};
use std::sync::Arc;

const SPENT: &str = "bad-txns-inputs-missingorspent";

struct Race {
    ctx: RpcContext,
    dir: TempDir,
    hub: Arc<rbitcoin_net::ChainHub>,
    b: Block,
    x: Block,
    c: Block,
}

fn spend(op: OutPoint, value: u64) -> Transaction {
    Transaction {
        version: TxVersion::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: op,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(value),
            script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
        }],
    }
}

/// How the competing tip X arrives.
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// X is T's other child and spends B's last coin with B's tx.
    Sibling,
    /// X is T's other child and carries every tx B carries.
    SiblingSameTxs,
    /// X is the child of T's held sibling T' and spends B's last coin.
    /// Delivering X reorgs T out. The coins confirm below T.
    Reorg,
    /// As `Reorg`, but T' spends B's last coin, at T's height.
    ReorgSpentAtParent,
    /// As `Reorg`, but X spends nothing. B's coins are created in T, which
    /// the reorg disconnects.
    ReorgParentCoins,
}

const SHAPES: [Shape; 5] = [
    Shape::Sibling,
    Shape::SiblingSameTxs,
    Shape::Reorg,
    Shape::ReorgSpentAtParent,
    Shape::ReorgParentCoins,
];

/// B (child of tip T) spends `n` coins of one fan-out tx, one tx each. X is
/// the competing block per `shape`. C is B's child, so B+C outworks X.
fn setup(n: usize, shape: Shape) -> Race {
    let (ctx, dir, hub) = ctx_from_mature_pad(300_000_000);
    let mine = |prev: &Block, dt: u32, h: u32, script: u8, txs: Vec<Transaction>| {
        rbitcoin_consensus::mine_regtest_paying(
            prev.block_hash(),
            prev.header.time + dt,
            h,
            ScriptBuf::from_bytes(vec![script]),
            txs,
        )
    };
    let mut h = hub.tip_height().unwrap();
    let mut tip = hub.query.reconstruct_block_at_height(Height(h)).unwrap();
    let cb = hub
        .query
        .reconstruct_block_at_height(Height(1))
        .unwrap()
        .txdata[0]
        .clone();
    let each = cb.output[0].value.to_sat() / (n as u64 + 1);
    let mut fan = spend(
        OutPoint {
            txid: cb.compute_txid(),
            vout: 0,
        },
        each,
    );
    fan.output = vec![fan.output[0].clone(); n];
    let fan_id = fan.compute_txid();
    let fan_in_t = !matches!(shape, Shape::Reorg | Shape::ReorgSpentAtParent);
    if !fan_in_t {
        // P1 confirms the coins under both T and T'.
        let p1 = mine(&tip, 1, h + 1, 0x51, vec![fan.clone()]);
        hub.accept_received_block(p1.clone()).unwrap();
        tip = p1;
        h += 1;
    }
    let t = mine(
        &tip,
        1,
        h + 1,
        0x51,
        if fan_in_t { vec![fan] } else { vec![] },
    );
    hub.accept_received_block(t.clone()).unwrap();
    assert_eq!(hub.tip_hash(), Some(t.block_hash()));
    let txs: Vec<Transaction> = (0..n as u32)
        .map(|vout| spend(OutPoint { txid: fan_id, vout }, each - 1_000))
        .collect();
    let last = txs.last().unwrap().clone();
    let b = mine(&t, 1, h + 2, 0x51, txs.clone());
    // Coinbase scripts differ so no two blocks share a coinbase txid.
    let x = match shape {
        Shape::Sibling => mine(&t, 2, h + 2, 0x52, vec![last]),
        Shape::SiblingSameTxs => mine(&t, 2, h + 2, 0x52, txs),
        Shape::Reorg | Shape::ReorgSpentAtParent | Shape::ReorgParentCoins => {
            let in_t2 = if matches!(shape, Shape::ReorgSpentAtParent) {
                vec![last.clone()]
            } else {
                vec![]
            };
            let t2 = mine(&tip, 2, h + 1, 0x53, in_t2);
            let held = hub.accept_received_block(t2.clone()).unwrap();
            assert!(
                matches!(held, rbitcoin_net::AcceptOutcome::IgnoredWeaker),
                "{held:?}"
            );
            let spends = if matches!(shape, Shape::Reorg) {
                vec![last]
            } else {
                vec![]
            };
            mine(&t2, 1, h + 2, 0x52, spends)
        }
    };
    let c = mine(&b, 1, h + 3, 0x51, vec![]);
    Race {
        ctx,
        dir,
        hub,
        b,
        x,
        c,
    }
}

fn submit(ctx: &RpcContext, block: &Block) -> Value {
    dispatch(ctx, "submitblock", vec![json!(block_hex(block))]).unwrap()
}

/// X connects after the pre-check read the tip and before it read B's
/// inputs. B is a held sibling, as when X arrives first. C then reorgs to B.
#[test]
fn submit_spent_race_tip_moves_mid_check() {
    for shape in SHAPES {
        let r = setup(2, shape);
        let (hub, x) = (Arc::clone(&r.hub), r.x.clone());
        let mut x = Some(x);
        during_next_submit_inputs(move |point| {
            if point == SubmitInputsPoint::BeforeInputs {
                hub.accept_received_block(x.take().unwrap()).unwrap();
            }
        });
        assert_eq!(submit(&r.ctx, &r.b), json!("inconclusive"), "{shape:?}");
        assert_eq!(r.hub.tip_hash(), Some(r.x.block_hash()), "{shape:?}");
        assert!(!r.hub.is_block_invalid(&r.b.block_hash()), "{shape:?}");
        assert_eq!(submit(&r.ctx, &r.c), Value::Null, "{shape:?}");
        assert_eq!(r.hub.tip_hash(), Some(r.c.block_hash()), "{shape:?}");
        let _ = std::fs::remove_dir_all(&r.dir);
    }
}

/// X reorgs T out before B's input reads, and T' (X's parent, which spent
/// B's coin at T's height) is invalidated after them, so T is the tip again
/// at the recheck. The reads saw T', not T: B extends T.
#[test]
fn submit_spent_race_reorg_undone_mid_check() {
    let r = setup(2, Shape::ReorgSpentAtParent);
    let (hub, x) = (Arc::clone(&r.hub), r.x.clone());
    during_next_submit_inputs(move |point| match point {
        SubmitInputsPoint::BeforeInputs => {
            hub.accept_received_block(x.clone()).unwrap();
            assert_eq!(hub.tip_hash(), Some(x.block_hash()));
        }
        SubmitInputsPoint::BeforeRecheck => {
            hub.invalidate_block(x.header.prev_blockhash).unwrap();
        }
    });
    assert_eq!(submit(&r.ctx, &r.b), Value::Null);
    assert_eq!(r.hub.tip_hash(), Some(r.b.block_hash()));
    let _ = std::fs::remove_dir_all(&r.dir);
}

/// D spends the coin T's fan-out spent, and X connects on T before D's input reads.
/// The reject read while the tip moved is the accept's to give, and D is an
/// equal-work sibling of X there, as when X arrives first.
#[test]
fn submit_spent_race_reject_read_while_tip_moves() {
    let r = setup(2, Shape::Sibling);
    let cb = r
        .hub
        .query
        .reconstruct_block_at_height(Height(1))
        .unwrap()
        .txdata[0]
        .compute_txid();
    let d = rbitcoin_consensus::mine_regtest_paying(
        r.b.header.prev_blockhash,
        r.b.header.time + 2,
        r.hub.tip_height().unwrap() + 1,
        ScriptBuf::from_bytes(vec![0x54]),
        vec![spend(OutPoint { txid: cb, vout: 0 }, 1)],
    );
    let (hub, x) = (Arc::clone(&r.hub), r.x.clone());
    let mut x = Some(x);
    during_next_submit_inputs(move |point| {
        if point == SubmitInputsPoint::BeforeInputs {
            hub.accept_received_block(x.take().unwrap()).unwrap();
        }
    });
    assert_eq!(submit(&r.ctx, &d), json!("inconclusive"));
    assert!(!r.hub.is_block_invalid(&d.block_hash()));
    let _ = std::fs::remove_dir_all(&r.dir);
}

/// Controls. B alone extends T. With X already the tip, B is held and not
/// marked. A block on that still tip that spends X's spent coin again is
/// rejected and cached.
#[test]
fn submit_spent_race_controls() {
    let r = setup(2, Shape::Sibling);
    assert_eq!(submit(&r.ctx, &r.b), Value::Null);
    assert_eq!(r.hub.tip_hash(), Some(r.b.block_hash()));
    let _ = std::fs::remove_dir_all(&r.dir);

    let r = setup(2, Shape::Sibling);
    r.hub.accept_received_block(r.x.clone()).unwrap();
    assert_eq!(submit(&r.ctx, &r.b), json!("inconclusive"));
    assert!(!r.hub.is_block_invalid(&r.b.block_hash()));

    // X spent B's last coin. D, on tip X, spends it again.
    let d = rbitcoin_consensus::mine_regtest_paying(
        r.x.block_hash(),
        r.x.header.time + 1,
        r.hub.tip_height().unwrap() + 1,
        ScriptBuf::from_bytes(vec![0x54]),
        vec![spend(
            r.b.txdata.last().unwrap().input[0].previous_output,
            1,
        )],
    );
    assert_eq!(submit(&r.ctx, &d), json!(SPENT));
    assert!(r.hub.is_block_invalid(&d.block_hash()));
    assert_eq!(submit(&r.ctx, &d), json!("duplicate-invalid"));
    let _ = std::fs::remove_dir_all(&r.dir);
}
