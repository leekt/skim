use super::*;

const WS_EMITTER: Address = Address::repeat_byte(0xe5);

fn ws_block(n: u64, parent: B256, tag: u8) -> (Header, Vec<skim_el::Receipt>, B256) {
    let log = alloy_primitives::Log::new_unchecked(WS_EMITTER, vec![B256::repeat_byte(0x70)], Bytes::from(vec![tag, n as u8]));
    let mut bloom = alloy_primitives::Bloom::default();
    bloom.accrue_log(&log);
    let header = Header {
        number: n,
        parent_hash: parent,
        logs_bloom: bloom,
        receipts_root: B256::repeat_byte(1),
        transactions_root: B256::repeat_byte(2),
        timestamp: n * 12,
        extra_data: vec![tag].into(),
        ..Default::default()
    };
    let receipts = vec![skim_el::Receipt { success: true, cumulative_gas_used: 21_000, logs: vec![log], ..Default::default() }];
    (header, receipts, B256::repeat_byte(tag) ^ B256::from(U256::from(n)))
}

fn ws_append(rpc: &Rpc, n: u64, tag: u8, with_receipts: bool) -> BlockRef {
    let parent = rpc.sim.store.head().map_or(B256::ZERO, |b| b.hash);
    let (header, receipts, tx) = ws_block(n, parent, tag);
    let b = crate::chain::block_ref(&header);
    rpc.sim.insert_header(header.clone());
    rpc.sim.store.apply_block(b, Default::default()).unwrap();
    let chain = rpc.chain().unwrap();
    chain.note_header(&header);
    if with_receipts {
        chain.note_receipts(n, b.hash, receipts, vec![tx]);
    }
    b
}

struct WsTest {
    rpc: Arc<Rpc>,
    socket: reqwest::Upgraded,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Drop for WsTest {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn test_rpc() -> Arc<Rpc> {
    let net = skim_el::ElNet::start_sepolia(0, vec![], true).await.unwrap();
    let sim = Simulator::new(
        skim_state::Store::new(256),
        skim_snap::Scheduler::new(skim_snap::el::ElTransport::new(net), Default::default()),
        skim_sim::predictor::Predictor {
            learned: skim_sim::learned::Learned::new(8, 0.5, 256),
            v3: vec![],
            base_words: 2,
            reach: Default::default(),
        },
    );
    Arc::new(Rpc {
        sim,
        subs: None,
        chain: Some(ChainData::offline(2048)),
        light: None,
        require_verified: false,
        events: tokio::sync::broadcast::channel(256).0,
        call_cache: Default::default(),
        cache_calls: true,
        dash: Arc::new(Default::default()),
        cl_stats: None,
    })
}

/// A server with `cors`, and a POST helper sending `Origin` when given.
async fn origin_server(cors: Option<&str>) -> (std::net::SocketAddr, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (addr, tokio::spawn(serve(test_rpc().await, listener, cors.map(String::from))))
}

async fn post_from(addr: std::net::SocketAddr, origin: Option<&str>, method: &str) -> Value {
    let mut req = reqwest::Client::new().post(format!("http://{addr}/")).json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": []}));
    if let Some(o) = origin {
        req = req.header("origin", o);
    }
    req.send().await.unwrap().json().await.unwrap()
}

async fn upgrade_from(addr: std::net::SocketAddr, origin: Option<&str>) -> reqwest::StatusCode {
    let mut req = reqwest::Client::new()
        .get(format!("http://{addr}/"))
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");
    if let Some(o) = origin {
        req = req.header("origin", o);
    }
    req.send().await.unwrap().status()
}

#[tokio::test(flavor = "multi_thread")]
async fn web_pages_get_skim_methods_only_from_an_origin_cors_names() {
    // Review R6: with the default `--cors *`, any web page the user opened could read
    // skim_status / skim_dashboard (watched addresses, subscription names, peer ids) from a
    // local node, over HTTP or a WebSocket (browsers apply no CORS to WebSocket upgrades).
    let refused = |r: &Value| r["error"]["code"] == -32004 && r["error"]["message"].as_str().is_some_and(|m| m.contains("web pages"));
    let (addr, server) = origin_server(Some("*")).await;
    let evil = Some("https://evil.example");
    assert!(refused(&post_from(addr, evil, "skim_status").await));
    assert!(refused(&post_from(addr, evil, "skim_dashboard").await));
    assert_eq!(post_from(addr, evil, "eth_chainId").await["result"], "0xaa36a7", "dapps keep eth_* under --cors *");
    assert!(!refused(&post_from(addr, None, "skim_status").await), "no Origin: curl, servers, viem in Node");
    assert_eq!(upgrade_from(addr, evil).await, reqwest::StatusCode::SWITCHING_PROTOCOLS, "--cors * allows the WebSocket");
    server.abort();
    // An origin named exactly is trusted with everything; others can't even open a WebSocket.
    let (addr, server) = origin_server(Some("https://app.example")).await;
    assert!(!refused(&post_from(addr, Some("https://app.example"), "skim_status").await));
    assert!(refused(&post_from(addr, evil, "skim_status").await));
    assert_eq!(upgrade_from(addr, evil).await, reqwest::StatusCode::FORBIDDEN);
    assert_eq!(upgrade_from(addr, Some("https://app.example")).await, reqwest::StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(upgrade_from(addr, None).await, reqwest::StatusCode::SWITCHING_PROTOCOLS);
    server.abort();
    // --cors none: no page at all over WebSocket.
    let (addr, server) = origin_server(None).await;
    assert_eq!(upgrade_from(addr, evil).await, reqwest::StatusCode::FORBIDDEN);
    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_websocket_cannot_subscribe_to_skim_subscriptions() {
    let (addr, server) = origin_server(Some("*")).await;
    let mut ws = WsTest::connect(addr, server, test_rpc().await, Some("https://evil.example")).await;
    ws.send(json!({"jsonrpc": "2.0", "id": 1, "method": "eth_subscribe", "params": ["skim_subscriptions"]})).await;
    assert_eq!(ws.recv().await["error"]["code"], -32004);
    ws.send(json!({"jsonrpc": "2.0", "id": 2, "method": "skim_status", "params": []})).await;
    assert_eq!(ws.recv().await["error"]["code"], -32004);
}

impl WsTest {
    async fn connect(addr: std::net::SocketAddr, server: tokio::task::JoinHandle<anyhow::Result<()>>, rpc: Arc<Rpc>, origin: Option<&str>) -> Self {
        let mut req = reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");
        if let Some(o) = origin {
            req = req.header("origin", o);
        }
        let response = req.send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::SWITCHING_PROTOCOLS);
        Self { rpc, socket: response.upgrade().await.unwrap(), server }
    }

    async fn start() -> Self {
        let net = skim_el::ElNet::start_sepolia(0, vec![], true).await.unwrap();
        let sim = Simulator::new(
            skim_state::Store::new(256),
            skim_snap::Scheduler::new(skim_snap::el::ElTransport::new(net), Default::default()),
            skim_sim::predictor::Predictor {
                learned: skim_sim::learned::Learned::new(8, 0.5, 256),
                v3: vec![],
                base_words: 2,
                reach: Default::default(),
            },
        );
        let rpc = Arc::new(Rpc {
            sim,
            subs: None,
            chain: Some(ChainData::offline(2048)),
            light: None,
            require_verified: false,
            events: tokio::sync::broadcast::channel(256).0,
            call_cache: Default::default(),
            cache_calls: true,
            dash: Arc::new(Default::default()),
            cl_stats: None,
        });
        ws_append(&rpc, 99, 0xa, true);
        ws_append(&rpc, 100, 0xa, true);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve(rpc.clone(), listener, None));
        let response = reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::SWITCHING_PROTOCOLS);
        Self { rpc, socket: response.upgrade().await.unwrap(), server }
    }

    // A small RFC 6455 client keeps these transport tests on existing dependencies. Client
    // frames are masked; these tests exchange small, unfragmented JSON text messages.
    async fn send(&mut self, value: Value) {
        use tokio::io::AsyncWriteExt;
        let payload = value.to_string().into_bytes();
        let mut frame = vec![0x81];
        match payload.len() {
            n @ 0..=125 => frame.push(0x80 | n as u8),
            n => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(n as u16).to_be_bytes());
            }
        }
        let mask = [1, 2, 3, 4];
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.socket.write_all(&frame).await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        use tokio::io::AsyncReadExt;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut header = [0; 2];
            self.socket.read_exact(&mut header).await.unwrap();
            assert_eq!(header[0], 0x81, "expected one text frame");
            assert_eq!(header[1] & 0x80, 0, "server frames are unmasked");
            let len = match header[1] & 0x7f {
                126 => self.socket.read_u16().await.unwrap() as usize,
                127 => self.socket.read_u64().await.unwrap() as usize,
                n => n as usize,
            };
            let mut payload = vec![0; len];
            self.socket.read_exact(&mut payload).await.unwrap();
            serde_json::from_slice(&payload).unwrap()
        }).await.expect("WebSocket delivery timed out")
    }

    async fn expect_log(&mut self, number: u64, hash: B256, removed: bool) {
        let push = self.recv().await;
        assert_eq!(push["method"], "eth_subscription", "{push}");
        let log = &push["params"]["result"];
        assert_eq!(log["blockNumber"], format!("{number:#x}"), "{push}");
        assert_eq!(log["blockHash"], json!(hash), "{push}");
        assert_eq!(log["removed"], removed, "{push}");
    }

    fn publish(&self, b: BlockRef) {
        self.rpc.events.send(WsEvent::NewHead(b)).unwrap();
    }

    fn rollback(&self, number: u64) {
        self.rpc.sim.store.note_reorg();
        self.rpc.chain().unwrap().rollback_to(number);
        self.rpc.sim.store.rollback_to(number);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_first_announcement_after_subscribing_delivers_the_whole_gap() {
    let mut ws = WsTest::start().await;
    ws.send(json!({"jsonrpc": "2.0", "id": 1, "method": "eth_subscribe", "params": ["logs", {"address": WS_EMITTER}]})).await;
    assert_eq!(ws.recv().await["result"], "0x1");
    let blocks: Vec<_> = (101..=104).map(|n| ws_append(&ws.rpc, n, 0xa, true)).collect();
    ws.publish(blocks[3]);
    for b in blocks {
        ws.expect_log(b.number, b.hash, false).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_logs_deliver_subscription_replacements_gaps_and_retries() {
    let mut ws = WsTest::start().await;
    ws.send(json!({"jsonrpc": "2.0", "id": 1, "method": "eth_subscribe", "params": ["logs", {"address": WS_EMITTER}]})).await;
    assert_eq!(ws.recv().await["result"], "0x1");

    // A sibling of the subscription's initial tip is a new block, even at that height.
    ws.rollback(99);
    let b100 = ws_append(&ws.rpc, 100, 0xb, true);
    ws.publish(b100);
    ws.expect_log(100, b100.hash, false).await;

    // Only the last block of a recovered gap is announced: every intermediate log arrives.
    let mut branch = vec![b100];
    for n in 101..=104 {
        branch.push(ws_append(&ws.rpc, n, 0xb, true));
    }
    ws.publish(branch[4]);
    for b in &branch[1..] {
        ws.expect_log(b.number, b.hash, false).await;
    }

    // A receipt fetch really fails before peers make it available. The next announcement
    // retries that block before delivering anything later.
    let failed = Arc::new(tokio::sync::Notify::new());
    let signal = failed.clone();
    ws.rpc.chain().unwrap().test_hooks.lock().insert("ws_delivery_failed", Box::new(move || signal.notify_one()));
    let b105 = ws_append(&ws.rpc, 105, 0xb, false);
    ws.publish(b105);
    tokio::time::timeout(std::time::Duration::from_secs(5), failed.notified()).await.unwrap();
    let (_, receipts, tx) = ws_block(105, b105.parent_hash, 0xb);
    ws.rpc.chain().unwrap().note_receipts(105, b105.hash, receipts, vec![tx]);
    let b106 = ws_append(&ws.rpc, 106, 0xb, true);
    ws.publish(b106);
    ws.expect_log(105, b105.hash, false).await;
    ws.expect_log(106, b106.hash, false).await;
    branch.extend([b105, b106]);

    // Replacing every remembered block must remove the old logs and then deliver the
    // entire replacement branch, including the subscription's original height, even if
    // fetching that first replacement fails once.
    ws.rollback(99);
    let failed = Arc::new(tokio::sync::Notify::new());
    let signal = failed.clone();
    ws.rpc.chain().unwrap().test_hooks.lock().insert("ws_delivery_failed", Box::new(move || signal.notify_one()));
    let mut replacement: Vec<_> = (100..=107).map(|n| ws_append(&ws.rpc, n, 0xc, n != 100)).collect();
    ws.publish(replacement[7]);
    for b in branch.iter().rev() {
        ws.expect_log(b.number, b.hash, true).await;
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), failed.notified()).await.unwrap();
    let (_, receipts, tx) = ws_block(100, replacement[0].parent_hash, 0xc);
    ws.rpc.chain().unwrap().note_receipts(100, replacement[0].hash, receipts, vec![tx]);
    replacement.push(ws_append(&ws.rpc, 108, 0xc, true));
    ws.publish(replacement[8]);
    for b in replacement {
        ws.expect_log(b.number, b.hash, false).await;
    }
}

#[test]
fn the_call_cache_is_bounded_by_bytes_not_only_by_entries() {
    let mut c = CallCache::default();
    let big: R = Ok(json!("x".repeat(1 << 20))); // 1 MiB results
    for i in 0..200u64 {
        c.put((B256::from(alloy_primitives::U256::from(i)), B256::ZERO), &big, Arc::from(vec![]));
        assert!(c.bytes <= CallCache::MAX_BYTES + (2 << 20), "{} bytes after {i}", c.bytes);
    }
    assert!(c.map.len() < 40, "{} entries", c.map.len());
    assert_eq!(c.part().bytes, c.bytes);
    // Replacing an entry doesn't count it twice.
    let before = c.bytes;
    c.put((B256::from(alloy_primitives::U256::from(199u64)), B256::ZERO), &big, Arc::from(vec![]));
    assert_eq!(c.bytes, before);
}

#[test]
fn unknown_method_names_share_one_label_however_many_there_are() {
    // Review: names were only checked for their shape, so every distinct `eth_nonexistent_N`
    // got its own dashboard entry and metric series.
    let dash = crate::dashboard::Dashboard::default();
    let not_found: R = Err(RpcError::new(-32601, "method not found".to_string()));
    for i in 0..10_000 {
        dash.record_method(&method_label(&format!("eth_nonexistent_{i}"), &not_found), 1.0, false);
    }
    assert_eq!(dash.methods_json().len(), 1);
    assert_eq!(dash.methods_json()[0]["method"], "other");
    // A method skim implements keeps its label, even when it fails.
    let reverted: R = Err(RpcError::new(3, "execution reverted".to_string()));
    assert_eq!(method_label("eth_call", &reverted), "eth_call");
    assert_eq!(method_label("eth_blockNumber", &Ok(json!("0x1"))), "eth_blockNumber");
}

#[test]
fn unavailable_errors_say_why_and_whether_to_retry() {
    let data = |e: RpcError| (e.code, e.data.unwrap());
    let r = |reason: &str, retry: bool| (-32002, json!({"reason": reason, "retry": retry}));
    assert_eq!(data(SimError::NoStatePeers(60).into()), r("no_state_peers", true));
    assert_eq!(data(SimError::Incomplete(8).into()), r("state_incomplete", true));
    assert_eq!(data(SimError::State(StateError::OutOfWindow(5)).into()), r("outside_window", false));
    assert_eq!(data(SimError::NotArchived { block: 5, keys: 1 }.into()), r("not_archived", false));
    assert_eq!(data(DataError::Expired("old".into()).into()), r("outside_retention", false));
    assert_eq!(data(DataError::ChainChanged("reorg".into()).into()), r("chain_changed", true));
    assert_eq!(data(DataError::Unavailable("busy".into()).into()), r("not_served", true));
}

#[test]
fn a_request_spanning_a_reorg_is_refused_never_answered() {
    // Review: rollback, chain-data cleanup and the new block aren't one atomic step; a request
    // that ran across them may have mixed forks.
    let ok = || -> R { Ok(json!("0x1")) };
    assert_eq!(reorg_guard("eth_call", 4, 4, ok()).unwrap(), json!("0x1"), "no reorg: answered");
    let e = reorg_guard("eth_call", 4, 5, ok()).unwrap_err();
    assert_eq!((e.code, e.data.unwrap()["reason"].clone()), (-32002, json!("chain_changed")));
    assert!(reorg_guard("eth_getTransactionReceipt", 4, 6, ok()).is_err());
    // A broadcast already happened: reporting it as failed would be wrong.
    assert!(reorg_guard("eth_sendRawTransaction", 4, 5, ok()).is_ok());
    assert!(reorg_guard("eth_chainId", 4, 5, ok()).is_ok());
}

#[tokio::test]
async fn a_reorg_while_the_request_is_pending_is_seen() {
    // Review: the epoch to compare with was sampled before the request ran.
    use std::sync::atomic::{AtomicU64, Ordering};
    let epoch = AtomicU64::new(4);
    let req = async {
        tokio::task::yield_now().await;
        epoch.fetch_add(1, Ordering::SeqCst); // a rollback while receipts were being fetched
        Ok(json!("0x1"))
    };
    let e = guarded("eth_getTransactionReceipt", 4, req, || epoch.load(Ordering::SeqCst)).await.unwrap_err();
    assert_eq!(e.data.unwrap()["reason"], "chain_changed");
    let calm = async { Ok(json!("0x1")) };
    assert!(guarded("eth_call", 5, calm, || epoch.load(Ordering::SeqCst)).await.is_ok());
}