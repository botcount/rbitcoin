rbitcoin_store::skip_flushes_before_main!();

#[global_allocator]
static ALLOC: alloc_count::Counting = alloc_count::Counting;

mod alloc_count;

/// Arm the tip-accept lane permit before any test thread starts. A node
/// binary does not link this suite, so production never takes that lock.
#[used]
#[cfg_attr(
    any(target_os = "linux", target_os = "android", target_os = "freebsd"),
    link_section = ".init_array"
)]
#[cfg_attr(target_os = "macos", link_section = "__DATA,__mod_init_func")]
#[cfg_attr(target_os = "windows", link_section = ".CRT$XCU")]
static ENABLE_LANE_TEST_GATE: extern "C" fn() = {
    extern "C" fn enable() {
        rbitcoin_net::tip_accept::enable_lane_test_gate();
    }
    enable
};

mod asmap;
mod ban;
mod cache;
mod chain;
mod codec;
mod compact;
mod ephemeral;
mod error;
mod eviction;
mod fee_history;
mod fee_history_file;
mod i2p_sam;
mod inv_bucket;
mod listen;
mod log_burst;
mod mempool_removed;
mod most_work;
mod msg_decode;
mod net_permissions;
mod netaddr;
mod netgroup;
mod parent_req;
mod peer;
mod peer_dos;
mod peers;
mod perf_meter;
mod reactor;
mod root;
mod seeds;
mod service;
mod socks;
mod tip_accept;
mod tx_relay;
mod unconfirmed_script;
mod v1;
mod v2;
mod versionbits_warn;

/// Admit through [`rbitcoin_net::MempoolHub::accept_with_utxo`], the sync core
/// that peer relay and orphan promotion ship. A `Local` admit here does not
/// wake the isolated broadcast loop; a test that turns that loop on admits
/// with `accept_tx_from_async`.
pub(crate) trait Admit {
    fn admit_from(
        &self,
        tx: &bitcoin::Transaction,
        origin: rbitcoin_net::TxOrigin,
    ) -> Result<rbitcoin_net::AcceptResult, rbitcoin_net::AcceptError>;

    fn admit(
        &self,
        tx: &bitcoin::Transaction,
    ) -> Result<rbitcoin_net::AcceptResult, rbitcoin_net::AcceptError> {
        self.admit_from(tx, rbitcoin_net::TxOrigin::Relay(None))
    }
}

impl Admit for rbitcoin_net::MempoolHub {
    fn admit_from(
        &self,
        tx: &bitcoin::Transaction,
        origin: rbitcoin_net::TxOrigin,
    ) -> Result<rbitcoin_net::AcceptResult, rbitcoin_net::AcceptError> {
        self.accept_with_utxo(
            tx,
            &self.utxo_provider(),
            origin,
            rbitcoin_net::tx_relay::AcceptPath::Relay,
            &[],
        )
    }
}

/// Mine through [`rbitcoin_net::chain::ChainHub::generate_to_script_for_rpc_async`],
/// the path RPC `generate*` ships. The task runs on a suite runtime and the
/// caller blocks until it returns, so a plain test thread can call it.
pub(crate) trait Mine {
    fn mine(
        &self,
        nblocks: u32,
        script_pubkey: bitcoin::ScriptBuf,
        txs: Vec<bitcoin::Transaction>,
    ) -> Result<Vec<bitcoin::BlockHash>, rbitcoin_net::error::NetError>;
}

impl Mine for std::sync::Arc<rbitcoin_net::chain::ChainHub> {
    fn mine(
        &self,
        nblocks: u32,
        script_pubkey: bitcoin::ScriptBuf,
        txs: Vec<bitcoin::Transaction>,
    ) -> Result<Vec<bitcoin::BlockHash>, rbitcoin_net::error::NetError> {
        static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        let rt = RT.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("mine runtime")
        });
        let hub = std::sync::Arc::clone(self);
        let (tx, rx) = std::sync::mpsc::channel();
        rt.spawn(async move {
            let mined = hub
                .generate_to_script_for_rpc_async(nblocks, script_pubkey, txs, false)
                .await;
            // Release the clone before the caller wakes, so it can unwrap.
            drop(hub);
            let _ = tx.send(mined);
        });
        rx.recv().expect("mine task")
    }
}
