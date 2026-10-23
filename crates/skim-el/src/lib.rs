//! Execution-layer P2P for skim (PROJECT.md §3.3, §3.4), built on reth's standalone network.
//!
//! [`ElNet`] keeps a table of live sessions and sends **peer-targeted, timed** requests, so
//! callers can measure per-peer latency and availability instead of going through reth's
//! peer-selecting fetch client.

pub mod book;

/// Requests skim sent to peers, by kind (process-wide): what request coalescing and fetch
/// pipelining save is measured with these. Continuations of a partial response count again.
pub mod requests {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    pub static HEADERS: AtomicU64 = AtomicU64::new(0);
    pub static BODIES: AtomicU64 = AtomicU64::new(0);
    pub static RECEIPTS: AtomicU64 = AtomicU64::new(0);
    pub static BALS: AtomicU64 = AtomicU64::new(0);
    pub static SNAP_ACCOUNT: AtomicU64 = AtomicU64::new(0);
    pub static SNAP_STORAGE: AtomicU64 = AtomicU64::new(0);
    pub static SNAP_CODE: AtomicU64 = AtomicU64::new(0);
    pub static SNAP_OTHER: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn add(c: &AtomicU64) {
        c.fetch_add(1, Relaxed);
    }

    /// `(kind, count)` for every kind.
    pub fn counts() -> [(&'static str, u64); 8] {
        [
            ("headers", HEADERS.load(Relaxed)),
            ("bodies", BODIES.load(Relaxed)),
            ("receipts", RECEIPTS.load(Relaxed)),
            ("block_access_lists", BALS.load(Relaxed)),
            ("snap_account", SNAP_ACCOUNT.load(Relaxed)),
            ("snap_storage", SNAP_STORAGE.load(Relaxed)),
            ("snap_code", SNAP_CODE.load(Relaxed)),
            ("snap_other", SNAP_OTHER.load(Relaxed)),
        ]
    }
}
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

pub mod frame;
pub use frame::{FRAME_TX_TYPE, FrameReceipt, FrameResult, Receipt, SignedTx, SkimNetworkPrimitives};
/// A receipt as served by `GetReceipts` (the bloom is recomputed locally).
/// EL session limits. reth's defaults (100 outbound, 30 inbound) are for a full node; skim
/// reaches 30-40 sessions in practice, and each one holds buffers and peer state, so a ceiling
/// keeps that memory bounded (measured with `skim_status.memory`).
pub const MAX_OUTBOUND_PEERS: usize = 40;
pub const MAX_INBOUND_PEERS: usize = 15;

pub type BlockBody = alloy_consensus::BlockBody<SignedTx>;
pub use reth_network_peers::TrustedPeer;

/// EIP-1459 DNS tree for Sepolia (same signing key as geth's built-in ethdisco trees).
const SEPOLIA_DNS: &str =
    "enrtree://AKA3AM6LPBYEUDMVNU3BSVQJ5AD45Y7YPOHJLEF6W26QOE4VTUDPE@all.sepolia.ethdisco.net";

/// The execution network to join: its chain spec (genesis and fork schedule) and how to find
/// peers. Sepolia is built in; any other network comes from its `genesis.json` (with
/// `amsterdamTime` for Glamsterdam) and bootnode enodes.
#[derive(Clone)]
pub struct ElNetwork {
    pub spec: Arc<ChainSpec>,
    pub bootnodes: Vec<reth_network_peers::NodeRecord>,
    /// EIP-1459 DNS discovery tree, if the network publishes one.
    pub dns: Option<String>,
}

impl ElNetwork {
    pub fn sepolia() -> Self {
        Self { spec: SEPOLIA.clone(), bootnodes: reth_network_peers::sepolia_nodes(), dns: Some(SEPOLIA_DNS.to_string()) }
    }

    /// Hoodi (built in), from its published `genesis.json` (with `amsterdamTime`) and enodes.
    pub fn hoodi() -> Self {
        let genesis: alloy_genesis::Genesis = serde_json::from_str(include_str!("../networks/hoodi/genesis.json")).expect("built-in Hoodi genesis parses");
        let spec = Self::spec_from_genesis(genesis).expect("built-in Hoodi genesis is a Glamsterdam network");
        let bootnodes = enode_lines(include_str!("../networks/hoodi/enodes.yaml")).iter().filter_map(|l| l.parse().ok()).collect();
        Self { spec: Arc::new(spec), bootnodes, dns: None }
    }

    /// `genesis.json` in `dir`, plus `enodes.txt` (one `enode://` per line) if present, plus
    /// `extra_bootnodes`.
    pub fn from_dir(dir: &std::path::Path, extra_bootnodes: &[String]) -> Result<Self, Box<dyn std::error::Error>> {
        let raw = std::fs::read(dir.join("genesis.json")).map_err(|e| format!("{}: {e}", dir.join("genesis.json").display()))?;
        let genesis: alloy_genesis::Genesis = serde_json::from_slice(&raw)?;
        let spec = Self::spec_from_genesis(genesis)?;
        let mut lines: Vec<String> = std::fs::read_to_string(dir.join("enodes.txt"))
            .or_else(|_| std::fs::read_to_string(dir.join("enodes.yaml")))
            .map(|t| enode_lines(&t))
            .unwrap_or_default();
        lines.extend(extra_bootnodes.iter().cloned());
        let bootnodes = lines
            .iter()
            .filter(|l| l.starts_with("enode://"))
            .map(|l| l.parse::<reth_network_peers::NodeRecord>().map_err(|e| format!("bootnode {l}: {e}")))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { spec: Arc::new(spec), bootnodes, dns: None })
    }

    /// The chain spec of a Glamsterdam network's `genesis.json`. Refused without `amsterdamTime`,
    /// or if Cancun isn't active by then: skim applies BAL post-values as the new state, which
    /// is only complete with EIP-6780 (SELFDESTRUCT deletes only accounts created in the same
    /// transaction).
    pub fn spec_from_genesis(genesis: alloy_genesis::Genesis) -> Result<ChainSpec, Box<dyn std::error::Error>> {
        let Some(amsterdam) = genesis.config.amsterdam_time else {
            return Err("genesis.json has no amsterdamTime: skim follows Glamsterdam networks".into());
        };
        if genesis.config.cancun_time.is_none_or(|c| c > amsterdam) {
            return Err("genesis.json: Cancun (EIP-6780) must be active by amsterdamTime".into());
        }
        let mut spec = ChainSpec::from(genesis);
        spec.hardforks = forks_in_activation_order(&spec.hardforks);
        Ok(spec)
    }

    pub fn chain_id(&self) -> u64 {
        self.spec.chain.id()
    }

    /// The blob base-fee update fraction the chain's blob schedule sets at `timestamp`.
    pub fn blob_update_fraction(&self, timestamp: u64) -> Option<u64> {
        use reth_chainspec::EthChainSpec;
        self.spec.blob_params_at_timestamp(timestamp).map(|p| p.update_fraction as u64)
    }

    /// Bogota (Hegota's execution half), if scheduled: its activation time.
    pub fn bogota_time(&self) -> Option<u64> {
        self.spec.hardforks.get(reth_chainspec::EthereumHardfork::Bogota).and_then(|c| c.as_timestamp())
    }

    /// Timestamp forks in `genesis.json` that neither skim nor reth knows (`fooTime` keys, kept
    /// as extra fields), with their activation times. Ascending.
    pub fn unknown_forks(&self) -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> = self
            .spec
            .genesis
            .config
            .extra_fields
            .iter()
            .filter_map(|(k, v)| Some((k.strip_suffix("Time")?.to_string(), v.as_u64()?)))
            .collect();
        v.sort_by_key(|(_, t)| *t);
        v
    }

    /// The first scheduled fork skim won't follow: `(name, activation time)`. Bogota counts
    /// unless experimental Hegota support is on.
    pub fn unsupported_fork(&self, experimental_hegota: bool) -> Option<(String, u64)> {
        let bogota = self.bogota_time().filter(|_| !experimental_hegota).map(|t| ("bogota".to_string(), t));
        bogota.into_iter().chain(self.unknown_forks()).min_by_key(|(_, t)| *t)
    }
}

/// A pending transaction as a mempool measurement needs it.
#[derive(Debug, Clone)]
pub struct PendingTx {
    pub hash: B256,
    pub sender: alloy_primitives::Address,
    pub nonce: u64,
    /// gas_limit x max fee per gas + value: the balance it needs to stay includable.
    pub max_cost: U256,
    /// EIP-2718 type (4: an EIP-7702 set-code transaction).
    pub ty: u8,
}

impl PendingTx {
    /// `None` for a frame transaction: its sender is claimed, not recovered from a signature
    /// (frames authenticate in validation frames skim doesn't execute), so it says nothing about
    /// that account's pending nonce or balance. (Review: it was recorded as the sender's.)
    fn from_tx(tx: &SignedTx) -> Option<Self> {
        use alloy_consensus::Transaction as _;
        use alloy_consensus::transaction::SignerRecoverable as _;
        if tx.as_frame().is_some() {
            return None;
        }
        let sender = tx.recover_signer().ok()?;
        let max_cost = U256::from(tx.gas_limit()) * U256::from(tx.max_fee_per_gas()) + tx.value();
        Some(Self { hash: *tx.tx_hash(), sender, nonce: tx.nonce(), max_cost, ty: alloy_consensus::Typed2718::ty(tx) })
    }
}

/// What peers say about pending transactions.
#[derive(Debug)]
pub enum MempoolEvent {
    /// Hashes announced (eth/68 `NewPooledTransactionHashes`).
    Announced { peer: PeerId, hashes: Vec<B256> },
    /// Full transactions sent to skim (`Transactions`).
    Transactions { peer: PeerId, txs: Vec<PendingTx> },
}

/// Enodes from an `enodes.txt` (one per line) or `.yaml` (`- enode://…` items, comments).
fn enode_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.split(" #").next().unwrap_or("").trim().trim_start_matches("- ").trim_matches('"').to_string())
        .filter(|l| l.starts_with("enode://"))
        .collect()
}

/// reth lists a genesis's timestamp forks in mainnet's order and folds them into the fork id
/// (EIP-2124/6122) in list order. A network that activates them in another order, such as a
/// blob-parameter fork after Amsterdam, then gets a fork id no other client computes, and peers
/// drop skim at the handshake (Nethermind: a protocol breach, then a block on the address).
/// Timestamp forks are ordered by activation time here (stable; block forks stay first, as
/// they were).
fn forks_in_activation_order(h: &reth_chainspec::ChainHardforks) -> reth_chainspec::ChainHardforks {
    use reth_chainspec::ForkCondition;
    let mut forks: Vec<_> = h.forks_iter().map(|(f, c)| (dyn_clone::clone_box(f), c)).collect();
    forks.sort_by_key(|(_, c)| match c {
        ForkCondition::Timestamp(t) => (1, *t),
        _ => (0, 0),
    });
    reth_chainspec::ChainHardforks::new(forks)
}

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
    handle: NetworkHandle<SkimNetworkPrimitives>,
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
    /// `trusted` peers are kept connected (the trusted state-provider pool, PROJECT.md §6.3.1);
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
        Self::start(&ElNetwork::sepolia(), listen_port, trusted, trusted_only, node_key).await
    }

    /// Joins `network` (see [`ElNetwork`]); otherwise as [`Self::start_sepolia_with_key`].
    pub async fn start(
        network: &ElNetwork,
        listen_port: u16,
        trusted: Vec<TrustedPeer>,
        trusted_only: bool,
        node_key: Option<[u8; 32]>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::start_with(network, listen_port, trusted, trusted_only, node_key, None).await
    }

    /// [`Self::start`], also listening to the transactions peers announce or send
    /// (`mempool`: what they say, as [`MempoolEvent`]s). skim keeps no transaction pool and
    /// relays nothing; this is for measurements (`skim run --mempool-probe`).
    pub async fn start_with(
        network: &ElNetwork,
        listen_port: u16,
        trusted: Vec<TrustedPeer>,
        trusted_only: bool,
        node_key: Option<[u8; 32]>,
        mempool: Option<tokio::sync::mpsc::Sender<MempoolEvent>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let secret = match node_key {
            Some(k) => reth_network::config::SecretKey::from_byte_array(&k)?,
            None => rng_secret_key(),
        };
        let spec: Arc<ChainSpec> = network.spec.clone();
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
        let builder = NetworkConfig::<(), SkimNetworkPrimitives>::builder(secret, reth_tasks::Runtime::test())
            .boot_nodes(network.bootnodes.clone())
            .listener_addr(addr)
            .discovery_addr(addr);
        let builder = match &network.dns {
            Some(tree) => builder.dns_discovery(DnsDiscoveryConfig {
                bootstrap_dns_networks: Some([tree.parse::<LinkEntry>()?].into()),
                ..Default::default()
            }),
            None => builder.disable_dns_discovery(),
        };
        let config = builder
            .peer_config(
                PeersConfig::default()
                    .with_trusted_nodes(trusted)
                    .with_trusted_nodes_only(trusted_only)
                    .with_max_outbound(MAX_OUTBOUND_PEERS)
                    .with_max_inbound_opt(Some(MAX_INBOUND_PEERS)),
            )
            .with_snap(true)
            .add_rlpx_sub_protocol(snap1::Snap1Handler { peers: snap1.clone() })
            .disable_tx_gossip(true)
            .set_head(head)
            .build(NoopProvider::eth(spec));
        let mut manager = NetworkManager::<SkimNetworkPrimitives>::new(config).await?;
        if let Some(out) = mempool {
            let (tx, mut rx) = reth_metrics::common::mpsc::memory_bounded_channel(32 << 20, "skim_mempool");
            manager.set_transactions(tx);
            tokio::spawn(async move {
                use reth_network::transactions::NetworkTransactionEvent as E;
                while let Some(ev) = rx.recv().await {
                    let ev = match ev {
                        E::IncomingPooledTransactionHashes { peer_id, msg } => {
                            // Blob transactions come back wrapped with their sidecars, which skim's
                            // transaction type doesn't decode: they're not requested (eth/68+ says
                            // each announced hash's type; eth/66 announcements don't, and are skipped).
                            let hashes = match &msg {
                                reth_eth_wire_types::NewPooledTransactionHashes::Eth68(m) => {
                                    m.hashes.iter().zip(&m.types).filter(|(_, t)| **t != 3).map(|(h, _)| *h).collect()
                                }
                                reth_eth_wire_types::NewPooledTransactionHashes::Eth72(m) => {
                                    m.hashes.iter().zip(&m.types).filter(|(_, t)| **t != 3).map(|(h, _)| *h).collect()
                                }
                                reth_eth_wire_types::NewPooledTransactionHashes::Eth66(_) => Vec::new(),
                            };
                            MempoolEvent::Announced { peer: peer_id, hashes }
                        }
                        E::IncomingTransactions { peer_id, msg } => {
                            MempoolEvent::Transactions { peer: peer_id, txs: msg.0.iter().filter_map(PendingTx::from_tx).collect() }
                        }
                        _ => continue, // peers asking skim for pooled transactions: it has none
                    };
                    if out.try_send(ev).is_err() {
                        // The probe is behind: drop rather than buffer without bound.
                    }
                }
            });
        }
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

    pub fn handle(&self) -> &NetworkHandle<SkimNetworkPrimitives> {
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
        requests::add(match &request {
            SnapProtocolMessage::GetAccountRange(_) => &requests::SNAP_ACCOUNT,
            SnapProtocolMessage::GetStorageRanges(_) => &requests::SNAP_STORAGE,
            SnapProtocolMessage::GetByteCodes(_) => &requests::SNAP_CODE,
            _ => &requests::SNAP_OTHER,
        });
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

    /// [`Self::send`], counted under `kind` in [`requests`].
    async fn send_counted<T>(
        &self,
        kind: &std::sync::atomic::AtomicU64,
        peer: PeerId,
        make: impl FnOnce(oneshot::Sender<reth_network::p2p::error::RequestResult<T>>) -> PeerRequest<SkimNetworkPrimitives>,
    ) -> Timed<T> {
        requests::add(kind);
        self.send(peer, make).await
    }

    async fn send<T>(
        &self,
        peer: PeerId,
        make: impl FnOnce(oneshot::Sender<reth_network::p2p::error::RequestResult<T>>) -> PeerRequest<SkimNetworkPrimitives>,
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
        let (r, d) = self.send_counted(&requests::HEADERS, peer, |response| PeerRequest::GetBlockHeaders { request, response }).await;
        (r.map(|h| h.0), d)
    }

    /// eth/71 `GetBlockAccessLists`. `None` entries are "unavailable" (`0x80`).
    pub async fn bals(&self, peer: PeerId, hashes: Vec<B256>) -> Timed<BlockAccessLists> {
        let request = GetBlockAccessLists(hashes);
        self.send_counted(&requests::BALS, peer, |response| PeerRequest::GetBlockAccessLists { request, response }).await
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
        let (r, d) = self.send_counted(&requests::HEADERS, peer, |response| PeerRequest::GetBlockHeaders { request, response }).await;
        (r.map(|h| h.0), d)
    }

    /// `GetBlockBodies`. Callers verify each body against its header's `transactions_root`.
    pub async fn bodies(&self, peer: PeerId, hashes: Vec<B256>) -> Timed<Vec<BlockBody>> {
        let request = GetBlockBodies(hashes);
        let (r, d) = self.send_counted(&requests::BODIES, peer, |response| PeerRequest::GetBlockBodies { request, response }).await;
        (r.map(|b| b.0), d)
    }

    /// All receipts of one block (eth/70+ `GetReceipts`, following `lastBlockIncomplete`), within
    /// what a block that used `gas_used` can hold ([`ReceiptBudget`]). Callers verify them
    /// against the header's `receipts_root`.
    pub async fn block_receipts(&self, peer: PeerId, hash: B256, gas_used: u64) -> Timed<Vec<Receipt>> {
        let start = Instant::now();
        let mut out = Vec::new();
        let mut budget = ReceiptBudget::new(gas_used);
        for _ in 0..64 {
            let request = GetReceipts70 { first_block_receipt_index: out.len() as u64, block_hashes: vec![hash] };
            let (r, _) = self.send_counted(&requests::RECEIPTS, peer, |response| PeerRequest::GetReceipts70 { request, response }).await;
            match r {
                Ok(mut m) => {
                    // One block asked for: only the first list is an answer to it.
                    m.receipts.truncate(1);
                    let got = m.receipts.pop().unwrap_or_default();
                    if !budget.admit(&got) {
                        return (Err(ReqError::Peer("receipts: more than the block's gas allows".into())), start.elapsed());
                    }
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

    /// Receipts of several blocks (hash, header `gas_used`) in one `GetReceipts` exchange
    /// (continuing partial responses). Returns receipts for a prefix of `blocks`, in order: a peer
    /// may stop early (response size limits, or blocks it doesn't have). Callers verify each
    /// against its `receipts_root`.
    pub async fn blocks_receipts(&self, peer: PeerId, blocks: &[(B256, u64)]) -> Timed<Vec<Vec<Receipt>>> {
        let start = Instant::now();
        let mut out: Vec<Vec<Receipt>> = Vec::new();
        let mut cur: Vec<Receipt> = Vec::new();
        // The budget of the block `cur` is filling: blocks[at].
        let mut budget: Option<(usize, ReceiptBudget)> = None;
        for _ in 0..256 {
            if out.len() >= blocks.len() {
                break;
            }
            let request = GetReceipts70 { first_block_receipt_index: cur.len() as u64, block_hashes: blocks[out.len()..].iter().map(|b| b.0).collect() };
            let (r, _) = self.send_counted(&requests::RECEIPTS, peer, |response| PeerRequest::GetReceipts70 { request, response }).await;
            let mut m = match r {
                Ok(m) => m,
                Err(e) if out.is_empty() => return (Err(e), start.elapsed()),
                Err(_) => break,
            };
            if m.receipts.is_empty() {
                break;
            }
            // Lists beyond the blocks asked for answer nothing. (Review: they were kept, and the
            // next request's `blocks[out.len()..]` panicked.)
            m.receipts.truncate(blocks.len() - out.len());
            let k = m.receipts.len();
            let mut progressed = false;
            for (i, r) in m.receipts.into_iter().enumerate() {
                progressed |= !r.is_empty() || i > 0;
                if i > 0 {
                    cur = Vec::new();
                }
                let at = out.len();
                let b = match &mut budget {
                    Some((n, b)) if *n == at => b,
                    _ => &mut budget.insert((at, ReceiptBudget::new(blocks[at].1))).1,
                };
                if !b.admit(&r) {
                    // Past what the block can hold: what came before it still stands.
                    return (Ok(out), start.elapsed());
                }
                cur.extend(r);
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

    /// eth/68 `GetPooledTransactions`: the pending transactions behind announced hashes (the peer
    /// returns those it still has), with their senders recovered.
    pub async fn pooled_transactions(&self, peer: PeerId, hashes: Vec<B256>) -> Timed<Vec<PendingTx>> {
        let request = reth_eth_wire_types::GetPooledTransactions(hashes);
        let (r, d) = self.send(peer, |response| PeerRequest::GetPooledTransactions { request, response }).await;
        (r.map(|p| p.0.into_iter().filter_map(|t| PendingTx::from_tx(&t)).collect()), d)
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

/// What one block's receipts can plausibly hold, given its header's `gas_used`, checked as
/// partial eth/70 responses accumulate. The `receipts_root` check comes only when a block is
/// complete, so without this a peer that keeps answering "incomplete" grows the buffer without
/// bound before anything is verified. (Review.) Every transaction costs at least 21,000 gas and
/// every log byte 8 (plus 375 per log and per topic); the limits allow twice that, so a gas
/// accounting change (refunds, EIP-7778) can't make an honest block exceed them.
#[derive(Debug, Clone, Copy)]
pub struct ReceiptBudget {
    gas_used: u64,
    receipts: u64,
    last_cumulative: u64,
    log_gas: u64,
}

impl ReceiptBudget {
    const SLACK_GAS: u64 = 1_000_000;

    pub fn new(gas_used: u64) -> Self {
        Self { gas_used, receipts: 0, last_cumulative: 0, log_gas: 0 }
    }

    fn allowance(&self) -> u64 {
        self.gas_used.saturating_mul(2).saturating_add(Self::SLACK_GAS)
    }

    /// Adds `rs`; false once they can't all belong to the block.
    pub fn admit(&mut self, rs: &[Receipt]) -> bool {
        for r in rs {
            if r.cumulative_gas_used < self.last_cumulative || r.cumulative_gas_used > self.allowance() {
                return false;
            }
            self.last_cumulative = r.cumulative_gas_used;
            self.receipts += 1;
            for l in &r.logs {
                let cost = 375 + 375 * l.data.topics().len() as u64 + 8 * l.data.data.len() as u64;
                self.log_gas = self.log_gas.saturating_add(cost);
            }
        }
        self.receipts <= self.allowance() / 21_000 && self.log_gas <= self.allowance()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_fees_follow_the_chains_blob_schedule() {
        // Review: BLOBBASEFEE used the Prague update fraction on every chain.
        let sepolia = ElNetwork::sepolia();
        // 2026-05: after Sepolia's BPO2 (2025-11).
        assert_eq!(sepolia.blob_update_fraction(1_780_000_000), Some(11_684_671), "BPO2's");
        assert_eq!(sepolia.blob_update_fraction(0), None, "before Cancun there is no blob fee");
    }

    #[test]
    fn partial_receipts_stop_at_what_the_block_can_hold() {
        // Review: partial eth/70 answers were appended until the final root check, so a peer
        // always answering "incomplete" with junk grew the buffer by ~10 MiB per message.
        let receipt = |cum: u64, log_bytes: usize| Receipt {
            tx_type: 2,
            success: true,
            cumulative_gas_used: cum,
            logs: vec![alloy_primitives::Log::new_unchecked(alloy_primitives::Address::ZERO, vec![B256::ZERO], vec![0u8; log_bytes].into())],
            frame: None,
        };
        // An honest-looking 30M-gas block: 100 transactions with 1 KB logs.
        let mut b = ReceiptBudget::new(30_000_000);
        let honest: Vec<Receipt> = (1..=100).map(|i| receipt(i * 300_000, 1024)).collect();
        assert!(b.admit(&honest[..50]) && b.admit(&honest[50..]));
        // Junk: log data far beyond the block's gas, arriving in partial answers.
        let mut b = ReceiptBudget::new(30_000_000);
        let junk = receipt(21_000, 4 << 20);
        let admitted = (0..64).take_while(|_| b.admit(std::slice::from_ref(&junk))).count();
        assert!(admitted < 4, "{admitted} junk partials admitted");
        // More receipts than transactions the gas could pay for, or a cumulative gas going back.
        let mut b = ReceiptBudget::new(21_000);
        assert!(!b.admit(&vec![receipt(21_000, 0); 200]));
        let mut b = ReceiptBudget::new(30_000_000);
        assert!(!b.admit(&[receipt(500_000, 0), receipt(400_000, 0)]));
    }

    fn genesis(amsterdam: u64, bpo3: Option<u64>, cancun: Option<u64>) -> alloy_genesis::Genesis {
        let mut g = alloy_genesis::Genesis::default();
        let c = &mut g.config;
        c.chain_id = 3151908;
        c.terminal_total_difficulty = Some(U256::ZERO);
        c.terminal_total_difficulty_passed = true;
        (c.shanghai_time, c.cancun_time, c.prague_time, c.osaka_time) = (Some(0), cancun, Some(0), Some(0));
        (c.bpo1_time, c.bpo2_time, c.bpo3_time, c.amsterdam_time) = (Some(0), Some(0), bpo3, Some(amsterdam));
        g
    }

    #[test]
    fn a_blob_parameter_fork_after_amsterdam_folds_into_the_fork_id_in_activation_order() {
        // The devnet that found this: BPO3 activated after Amsterdam. reth's mainnet ordering put
        // BPO3 first, so skim's fork id matched no peer once both were active.
        let spec = ElNetwork::spec_from_genesis(genesis(100, Some(200), Some(0))).unwrap();
        let at = |t: u64| spec.fork_id(&Head { timestamp: t, ..Default::default() });
        let before = at(50);
        assert_eq!(before.next, 100, "Amsterdam is the next fork");
        let mid = at(150);
        assert_eq!(mid.next, 200, "then BPO3");
        let mut h = before.hash;
        h += 100u64;
        assert_eq!(mid.hash, h, "Amsterdam folded in");
        h += 200u64;
        assert_eq!(at(300).hash, h, "then BPO3, in activation order");
        assert_eq!(at(300).next, 0);
    }

    #[test]
    fn a_network_without_cancun_by_amsterdam_is_refused() {
        // BAL post-values are the whole new state only with EIP-6780.
        assert!(ElNetwork::spec_from_genesis(genesis(100, None, None)).is_err());
        assert!(ElNetwork::spec_from_genesis(genesis(100, None, Some(150))).is_err());
        assert!(ElNetwork::spec_from_genesis(genesis(100, None, Some(100))).is_ok());
    }

    #[test]
    fn built_in_hoodi_matches_reths_genesis_and_fork_ids() {
        let h = ElNetwork::hoodi();
        assert_eq!(h.chain_id(), 560048);
        assert_eq!(h.spec.genesis_hash(), reth_chainspec::HOODI.genesis_hash(), "the embedded genesis is Hoodi's");
        assert_eq!(h.bootnodes.len(), 6);
        // Before Glamsterdam, the fork id is reth's own Hoodi one (BPO1/BPO2 included).
        let at = |t: u64| Head { timestamp: t, ..Default::default() };
        let pre = 1_790_000_000;
        assert_eq!(h.spec.fork_id(&at(pre)).hash, reth_chainspec::HOODI.fork_id(&at(pre)).hash);
        assert_eq!(h.spec.fork_id(&at(pre)).next, 1_793_036_568, "Amsterdam is next");
        assert_eq!(h.spec.fork_id(&at(1_793_036_568)).next, 0);
        assert_eq!(h.unsupported_fork(false), None);
        assert_eq!(ElNetwork::sepolia().unsupported_fork(false), None, "reth schedules nothing after Amsterdam on Sepolia");
    }

    #[test]
    fn bogota_and_unknown_forks_in_a_genesis() {
        let mut g: alloy_genesis::Genesis = serde_json::from_str(include_str!("../networks/hoodi/genesis.json")).unwrap();
        g.config.bogota_time = Some(1_800_000_000);
        g.config.extra_fields.insert("fooTime".into(), serde_json::json!(1_900_000_000u64));
        let n = ElNetwork { spec: Arc::new(ElNetwork::spec_from_genesis(g).unwrap()), bootnodes: vec![], dns: None };
        assert_eq!(n.bogota_time(), Some(1_800_000_000));
        assert_eq!(n.unknown_forks(), vec![("foo".to_string(), 1_900_000_000)]);
        assert_eq!(n.unsupported_fork(false), Some(("bogota".into(), 1_800_000_000)));
        assert_eq!(n.unsupported_fork(true), Some(("foo".into(), 1_900_000_000)));
        // reth knows Bogota: the fork id changes there, so peers past it stay compatible.
        let at = |t: u64| Head { timestamp: t, ..Default::default() };
        assert_eq!(n.spec.fork_id(&at(1_799_999_999)).next, 1_800_000_000);
        assert_ne!(n.spec.fork_id(&at(1_799_999_999)).hash, n.spec.fork_id(&at(1_800_000_000)).hash);
    }
}
