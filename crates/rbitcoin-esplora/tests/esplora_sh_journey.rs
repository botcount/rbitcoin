async fn cap_refuses_three_creates(q: &Query, app: &Router) {
    let h51 = block_hash_hex(&script_hash(&[0x51]));
    q.set_max_sh_creates(2);
    let (st, body) = oneshot_http(app, get_with_client(&format!("/scripthash/{h51}"), "cap")).await;
    assert_eq!(st, 503, "{body}");
    assert!(
        body.contains("scripthash join exceeds --max-sh-creates (3 creates, cap 2)"),
        "{body}"
    );
    let (st, utxo) =
        oneshot_http(app, get_with_client(&format!("/scripthash/{h51}/utxo"), "cap")).await;
    assert_eq!(st, 503, "unpaged utxo stays capped: {utxo}");
    let (st, txs) = oneshot_http(app, get_with_client(&format!("/scripthash/{h51}/txs"), "cap")).await;
    assert_eq!(st, 200, "paged /txs is served above the cap: {txs}");
    let (st, page) = oneshot_http(
        app,
        get_with_client(&format!("/scripthash/{h51}/txs/chain"), "cap"),
    )
    .await;
    assert_eq!(st, 200, "paged chain is served above the cap: {page}");
    let rows: Value = serde_json::from_str(&page).unwrap();
    assert!(!rows.as_array().unwrap().is_empty(), "{page}");
    let (st, sum) = oneshot_http(
        app,
        get_with_client(&format!("/scripthash/{h51}/txs/summary"), "cap"),
    )
    .await;
    assert_eq!(st, 200, "paged summary is served above the cap: {sum}");
    q.set_max_sh_creates(0);
}

async fn casa_reuses_last_slot(app: &Router, cache: &Mutex<JoinCache>) {
    let sh = script_hash(&[0x51]);
    let h51 = block_hash_hex(&sh);
    let (st, body) = oneshot_http(app, get_with_client(&format!("/scripthash/{h51}"), "casa")).await;
    assert_eq!(st, 200, "{body}");
    let info: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(info["chain_stats"]["tx_count"], 3);
    let (st, utxo) =
        oneshot_http(app, get_with_client(&format!("/scripthash/{h51}/utxo"), "casa")).await;
    assert_eq!(st, 200, "{utxo}");
    let utxos: Value = serde_json::from_str(&utxo).unwrap();
    assert_eq!(utxos.as_array().map(|a| a.len()), Some(3), "{utxo}");
    let (st, txs) =
        oneshot_http(app, get_with_client(&format!("/scripthash/{h51}/txs"), "casa")).await;
    assert_eq!(st, 200, "{txs}");
    assert_eq!(cache.lock().unwrap().last_sh_key("casa"), Some(sh));
}

async fn two_addresses_each_have_one_utxo(app: &Router, a1: &str, a2: &str) {
    let p1 = format!("/address/{a1}/utxo");
    let p2 = format!("/address/{a2}/utxo");
    let ((st1, b1), (st2, b2)) = tokio::join!(
        oneshot_http(app, get_with_client(&p1, "a")),
        oneshot_http(app, get_with_client(&p2, "b")),
    );
    assert_eq!(st1, 200, "{b1}");
    assert_eq!(st2, 200, "{b2}");
    let v1: Value = serde_json::from_str(&b1).unwrap();
    let v2: Value = serde_json::from_str(&b2).unwrap();
    assert_eq!(v1.as_array().map(|a| a.len()), Some(1), "{b1}");
    assert_eq!(v2.as_array().map(|a| a.len()), Some(1), "{b2}");
}

async fn last1_and_second_client(app: &Router, cache: &Mutex<JoinCache>, sh1: [u8; 32], sh2: [u8; 32]) {
    let h1hex = block_hash_hex(&sh1);
    let h2hex = block_hash_hex(&sh2);
    let (st, body) = oneshot_http(app, get_with_client(&format!("/scripthash/{h1hex}"), "c1")).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(cache.lock().unwrap().last_sh_key("c1"), Some(sh1));
    let (st, body) =
        oneshot_http(app, get_with_client(&format!("/scripthash/{h1hex}/utxo"), "c1")).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(cache.lock().unwrap().last_sh_key("c1"), Some(sh1));
    let (st, body) = oneshot_http(app, get_with_client(&format!("/scripthash/{h2hex}"), "c1")).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(
        cache.lock().unwrap().last_sh_key("c1"),
        Some(sh2),
        "GET B replaces last-1"
    );
    let (st, body) = oneshot_http(app, get_with_client(&format!("/scripthash/{h1hex}"), "c2")).await;
    assert_eq!(st, 200, "{body}");
    {
        let g = cache.lock().unwrap();
        assert_eq!(g.last_sh_key("c1"), Some(sh2));
        assert_eq!(g.last_sh_key("c2"), Some(sh1));
    }
}

/// A newest wallet page does not store a full join. The next page recomputes
/// its window. `last_bulk` stays empty.
async fn bulk_page_leaves_wallet_slot_empty(
    app: &Router,
    cache: &Mutex<JoinCache>,
    sh1: [u8; 32],
    sh2: [u8; 32],
) {
    let h1hex = block_hash_hex(&sh1);
    let h2hex = block_hash_hex(&sh2);
    let t1 = block_hash_hex(&[
        0x11, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0xaa,
    ]);
    let t2 = block_hash_hex(&[
        0x22, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0xaa,
    ]);
    let body = serde_json::to_vec(&json!([&h1hex, &h2hex])).unwrap();
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/scripthashes/txs")
        .header("X-Rbitcoin-Client", "wallet")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.clone()))
        .unwrap();
    let (st, resp) = oneshot_http(app, req).await;
    assert_eq!(st, 200, "{resp}");
    let rows: Value = serde_json::from_str(&resp).unwrap();
    let ids: Vec<&str> = rows
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["txid"].as_str())
        .collect();
    assert_eq!(ids, vec![t2.as_str(), t1.as_str()], "{resp}");
    assert_eq!(cache.lock().unwrap().bulk_len("wallet"), 0);
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/scripthashes/txs?after_txid={t2}"))
        .header("X-Rbitcoin-Client", "wallet")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body))
        .unwrap();
    let (st, resp) = oneshot_http(app, req).await;
    assert_eq!(st, 200, "{resp}");
    let rows: Value = serde_json::from_str(&resp).unwrap();
    let ids: Vec<&str> = rows
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["txid"].as_str())
        .collect();
    assert_eq!(ids, vec![t1.as_str()], "{resp}");
    assert_eq!(cache.lock().unwrap().bulk_len("wallet"), 0);
}

fn pay_height(h: u32, spk: bitcoin::ScriptBuf) -> TxApply {
    let mut txid = [0u8; 32];
    txid[0..4].copy_from_slice(&h.to_le_bytes());
    txid[31] = 0xaa;
    TxApply {
        tx: TxRecord { txid, version: 2, locktime: 0, input_count: 1, output_count: 1 },
        inputs: vec![InputRecord {
            prev_txid: [0u8; 32],
            parent: rbitcoin_store::InputParent::Coinbase,
            sequence: u32::MAX,
            script_sig: vec![],
            witness: vec![],
        }],
        outputs: vec![OutputRecord::unspent(1_0000_0000, spk.to_bytes())],
    }
}

async fn post_wallet_rows(
    app: &Router,
    path: &str,
    body: &[u8],
    after: Option<&str>,
) -> Vec<Value> {
    let uri = match after {
        Some(id) => format!("{path}?after_txid={id}"),
        None => path.to_string(),
    };
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("X-Rbitcoin-Client", "wallet")
        .body(axum::body::Body::from(body.to_vec()))
        .unwrap();
    let (st, resp) = oneshot_http(app, req).await;
    assert_eq!(st, 200, "{path}: {resp}");
    serde_json::from_str(&resp).unwrap()
}

fn row_txids(rows: &[Value]) -> Vec<&str> {
    rows.iter().filter_map(|v| v["txid"].as_str()).collect()
}

/// Script A has 30 creates, all newer than script B's 30. The merged page is
/// 25. The second page is A's remainder plus the front of B, and the third
/// page is the rest of B. One script over the create cap still returns 200
/// and leaves the wallet join slot empty.
#[tokio::test]
async fn multi_page_is_one_newest_window_per_script() {
    let (_a, spk_a) = regtest_p2wpkh();
    let (_b, spk_b) = regtest_p2wpkh_sk(9);
    let sh_a = block_hash_hex(&script_hash(spk_a.as_bytes()));
    let sh_b = block_hash_hex(&script_hash(spk_b.as_bytes()));
    let (dir, q) = temp_query("multi-window");
    let (h0, cb0) = coinbase(0, Fk::NULL, None);
    let mut prev = q.connect_block(Height(0), &h0, &[cb0]).unwrap();
    let mut parent = h0.hash;
    let mut older = Vec::new();
    let mut newer = Vec::new();
    for h in 1u32..=60 {
        let (header, cb) = coinbase(h, prev, Some(parent));
        let spk = if h <= 30 { spk_b.clone() } else { spk_a.clone() };
        let pay = pay_height(h, spk);
        let id = block_hash_hex(&pay.tx.txid);
        if h <= 30 {
            older.push(id);
        } else {
            newer.push(id);
        }
        prev = q.connect_block(Height(h), &header, &[cb, pay]).unwrap();
        parent = header.hash;
    }
    let a_new: Vec<&str> = newer.iter().rev().map(String::as_str).collect();
    let b_new: Vec<&str> = older.iter().rev().map(String::as_str).collect();
    let page1: Vec<&str> = a_new[..25].to_vec();
    let mut page2: Vec<&str> = a_new[25..].to_vec();
    page2.extend_from_slice(&b_new[..20]);
    let page3: Vec<&str> = b_new[20..].to_vec();

    q.set_max_sh_creates(10);
    let q = Arc::new(q);
    let cache = Arc::new(Mutex::new(JoinCache::default()));
    let app = app_with_join(Arc::clone(&q), Arc::clone(&cache), true);
    let body = serde_json::to_vec(&json!([sh_a, sh_b])).unwrap();

    let rows = post_wallet_rows(&app, "/scripthashes/txs", &body, None).await;
    assert_eq!(row_txids(&rows), page1);
    assert_eq!(cache.lock().unwrap().bulk_len("wallet"), 0);
    let rows = post_wallet_rows(&app, "/scripthashes/txs", &body, Some(page1[24])).await;
    assert_eq!(row_txids(&rows), page2);
    assert_eq!(cache.lock().unwrap().bulk_len("wallet"), 0);
    let rows = post_wallet_rows(&app, "/scripthashes/txs", &body, Some(page2[24])).await;
    assert_eq!(row_txids(&rows), page3);

    let rows = post_wallet_rows(&app, "/scripthashes/txs/summary", &body, None).await;
    assert_eq!(row_txids(&rows), page1);
    assert!(rows.iter().all(|v| v["value"] == 1_0000_0000));
    let rows = post_wallet_rows(&app, "/scripthashes/txs/summary", &body, Some(page1[24])).await;
    assert_eq!(row_txids(&rows), page2);
    assert!(rows.iter().all(|v| v["value"] == 1_0000_0000));
    let rows = post_wallet_rows(&app, "/scripthashes/txs/summary", &body, Some(page2[24])).await;
    assert_eq!(row_txids(&rows), page3);
    let _ = std::fs::remove_dir_all(&dir);
}

fn same_height_pay(mark: u8, second: u8, spk: bitcoin::ScriptBuf) -> TxApply {
    let mut txid = [0u8; 32];
    txid[0] = mark;
    txid[1] = second;
    txid[31] = 0xaa;
    let mut pay = pay_height(0, spk);
    pay.tx.txid = txid;
    pay
}

/// Same-height rows are ordered by internal txid, the key the next page
/// uses. Display hex would put a different row last and repeat the others.
#[tokio::test]
async fn multi_page_same_height_follows_internal_txid() {
    let (_a, spk) = regtest_p2wpkh();
    let sh = block_hash_hex(&script_hash(spk.as_bytes()));
    let (dir, q) = temp_query("same-height-page");
    let (h0, cb0) = coinbase(0, Fk::NULL, None);
    let prev = q.connect_block(Height(0), &h0, &[cb0]).unwrap();
    let w = same_height_pay(0x10, 0x00, spk.clone());
    let c = same_height_pay(0x02, 0x00, spk.clone());
    let v = same_height_pay(0x01, 0xff, spk);
    let ids = [w.tx.txid, c.tx.txid, v.tx.txid].map(|id| block_hash_hex(&id));
    let (h1, cb1) = coinbase(1, prev, Some(h0.hash));
    q.connect_block(Height(1), &h1, &[cb1, w, c, v]).unwrap();
    let q = Arc::new(q);
    let app = app_with_join(q, Arc::new(Mutex::new(JoinCache::default())), true);
    let body = serde_json::to_vec(&json!([sh])).unwrap();
    for path in ["/scripthashes/txs", "/scripthashes/txs/summary"] {
        let rows = post_wallet_rows(&app, path, &body, None).await;
        assert_eq!(row_txids(&rows), ids.iter().map(String::as_str).collect::<Vec<_>>(), "{path}");
        let rows = post_wallet_rows(&app, path, &body, Some(&ids[2])).await;
        assert!(row_txids(&rows).is_empty(), "{path} page 2: {rows:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

async fn untrusted_header_is_ignored(q: Arc<Query>, sh1: [u8; 32]) {
    let h1hex = block_hash_hex(&sh1);
    let public = Arc::new(Mutex::new(JoinCache::default()));
    let pub_app = app_with_join(Arc::clone(&q), Arc::clone(&public), false);
    let plain = axum::http::Request::builder()
        .uri(format!("/scripthash/{h1hex}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let (st_plain, body_plain) = oneshot_http(&pub_app, plain).await;
    let (st, body) =
        oneshot_http(&pub_app, get_with_client(&format!("/scripthash/{h1hex}"), "ignored")).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(st_plain, st);
    assert_eq!(
        body_plain, body,
        "public TCP does not read X-Rbitcoin-Client"
    );
    assert!(public.lock().unwrap().last_sh_key("ignored").is_none());

    let loopback = Arc::new(Mutex::new(JoinCache::default()));
    let lb_app = app_with_join(q, Arc::clone(&loopback), false);
    let mut req = get_with_client(&format!("/scripthash/{h1hex}"), "lb");
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1))));
    let (st, body) = oneshot_http(&lb_app, req).await;
    assert_eq!(st, 200, "{body}");
    assert!(
        loopback.lock().unwrap().last_sh_key("lb").is_none(),
        "loopback without join_header_trusted ignores X-Rbitcoin-Client"
    );
}

async fn template_cached_until_tip_moves(q: Arc<Query>, prev: Fk, parent: [u8; 32]) {
    let calls = Arc::new(AtomicU64::new(0));
    let c = Arc::clone(&calls);
    let qh = Arc::clone(&q);
    let mut cfg = EsploraConfig::with_network("127.0.0.1:0".parse().unwrap(), Network::Regtest);
    cfg.block_template = Some(BlockTemplateFn(Arc::new(move || {
        let n = c.fetch_add(1, Ordering::Relaxed);
        let tip = qh.tip_height().expect("tip");
        let hash = qh.header_at_height(tip).unwrap().unwrap().1.hash;
        Ok(json!({
            "height": 1,
            "rules": ["segwit"],
            "previousblockhash": block_hash_hex(&hash),
            "gen": n,
        }))
    })));
    let handle = run_esplora(cfg, Arc::clone(&q), None)
        .await
        .expect("listen");
    let (st, raw, body) = http_get_raw(handle.local_addr, "/block-template").await;
    assert_eq!(st, 200, "{body}");
    assert!(
        raw.to_ascii_lowercase().contains("cache-control: no-store"),
        "{raw}"
    );
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["height"], 1);
    assert_eq!(v["gen"], 0);
    let (st, again) = http_get(handle.local_addr, "/block-template").await;
    assert_eq!(st, 200, "{again}");
    let again: Value = serde_json::from_str(&again).unwrap();
    assert_eq!(again["gen"], 0, "cached body stays the first template");
    assert_eq!(calls.load(Ordering::Relaxed), 1, "15s cache");
    let (h3, t3) = coinbase(3, prev, Some(parent));
    q.connect_block(Height(3), &h3, &[t3]).unwrap();
    let (st, _) = http_get(handle.local_addr, "/block-template").await;
    assert_eq!(st, 200);
    assert_eq!(calls.load(Ordering::Relaxed), 2, "tip change invalidates");
    handle.shutdown().await;

    let mut cfg = EsploraConfig::new("127.0.0.1:0".parse().unwrap());
    cfg.block_template = Some(BlockTemplateFn(Arc::new(|| Err("no hub".into()))));
    let handle = run_esplora(cfg, q, None).await.expect("listen");
    let (st, body) = http_get(handle.local_addr, "/block-template").await;
    assert_eq!(st, 503, "{body}");
    assert!(body.contains("no hub"), "{body}");
    handle.shutdown().await;
}

async fn template_missing_tip_or_knob() {
    let (dir, empty) = temp_query("gbt-notip");
    let mut cfg = EsploraConfig::new("127.0.0.1:0".parse().unwrap());
    cfg.block_template = Some(BlockTemplateFn(Arc::new(|| Ok(json!({"height": 1})))));
    let handle = run_esplora(cfg, Arc::new(empty), None)
        .await
        .expect("listen");
    let (st, body) = http_get(handle.local_addr, "/block-template").await;
    assert_eq!(st, 503, "{body}");
    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);

    let (dir, q) = temp_query("gbt-404");
    let cfg = EsploraConfig::new("127.0.0.1:0".parse().unwrap());
    let handle = run_esplora(cfg, Arc::new(q), None)
        .await
        .expect("listen");
    let (st, body) = http_get(handle.local_addr, "/block-template").await;
    assert_eq!(st, 404, "{body}");
    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Cap below the create count: chain stats stay 503, and `/txs` matches
/// `/txs/summary` including each row's net value.
#[tokio::test]
async fn address_page_under_cap_matches_summary() {
    let (addr, spk) = regtest_p2wpkh();
    let (dir, q) = temp_query("esplora-addr-page");
    let (h0, cb0) = coinbase(0, Fk::NULL, None);
    let pay0 = two_script_pay(0x11, spk.clone());
    let pay0_txid = pay0.tx.txid;
    let h0fk = q.connect_block(Height(0), &h0, &[cb0, pay0]).unwrap();
    let create0 = q.block_tx_fks(Height(0)).unwrap()[1];

    let (h1, cb1) = coinbase(1, h0fk, Some(h0.hash));
    let pay1 = two_script_pay(0x22, spk.clone());
    let pay1_txid = pay1.tx.txid;
    let h1fk = q.connect_block(Height(1), &h1, &[cb1, pay1]).unwrap();

    let (h2, cb2) = coinbase(2, h1fk, Some(h1.hash));
    let pay2 = two_script_pay(0x33, spk);
    let mut spend = two_script_pay(0x44, bitcoin::ScriptBuf::from_bytes(vec![0x00]));
    spend.inputs[0].prev_txid = pay0_txid;
    spend.inputs[0].parent = rbitcoin_store::InputParent::Prevout {
        create_fk: create0,
        vout: 0,
    };
    let spend_txid = spend.tx.txid;
    let pay2_txid = pay2.tx.txid;
    q.connect_block(Height(2), &h2, &[cb2, pay2, spend]).unwrap();
    q.set_max_sh_creates(2);

    let cfg = EsploraConfig::with_network("127.0.0.1:0".parse().unwrap(), Network::Regtest);
    let handle = run_esplora(cfg, Arc::new(q), None).await.expect("listen");
    let (st, body) = http_get(handle.local_addr, &format!("/address/{addr}")).await;
    assert_eq!(st, 503, "{body}");
    assert!(
        body.contains("scripthash join exceeds --max-sh-creates"),
        "{body}"
    );

    let (st, txs_body) = http_get(handle.local_addr, &format!("/address/{addr}/txs")).await;
    assert_eq!(st, 200, "{txs_body}");
    let (st, sum_body) = http_get(handle.local_addr, &format!("/address/{addr}/txs/summary")).await;
    assert_eq!(st, 200, "{sum_body}");
    let txs: Vec<Value> = serde_json::from_str(&txs_body).unwrap();
    let sums: Vec<Value> = serde_json::from_str(&sum_body).unwrap();
    let txids: Vec<&str> = txs.iter().map(|v| v["txid"].as_str().unwrap()).collect();
    let sum_ids: Vec<&str> = sums.iter().map(|v| v["txid"].as_str().unwrap()).collect();
    assert_eq!(txids, sum_ids);
    assert_eq!(
        txids,
        vec![
            block_hash_hex(&spend_txid),
            block_hash_hex(&pay2_txid),
            block_hash_hex(&pay1_txid),
            block_hash_hex(&pay0_txid),
        ]
    );
    assert_eq!(sums[0]["value"], -1_0000_0000);
    assert_eq!(sums[1]["value"], 1_0000_0000);
    assert_eq!(sums[2]["value"], 1_0000_0000);
    assert_eq!(sums[3]["value"], 1_0000_0000);
    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A read that finds a torn extent key answers 503 `scripthash index
/// rebuilding` (not a 500 from the corrupt record), latches the degraded state,
/// and every other script answers 503 too until the rebuild publishes a head.
#[tokio::test]
async fn torn_key_read_is_503_for_every_script_until_rebuilt() {
    use rbitcoin_primitives::scripthash_display_hex as display;
    use rbitcoin_query::testutil::{heavy_sh_extent_query, tear_sh_extent_head, SH_HEAVY_SCRIPT};
    let (dir, q, key) = heavy_sh_extent_query("esplora-torn", 0);
    let q = Arc::new(q);
    let cfg = EsploraConfig::with_network("127.0.0.1:0".parse().unwrap(), Network::Regtest);
    let handle = run_esplora(cfg, q.clone(), None).await.expect("listen");
    let heavy = format!("/scripthash/{}/txs", display(SH_HEAVY_SCRIPT));
    let other = format!("/scripthash/{}/txs", display(&[0x51]));
    let (st, body) = http_get(handle.local_addr, &heavy).await;
    assert_eq!(st, 200, "{body}");
    tear_sh_extent_head(&q, &key);
    let (st, body) = http_get(handle.local_addr, &heavy).await;
    assert_eq!((st, body.as_str()), (503, "scripthash index rebuilding"));
    assert!(q.sh_degraded(), "the read latched");
    let (st, body) = http_get(handle.local_addr, &other).await;
    assert_eq!((st, body.as_str()), (503, "scripthash index rebuilding"));
    let q2 = q.clone();
    assert!(tokio::task::spawn_blocking(move || q2.rebuild_sh_if_degraded(None))
        .await
        .unwrap()
        .unwrap());
    let (st, body) = http_get(handle.local_addr, &heavy).await;
    assert_eq!(st, 200, "{body}");
    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn esplora_sh_join_and_template() {
    let (a1, spk1) = regtest_p2wpkh();
    let (a2, spk2) = regtest_p2wpkh_sk(9);
    let sh1 = script_hash(spk1.as_bytes());
    let sh2 = script_hash(spk2.as_bytes());
    let (dir, q) = temp_query("sh-join");
    let (h0, t0) = coinbase(0, Fk::NULL, None);
    let prev = q.connect_block(Height(0), &h0, &[t0]).unwrap();
    let (h1, cb1) = coinbase(1, prev, Some(h0.hash));
    let prev = q.connect_block(Height(1), &h1, &[cb1]).unwrap();
    let (h2, cb2) = coinbase(2, prev, Some(h1.hash));
    let prev = q
        .connect_block(
            Height(2),
            &h2,
            &[
                cb2,
                two_script_pay(0x11, spk1),
                two_script_pay(0x22, spk2),
            ],
        )
        .unwrap();
    let q = Arc::new(q);
    let cache = Arc::new(Mutex::new(JoinCache::default()));
    let app = app_with_join(Arc::clone(&q), Arc::clone(&cache), true);
    cap_refuses_three_creates(&q, &app).await;
    casa_reuses_last_slot(&app, &cache).await;
    two_addresses_each_have_one_utxo(&app, &a1, &a2).await;
    last1_and_second_client(&app, &cache, sh1, sh2).await;
    bulk_page_leaves_wallet_slot_empty(&app, &cache, sh1, sh2).await;
    untrusted_header_is_ignored(Arc::clone(&q), sh1).await;
    template_cached_until_tip_moves(q, prev, h2.hash).await;
    template_missing_tip_or_knob().await;
    let _ = std::fs::remove_dir_all(&dir);
}
