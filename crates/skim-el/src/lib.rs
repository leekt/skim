//! Execution-layer P2P for skim (PROJECT.md §3.3, §3.4), built on reth's standalone network.
//!
//! [`ElNet`] keeps a table of live sessions and sends **peer-targeted, timed** requests, so
//! callers can measure per-peer latency and availability instead of going through reth's
//! peer-selecting fetch client.

pub mod book;
pub mod serve;
mod snap1;

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use alloy_consensus::Header;
use alloy_primitives::{B256, U256};
use futures::StreamExt;
use parking_lot::RwLock;
use reth_chainspec::{ChainSpec, Head, SEPOLIA};
use reth_eth_wire_types::{
    snap::{
        AccountRangeMessage, ByteCodesMessage, GetAccountRangeMessage, GetByteCodesMessage, GetStorageRangesMessage, RangeBound,
        SnapProtocolMessage, StorageRangesMessage,
    },
    BlockAccessLists, BlockHashOrNumber, BlockRangeUpdate, GetBlockAccessLists, GetBlockBodies, GetBlockHeaders,
    GetReceipts70, HeadersDirection,
};
use reth_network::{
    config::rng_secret_key, p2p::snap::client::SnapResponse, NetworkConfig, NetworkHandle,
    NetworkManager, NetworkSyncUpdater, PeersInfo,
};
use reth_network_api::{
    events::{NetworkEvent, NetworkEventListenerProvider, PeerEvent, PeerRequest},
    PeerId,
};
use reth_dns_discovery::{tree::LinkEntry, DnsDiscoveryConfig};
use reth_network::PeersConfig;
use reth_storage_api::noop::NoopProvider;
use tokio::sync::oneshot;

pub use reth_network_api::PeerId as Peer;
pub use serve::{ServeCounts, ServeSource};

/// Fuzzing entry: decodes a raw snap/1 response as skim does for Geth peers (never panics).
#[doc(hidden)]
pub fn fuzz_snap1_response(raw: &[u8]) -> bool {
    snap1::fuzz_decode(raw)
}

/// A signed transaction as gossiped on the EL (no blob sidecars).
pub type SignedTx = alloy_consensus::EthereumTxEnvelope<alloy_consensus::TxEip4844>;
/// A receipt as served by `GetReceipts` (the bloom is recomputed locally).
pub type Receipt = alloy_consensus::EthereumReceipt;
pub type BlockBody = alloy_consensus::BlockBody<SignedTx>;
pub use reth_network_peers::TrustedPeer;

/// EIP-1459 DNS tree for Sepolia (same signing key as geth's built-in ethdisco trees).
const SEPOLIA_DNS: &str =
    "enrtree://AKA3AM6LPBYEUDMVNU3BSVQJ5AD45Y7YPOHJLEF6W26QOE4VTUDPE@all.sepolia.ethdisco.net";

#[derive(Debug, thiserror::Error)]
pub enum ReqError {
    #[error("timed out")]
    Timeout,
    #[error("peer request failed: {0}")]
    Peer(String),
    #[error("unexpected response type")]
    Unexpected,
}

/// What we know about a connected peer.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    /// snap/1 negotiated (via skim's satellite protocol); filled in by [`ElNet::peers`].
    pub snap1: bool,
    pub client: String,
    pub remote_addr: std::net::SocketAddr,
    pub eth_version: u8,
    pub snap2: bool,
    pub status_head: B256,
    pub status_latest: Option<u64>,
    /// The oldest block the peer serves bodies and receipts for (eth/69+ Status): history
    /// before it has expired on that peer.
    pub status_earliest: Option<u64>,
    pub connected_at: Instant,
}

/// A response plus how long the round trip took.
pub type Timed<T> = (Result<T, ReqError>, Duration);

#[derive(Clone)]
pub struct ElNet {
    handle: NetworkHandle,
    head: Arc<RwLock<Option<Header>>>,
    snap1: snap1::Snap1Peers,
    peers: Arc<RwLock<HashMap<PeerId, PeerInfo>>>,
    timeout: Duration,
    serve: serve::Source,
    serve_stats: Arc<serve::ServeStats>,
    /// What each peer turned out to be worth (sessions, SNAP, drops); ranks the saved list.
    book: Arc<RwLock<book::PeerBook>>,
    /// Set once the owner reports its own head ([`Self::set_head`]); the poller then stops.
    external_head: Arc<std::sync::atomic::AtomicBool>,
}

impl ElNet {
    /// Starts a Sepolia network with snap/2 advertised and the session tracker running.
    /// `trusted` peers are kept connected (the pinned state-provider pool, PROJECT.md §6.3.1);
    /// with `trusted_only`, no other peers are dialed or accepted.
    /// Must be called inside a Tokio runtime.
    pub async fn start_sepolia(
        listen_port: u16,
        trusted: Vec<TrustedPeer>,
        trusted_only: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::start_sepolia_with_key(listen_port, trusted, trusted_only, None).await
    }

    /// [`Self::start_sepolia`] with a fixed node key (32 secret bytes), so a restarted node keeps
    /// its identity and peers recognise it. `None` generates a fresh key.
    pub async fn start_sepolia_with_key(
        listen_port: u16,
        trusted: Vec<TrustedPeer>,
        trusted_only: bool,
        node_key: Option<[u8; 32]>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let secret = match node_key {
            Some(k) => reth_network::config::SecretKey::from_byte_array(&k)?,
            None => rng_secret_key(),
        };
        let spec: Arc<ChainSpec> = SEPOLIA.clone();
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        // Advertise a current fork id (Amsterdam) while claiming no synced chain beyond genesis.
        let head = Head {
            hash: spec.genesis_hash(),
            number: 0,
            timestamp: now,
            difficulty: U256::ZERO,
            total_difficulty: U256::ZERO,
        };
        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], listen_port));
        let snap1 = snap1::Snap1Peers::default();
        let config = NetworkConfig::builder(secret, reth_tasks::Runtime::test())
            .sepolia_boot_nodes()
            .listener_addr(addr)
            .discovery_addr(addr)
            .dns_discovery(DnsDiscoveryConfig {
                bootstrap_dns_networks: Some([SEPOLIA_DNS.parse::<LinkEntry>()?].into()),
                ..Default::default()
            })
            .peer_config(
                PeersConfig::default().with_trusted_nodes(trusted).with_trusted_nodes_only(trusted_only),
            )
            .with_snap(true)
            .add_rlpx_sub_protocol(snap1::Snap1Handler { peers: snap1.clone() })
            .disable_tx_gossip(true)
            .set_head(head)
            .build(NoopProvider::eth(spec));
        let mut manager = NetworkManager::eth(config).await?;
        // Peers' requests come to skim's handler (answered from `serve`, or empty until it is set).
        let (req_tx, req_rx) = tokio::sync::mpsc::channel(1024);
        manager.set_eth_request_handler(req_tx);
        let handle = manager.handle().clone();
        tokio::spawn(manager);

        let net = Self {
            handle,
            head: Default::default(),
            snap1,
            peers: Default::default(),
            timeout: Duration::from_secs(5),
            serve: Default::default(),
            serve_stats: Default::default(),
            external_head: Default::default(),
            book: Default::default(),
        };
        tokio::spawn(serve::run(req_rx, net.serve.clone(), net.serve_stats.clone()));
        net.spawn_tracker();
        net.spawn_head_follower();
        Ok(net)
    }

    fn spawn_tracker(&self) {
        let peers = self.peers.clone();
        let (handle, book) = (self.handle.clone(), self.book.clone());
        book.write().start(Instant::now());
        let mut events = self.handle.event_listener();
        tokio::spawn(async move {
            while let Some(ev) = events.next().await {
                match ev {
                    NetworkEvent::ActivePeerSession { info, .. } => {
                        let snap2 = info
                            .capabilities
                            .capabilities()
                            .iter()
                            .any(|c| c.name == "snap" && c.version == 2);
                        let caps: Vec<String> = info
                            .capabilities
                            .capabilities()
                            .iter()
                            .map(|c| format!("{}/{}", c.name, c.version))
                            .collect();
                        tracing::info!(peer = %info.peer_id, client = %info.client_version,
                            eth = %info.version, snap2, caps = caps.join(","), "session");
                        let snap = caps.iter().any(|c| c.starts_with("snap/"));
                        book.write().on_session(info.peer_id, &info.client_version, snap, Instant::now());
                        if info.version as u8 >= 69 && !info.client_version.starts_with("enrscout") {
                            // Only outgoing sessions show the peer's listen address.
                            let (handle, book, id) = (handle.clone(), book.clone(), info.peer_id);
                            tokio::spawn(async move {
                                use reth_network_api::Peers;
                                if let Ok(Some(p)) = handle.get_peer_by_id(id).await
                                    && !p.direction.is_incoming()
                                {
                                    book.write().on_addr(id, p.remote_addr);
                                }
                            });
                        }
                        peers.write().insert(
                            info.peer_id,
                            PeerInfo {
                                snap1: false,
                                client: info.client_version.to_string(),
                                remote_addr: info.remote_addr,
                                eth_version: info.version as u8,
                                snap2,
                                status_head: info.status.blockhash,
                                status_latest: info.status.latest_block,
                                status_earliest: info.status.earliest_block,
                                connected_at: Instant::now(),
                            },
                        );
                    }
                    NetworkEvent::Peer(PeerEvent::SessionClosed { peer_id, reason }) => {
                        if let Some(p) = peers.write().remove(&peer_id) {
                            tracing::info!(peer = %peer_id, client = %p.client, ?reason,
                                up_s = p.connected_at.elapsed().as_secs(), "session closed");
                            let why = reason.map(|r| format!("{r:?}"));
                            book.write().on_close(peer_id, p.connected_at.elapsed(), why.as_deref());
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    /// Polls peers for headers past our head (round-robin, every 500 ms), advertises each new
    /// head, and keeps the latest. Probe-level head selection: highest number with a parent
    /// link to the previous head, re-anchoring on a gap. The real chain tracker keeps a DAG (§4).
    fn spawn_head_follower(&self) {
        let net = self.clone();
        tokio::spawn(async move {
            let mut rr = 0usize;
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if net.external_head.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let peers: Vec<_> = net.peers().into_iter().filter(|(_, p)| p.eth_version >= 69).collect();
                if peers.is_empty() {
                    continue;
                }
                rr = rr.wrapping_add(1);
                let (peer, info) = &peers[rr % peers.len()];
                let cur = net.head.read().clone();
                let start = match &cur {
                    Some(h) => h.number + 1,
                    None => match peers.iter().filter_map(|(_, p)| p.status_latest).max().or(info.status_latest) {
                        Some(n) => n,
                        None => continue,
                    },
                };
                let Ok(hs) = net.headers(*peer, BlockHashOrNumber::Number(start), 32).await.0 else { continue };
                let Some(last) = hs.last().cloned() else { continue };
                let linked = cur.as_ref().is_none_or(|c| hs[0].parent_hash == c.hash_slow());
                if linked && hs.windows(2).all(|w| w[1].parent_hash == w[0].hash_slow()) {
                    net.advertise_head(&last);
                    *net.head.write() = Some(last);
                } else {
                    *net.head.write() = None; // gap or reorg: re-anchor next tick
                }
            }
        });
    }

    /// Reports the owner's verified head (e.g. the block it just applied): it is advertised to
    /// peers from now on instead of the head polled from peers, so the advertised range is what
    /// skim actually holds.
    pub fn set_head(&self, h: &Header) {
        self.external_head.store(true, std::sync::atomic::Ordering::Relaxed);
        self.advertise_head(h);
        *self.head.write() = Some(h.clone());
    }

    /// Latest followed head, if any.
    pub fn head(&self) -> Option<Header> {
        self.head.read().clone()
    }

    /// This node's enode URL.
    pub fn enode(&self) -> String {
        self.handle.local_node_record().to_string()
    }

    pub fn handle(&self) -> &NetworkHandle {
        &self.handle
    }

    /// Answers peers' requests from `src` from now on.
    pub fn serve(&self, src: Arc<dyn ServeSource>) {
        *self.serve.write() = Some(src);
    }

    /// Requests answered so far.
    pub fn serve_counts(&self) -> ServeCounts {
        self.serve_stats.counts()
    }

    /// Advertises `h` as our head: the eth Status for new handshakes and a `BlockRangeUpdate`
    /// for live eth/69+ sessions. The range starts at the oldest block the serve source holds
    /// without gaps (`[h, h]` without one). Peers that evict "useless" peers see a synced node
    /// instead of one stuck at genesis (§6.3.1).
    pub fn advertise_head(&self, h: &Header) {
        let hash = h.hash_slow();
        self.handle.update_status(Head {
            hash,
            number: h.number,
            timestamp: h.timestamp,
            difficulty: U256::ZERO,
            total_difficulty: U256::ZERO,
        });
        let earliest = self.serve.read().as_ref().and_then(|s| s.earliest()).map_or(h.number, |e| e.min(h.number));
        self.handle.update_block_range(BlockRangeUpdate {
            earliest,
            latest: h.number,
            latest_hash: hash,
        });
    }

    pub fn set_timeout(&mut self, t: Duration) {
        self.timeout = t;
    }

    pub fn peers(&self) -> Vec<(PeerId, PeerInfo)> {
        self.peers
            .read()
            .iter()
            .map(|(k, v)| {
                let mut v = v.clone();
                v.snap1 = !v.snap2 && self.snap1.contains(k);
                (*k, v)
            })
            .collect()
    }

    /// Sends a SNAP request over whichever version the peer negotiated.
    async fn snap_any(&self, peer: PeerId, request: SnapProtocolMessage) -> Timed<SnapResponse> {
        let snap2 = self.peers.read().get(&peer).is_some_and(|p| p.snap2);
        if snap2 {
            return self.snap(peer, request).await;
        }
        let req = match request {
            SnapProtocolMessage::GetAccountRange(m) => snap1::Snap1Request::AccountRange(m),
            SnapProtocolMessage::GetStorageRanges(m) => snap1::Snap1Request::StorageRanges(m),
            SnapProtocolMessage::GetByteCodes(m) => snap1::Snap1Request::ByteCodes(m),
            _ => return (Err(ReqError::Unexpected), Duration::ZERO),
        };
        let start = Instant::now();
        let Some(rx) = self.snap1.send(peer, req) else {
            return (Err(ReqError::Peer("response channel dropped: no snap session".into())), Duration::ZERO);
        };
        let res = match tokio::time::timeout(self.timeout, rx).await {
            Err(_) => Err(ReqError::Timeout),
            Ok(Err(_)) => Err(ReqError::Peer("response channel dropped".into())),
            Ok(Ok(r)) => Ok(match r {
                snap1::Snap1Response::AccountRange(m) => SnapResponse::AccountRange(m),
                snap1::Snap1Response::StorageRanges(m) => SnapResponse::StorageRanges(m),
                snap1::Snap1Response::ByteCodes(m) => SnapResponse::ByteCodes(m),
            }),
        };
        (res, start.elapsed())
    }

    pub fn num_connected(&self) -> usize {
        self.handle.num_connected_peers()
    }

    async fn send<T>(
        &self,
        peer: PeerId,
        make: impl FnOnce(oneshot::Sender<reth_network::p2p::error::RequestResult<T>>) -> PeerRequest,
    ) -> Timed<T> {
        let (tx, rx) = oneshot::channel();
        let start = Instant::now();
        self.handle.send_request(peer, make(tx));
        let res = match tokio::time::timeout(self.timeout, rx).await {
            Err(_) => Err(ReqError::Timeout),
            Ok(Err(_)) => Err(ReqError::Peer("response channel dropped".into())),
            Ok(Ok(Err(e))) => Err(ReqError::Peer(e.to_string())),
            Ok(Ok(Ok(v))) => Ok(v),
        };
        (res, start.elapsed())
    }

    pub async fn headers(
        &self,
        peer: PeerId,
        start: BlockHashOrNumber,
        limit: u64,
    ) -> Timed<Vec<Header>> {
        let request = GetBlockHeaders { start_block: start, limit, skip: 0, direction: HeadersDirection::Rising };
        let (r, d) = self.send(peer, |response| PeerRequest::GetBlockHeaders { request, response }).await;
        (r.map(|h| h.0), d)
    }

    /// eth/71 `GetBlockAccessLists`. `None` entries are "unavailable" (`0x80`).
    pub async fn bals(&self, peer: PeerId, hashes: Vec<B256>) -> Timed<BlockAccessLists> {
        let request = GetBlockAccessLists(hashes);
        self.send(peer, |response| PeerRequest::GetBlockAccessLists { request, response }).await
    }

    async fn snap(&self, peer: PeerId, request: SnapProtocolMessage) -> Timed<SnapResponse> {
        self.send(peer, |response| PeerRequest::GetSnap { request, response }).await
    }

    /// Narrow `GetAccountRange` starting at `account_hash` (a point lookup in practice).
    pub async fn account_range(
        &self,
        peer: PeerId,
        root: B256,
        account_hash: B256,
        response_bytes: u64,
    ) -> Timed<AccountRangeMessage> {
        let msg = SnapProtocolMessage::GetAccountRange(GetAccountRangeMessage {
            request_id: 0,
            root_hash: root,
            starting_hash: account_hash,
            limit_hash: account_hash,
            response_bytes,
        });
        match self.snap_any(peer, msg).await {
            (Ok(SnapResponse::AccountRange(m)), d) => (Ok(m), d),
            (Ok(_), d) => (Err(ReqError::Unexpected), d),
            (Err(e), d) => (Err(e), d),
        }
    }

    /// `GetStorageRanges` for one account from `start` to the end of its trie (one page of up to
    /// `response_bytes`; a non-empty proof means the page is partial).
    pub async fn storage_range_from(
        &self,
        peer: PeerId,
        root: B256,
        account_hash: B256,
        start: B256,
        response_bytes: u64,
    ) -> Timed<StorageRangesMessage> {
        let msg = SnapProtocolMessage::GetStorageRanges(GetStorageRangesMessage {
            request_id: 0,
            root_hash: root,
            account_hashes: vec![account_hash],
            starting_hash: RangeBound::from(start),
            limit_hash: RangeBound::from(B256::repeat_byte(0xff)),
            response_bytes,
        });
        match self.snap_any(peer, msg).await {
            (Ok(SnapResponse::StorageRanges(m)), d) => (Ok(m), d),
            (Ok(_), d) => (Err(ReqError::Unexpected), d),
            (Err(e), d) => (Err(e), d),
        }
    }

    /// Narrow `GetStorageRanges` for one account starting at `slot_hash`.
    pub async fn storage_range(
        &self,
        peer: PeerId,
        root: B256,
        account_hash: B256,
        slot_hash: B256,
        response_bytes: u64,
    ) -> Timed<StorageRangesMessage> {
        let msg = SnapProtocolMessage::GetStorageRanges(GetStorageRangesMessage {
            request_id: 0,
            root_hash: root,
            account_hashes: vec![account_hash],
            starting_hash: RangeBound::from(slot_hash),
            limit_hash: RangeBound::from(slot_hash),
            response_bytes,
        });
        match self.snap_any(peer, msg).await {
            (Ok(SnapResponse::StorageRanges(m)), d) => (Ok(m), d),
            (Ok(_), d) => (Err(ReqError::Unexpected), d),
            (Err(e), d) => (Err(e), d),
        }
    }

    /// `GetByteCodes` for the given code hashes.
    pub async fn byte_codes(
        &self,
        peer: PeerId,
        hashes: Vec<B256>,
        response_bytes: u64,
    ) -> Timed<ByteCodesMessage> {
        let msg = SnapProtocolMessage::GetByteCodes(GetByteCodesMessage {
            request_id: 0,
            hashes,
            response_bytes,
        });
        match self.snap_any(peer, msg).await {
            (Ok(SnapResponse::ByteCodes(m)), d) => (Ok(m), d),
            (Ok(_), d) => (Err(ReqError::Unexpected), d),
            (Err(e), d) => (Err(e), d),
        }
    }

    /// Headers walking towards genesis from `start` (inclusive).
    pub async fn headers_falling(&self, peer: PeerId, start: BlockHashOrNumber, limit: u64) -> Timed<Vec<Header>> {
        let request = GetBlockHeaders { start_block: start, limit, skip: 0, direction: HeadersDirection::Falling };
        let (r, d) = self.send(peer, |response| PeerRequest::GetBlockHeaders { request, response }).await;
        (r.map(|h| h.0), d)
    }

    /// `GetBlockBodies`. Callers verify each body against its header's `transactions_root`.
    pub async fn bodies(&self, peer: PeerId, hashes: Vec<B256>) -> Timed<Vec<BlockBody>> {
        let request = GetBlockBodies(hashes);
        let (r, d) = self.send(peer, |response| PeerRequest::GetBlockBodies { request, response }).await;
        (r.map(|b| b.0), d)
    }

    /// All receipts of one block (eth/70+ `GetReceipts`, following `lastBlockIncomplete`).
    /// Callers verify them against the header's `receipts_root`.
    pub async fn block_receipts(&self, peer: PeerId, hash: B256) -> Timed<Vec<Receipt>> {
        let start = Instant::now();
        let mut out = Vec::new();
        for _ in 0..64 {
            let request = GetReceipts70 { first_block_receipt_index: out.len() as u64, block_hashes: vec![hash] };
            let (r, _) = self.send(peer, |response| PeerRequest::GetReceipts70 { request, response }).await;
            match r {
                Ok(mut m) => {
                    let got = m.receipts.pop().unwrap_or_default();
                    let empty = got.is_empty();
                    out.extend(got);
                    if !m.last_block_incomplete || empty {
                        return (Ok(out), start.elapsed());
                    }
                }
                Err(e) => return (Err(e), start.elapsed()),
            }
        }
        (Err(ReqError::Peer("receipts: too many partial responses".into())), start.elapsed())
    }

    /// Receipts of several blocks in one `GetReceipts` exchange (continuing partial responses).
    /// Returns receipts for a prefix of `hashes`, in order: a peer may stop early (response size
    /// limits, or blocks it doesn't have). Callers verify each against its `receipts_root`.
    pub async fn blocks_receipts(&self, peer: PeerId, hashes: &[B256]) -> Timed<Vec<Vec<Receipt>>> {
        let start = Instant::now();
        let mut out: Vec<Vec<Receipt>> = Vec::new();
        let mut cur: Vec<Receipt> = Vec::new();
        for _ in 0..256 {
            if out.len() == hashes.len() {
                break;
            }
            let request = GetReceipts70 { first_block_receipt_index: cur.len() as u64, block_hashes: hashes[out.len()..].to_vec() };
            let (r, _) = self.send(peer, |response| PeerRequest::GetReceipts70 { request, response }).await;
            let m = match r {
                Ok(m) => m,
                Err(e) if out.is_empty() => return (Err(e), start.elapsed()),
                Err(_) => break,
            };
            if m.receipts.is_empty() {
                break;
            }
            let k = m.receipts.len();
            let mut progressed = false;
            for (i, r) in m.receipts.into_iter().enumerate() {
                progressed |= !r.is_empty() || i > 0;
                if i == 0 {
                    cur.extend(r);
                } else {
                    cur = r;
                }
                if i + 1 < k || !m.last_block_incomplete {
                    out.push(std::mem::take(&mut cur));
                }
            }
            if !progressed && m.last_block_incomplete {
                break;
            }
        }
        (Ok(out), start.elapsed())
    }

    /// Sends full transactions to every connected peer. Returns how many peers got them.
    pub fn broadcast(&self, txs: Vec<SignedTx>) -> usize {
        let txs: Vec<Arc<SignedTx>> = txs.into_iter().map(Arc::new).collect();
        let peers: Vec<PeerId> = self.peers.read().keys().copied().collect();
        for p in &peers {
            self.handle.send_transactions(*p, txs.clone());
        }
        peers.len()
    }

    /// Connected peers worth reconnecting to after a restart (real clients on eth/69+):
    /// `(node id, address)`.
    /// Peers worth saving for the next start, best first: SNAP peers that held a session,
    /// then other SNAP peers, then the rest; crawlers and peers that only dropped us are out.
    pub fn good_peers(&self) -> Vec<(PeerId, std::net::SocketAddr)> {
        self.book.read().ranked().into_iter().map(|(id, a, _)| (id, a)).collect()
    }

    /// Peers seen this run that are not worth a saved slot.
    pub fn cooled_peers(&self) -> Vec<PeerId> {
        self.book.read().cooled()
    }

    /// Peer acquisition so far: sessions by client, close reasons, time to the first SNAP peer.
    pub fn peer_book(&self) -> book::BookSummary {
        self.book.read().summary()
    }

    /// Dials previously good peers (their addresses may be stale; failed dials are harmless).
    pub fn add_peers(&self, peers: &[(PeerId, std::net::SocketAddr)]) {
        use reth_network_api::Peers;
        for (id, addr) in peers {
            self.handle.add_peer(*id, *addr);
        }
    }
}
