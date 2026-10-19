//! `skim run`: follow the chain from CL payload gossip and serve JSON-RPC on top of it.
//!
//! - Chain: each verified `execution_payload` envelope is applied to the MVCC store (BAL), and
//!   its header kept for the execution environment. Gaps are filled from EL peers (headers +
//!   eth/71 BALs). A one-block fork switches branches; deeper ones wait for the EL path.
//! - State: nothing is watched up front. Every key a request reads is fetched once over SNAP
//!   (parent root + BAL replay) and from then on stays current through BAL writes.

use std::{net::SocketAddr, sync::Arc};

use skim_bal::BlockWrites;
use skim_cl::{ClEvent, ClNode, NodeConfig, ssz::Envelope, verify};
use skim_el::ElNet;
use skim_sim::{Simulator, learned::{self, Learned}, predictor::Predictor};
use skim_snap::{Scheduler, el::ElTransport};
use skim_state::Store;

use crate::{
    chain::{Chain, Placed},
    rpc::{Rpc, serve},
};

#[derive(clap::Args)]
pub struct Args {
    /// JSON-RPC listen address.
    #[arg(long, default_value = "127.0.0.1:8545")]
    rpc: SocketAddr,
    /// `Access-Control-Allow-Origin` for browser dapps calling skim directly: an origin
    /// (`https://app.example`), `*` (any page in this machine's browser may call it), or `none`.
    /// skim holds no keys, and listens on localhost by default.
    #[arg(long, default_value = "*")]
    cors: String,
    /// SNAP hedging: `adaptive` (each peer's own latency decides when a slow request is
    /// duplicated to another peer) or `fixed` (after 1.2 s). For A/B measurements.
    #[arg(long, default_value = "adaptive", hide = true)]
    snap_hedge: String,
    /// Enable diagnostic RPC methods for measurements (`skim_debugSetSnapHedge`).
    #[arg(long, hide = true)]
    debug_rpc: bool,
    /// The network: `sepolia` or `hoodi` (built in), or a network-config directory, as consensus clients
    /// use (and ethereum-package / eth-clients publish): `config.yaml` (with its PRESET_BASE),
    /// `genesis.ssz` or `genesis_validators_root.txt`, `bootstrap_nodes.txt`, plus the execution
    /// `genesis.json` (with `amsterdamTime`) and optionally `enodes.txt`.
    #[arg(long, default_value = "sepolia")]
    network: String,
    /// Extra EL bootnodes (`enode://…`), for networks without a DNS tree or `enodes.txt`.
    #[arg(long = "el-bootnode")]
    el_bootnode: Vec<String>,
    /// CL peers to dial directly and keep (libp2p multiaddrs ending in `/p2p/<peer id>`), e.g.
    /// a devnet's beacon nodes through their mapped ports. Repeatable.
    #[arg(long = "cl-peer")]
    cl_peer: Vec<skim_cl::Multiaddr>,
    /// Attach to a skim-hub (shared chain data for several skim instances; a trusted setup).
    /// Not supported yet: skim refuses to start with it.
    #[arg(long, value_name = "URL")]
    hub: Option<String>,
    #[arg(long, default_value_t = 9130)]
    cl_port: u16,
    #[arg(long, default_value_t = 30388)]
    el_port: u16,
    /// Blocks of history kept for reads and reorgs.
    #[arg(long, default_value_t = 256)]
    window: u64,
    /// Never use these peers (hex node ids) for SNAP state, e.g. a node being benchmarked
    /// against skim on the same host. Repeatable.
    #[arg(long = "exclude-snap-peer")]
    exclude_snap_peer: Vec<alloy_primitives::B512>,
    /// skim.yaml: state subscriptions, on-demand cache TTL, limits (PROJECT.md §7.5).
    #[arg(long)]
    config: Option<std::path::PathBuf>,
    /// On-disk archive for subscriptions with `retention: { mode: archive }` (M4).
    #[arg(long)]
    archive: Option<std::path::PathBuf>,
    /// Extend archived history this many blocks back from retained BALs (eth/71 keeps ~2 weeks).
    /// Keys never written in that range become covered for all of it; written keys from their
    /// first write. 0 = off.
    #[arg(long, default_value_t = 0)]
    archive_backfill: u64,
    /// Largest block range one `eth_getLogs` request may scan.
    #[arg(long, default_value_t = 2048)]
    max_logs_range: u64,
    /// History beyond the in-memory window (about a day of headers): `off` keeps skim head-only
    /// (no history database; logs and block streams cover the in-memory window, older ranges
    /// are a `gap`), for when an external indexer keeps what it needs. `local` keeps post-fork
    /// headers and every fetched block's logs on disk (`history.redb` in the data dir), so
    /// history reaches back to the fork and nothing is fetched twice. Memory stays bounded
    /// either way.
    #[arg(long, value_enum, default_value_t = HistoryMode::Off)]
    history: HistoryMode,
    /// Which transactions `eth_getTransactionByHash`, `eth_getTransactionReceipt` and
    /// `debug_traceTransaction` can find by hash. `pinned`: only those whose BAL changes touch
    /// a pinned address (which covers the sender, through its nonce change; a call that only
    /// reads a pinned contract isn't indexed), so nothing is kept about unrelated transactions. `all`: every transaction (a wallet backend or
    /// explorer looking up arbitrary hashes). Transactions sent through skim are always found.
    #[arg(long, value_enum, default_value_t = TxIndexArg::Pinned)]
    tx_index: TxIndexArg,
    /// Recent blocks the transaction index covers (2048 is about 7 h of 12 s blocks; 0 = none).
    #[arg(long, default_value_t = 2048)]
    tx_index_blocks: usize,
    /// Consensus-layer gossip mesh degree (more = more eager-push paths for new payloads).
    #[arg(long, default_value_t = 8)]
    cl_mesh: usize,
    /// Consensus-layer peers to keep (more = better odds of being close to each publisher).
    #[arg(long, default_value_t = 50)]
    cl_peers: usize,
    /// EL peers to keep connected (enode URLs), e.g. your own node as a dependable state source.
    /// Answers are still Merkle-proof checked. Repeatable.
    #[arg(long = "trusted-peer")]
    trusted_peer: Vec<skim_el::TrustedPeer>,
    /// Directory for restart data: a snapshot of subscribed state, the EL node key and known
    /// peers (a few MB at most). Default: `$HOME/.skim/<network>` (`sepolia` for the built-in one).
    #[arg(long)]
    data_dir: Option<std::path::PathBuf>,
    /// Keep nothing on disk (every start begins empty, with a new node identity).
    #[arg(long)]
    ephemeral: bool,
    /// Trusted beacon block root to bootstrap consensus verification from (weak subjectivity).
    /// Default: the last finalized root this node verified, else `--checkpoint-url`.
    #[arg(long)]
    checkpoint: Option<alloy_primitives::B256>,
    /// Checkpoint-sync endpoint used when no checkpoint is given or saved (Sepolia: ethPandaOps'
    /// by default; other networks: a beacon node's API, e.g. a devnet's).
    #[arg(long)]
    checkpoint_url: Option<String>,
    /// Answer `latest` with the highest block the sync committee has signed off on, instead of
    /// the newest gossiped block (trust-minimized, typically ~1-2 slots behind).
    #[arg(long)]
    require_verified: bool,
    /// Don't cache `eth_call` results per (block, request) (measurements of raw execution).
    #[arg(long)]
    no_call_cache: bool,
    /// Serve Prometheus metrics at http://<addr>/metrics (e.g. 127.0.0.1:9465; off unless set).
    /// `metrics/` has a Prometheus + Grafana stack with skim's dashboard.
    #[arg(long, value_name = "ADDR")]
    metrics: Option<SocketAddr>,
    /// Addresses (most-called first) exported with per-address series; 0 exports none.
    #[arg(long, default_value_t = 50)]
    metrics_addresses: usize,
    /// Seconds after which a series that stopped updating (a peer that left, an address that
    /// fell out of the top N) is dropped from /metrics.
    #[arg(long, default_value_t = 300)]
    metrics_idle: u64,
    /// How long a request that needs state skim doesn't hold waits for a SNAP peer when none is
    /// connected (the first minutes after a cold start), before answering -32002.
    #[arg(long, default_value_t = 60)]
    state_wait: u64,
    /// Don't answer other peers' requests (EL headers/bodies/receipts/BALs/bytecode, CL payload
    /// envelopes); requests get empty answers.
    #[arg(long)]
    no_serve: bool,
    /// Disable the learned-set pattern tier (A/B measurements).
    #[arg(long)]
    no_learned_patterns: bool,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum TxIndexArg {
    Pinned,
    All,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum HistoryMode {
    Off,
    Local,
}

#[derive(serde::Deserialize, Default)]
struct Config {
    #[serde(default)]
    state: StateConfig,
    #[serde(default)]
    predictors: Predictors,
}

/// Protocol-aware predictors (PROJECT.md §7.2).
#[derive(serde::Deserialize, Default)]
struct Predictors {
    #[serde(default)]
    uniswap_v3: Option<V3Config>,
}

/// Uniswap V3: for `quoteExactInputSingle` on these pools, predict the bitmap words a swap of
/// the given size can reach, and keep a window around the current tick warm in the background.
#[derive(serde::Deserialize)]
struct V3Config {
    pools: Vec<alloy_primitives::Address>,
    /// Bitmap words kept warm on each side of the current word (grows with observed reach).
    #[serde(default = "two")]
    words: i32,
    /// Background prefetch budget: keys per pool per block.
    #[serde(default = "two_fifty_six")]
    prefetch_budget: usize,
    /// Prefetched keys are a hot window, not a subscription: a key stays cached this many
    /// blocks after it was last inside the window around the current tick.
    #[serde(default = "sixteen")]
    window_ttl_blocks: u64,
}

fn sixteen() -> u64 {
    16
}

fn two() -> i32 {
    2
}
fn two_fifty_six() -> usize {
    256
}

#[derive(serde::Deserialize, Default)]
struct StateConfig {
    #[serde(default)]
    subscriptions: Vec<skim_sim::subscription::SubscriptionSpec>,
    #[serde(default)]
    on_demand: OnDemand,
    #[serde(default)]
    limits: Limits,
}

#[derive(serde::Deserialize)]
struct OnDemand {
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default = "four")]
    cache_ttl_blocks: u64,
    /// Fetch rounds a call may take before it is refused. Each round fetches the state where the
    /// previous run stopped; deep call chains (e.g. ENS's Universal Resolver) need more.
    #[serde(default = "eight")]
    max_fetch_rounds: usize,
}

impl Default for OnDemand {
    fn default() -> Self {
        Self { enabled: true, cache_ttl_blocks: 4, max_fetch_rounds: 8 }
    }
}

fn eight() -> usize {
    8
}

#[derive(serde::Deserialize)]
struct Limits {
    #[serde(default = "hundred_k")]
    max_subscribed_keys: skim_sim::subscription::Limit,
    #[serde(default = "two_hundred_k")]
    max_on_demand_keys: skim_sim::subscription::Limit,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_subscribed_keys: hundred_k(), max_on_demand_keys: two_hundred_k() }
    }
}

fn two_hundred_k() -> skim_sim::subscription::Limit {
    skim_sim::subscription::Limit(200_000)
}

fn yes() -> bool {
    true
}
fn four() -> u64 {
    4
}
fn hundred_k() -> skim_sim::subscription::Limit {
    skim_sim::subscription::Limit(100_000)
}

pub async fn run(args: Args) -> anyhow::Result<()> {
    // Before anything else starts: reth registers its network metrics when the network is built.
    let metrics_handle = match args.metrics {
        Some(_) => Some(crate::metrics::install(std::time::Duration::from_secs(args.metrics_idle))?),
        None => None,
    };
    if let Some(h) = &args.hub {
        anyhow::bail!("--hub {h}: attaching to a skim-hub isn't supported yet (skim-hub is in development); run without --hub");
    }
    // Which network: everything chain-specific (chain id, fork schedule, preset, bootnodes,
    // genesis) comes from here.
    let (cl_net, el_net) = if let Some(cl) = skim_cl::network::ClNetwork::built_in(&args.network) {
        let mut el = if args.network == "hoodi" { skim_el::ElNetwork::hoodi() } else { skim_el::ElNetwork::sepolia() };
        for b in &args.el_bootnode {
            el.bootnodes.push(b.parse().map_err(|e| anyhow::anyhow!("--el-bootnode {b}: {e}"))?);
        }
        (cl, el)
    } else {
        let dir = std::path::PathBuf::from(&args.network);
        anyhow::ensure!(dir.is_dir(), "--network: `sepolia`, `hoodi` or a network-config directory ({} isn't one)", dir.display());
        let cl = skim_cl::network::ClNetwork::from_dir(&dir)?;
        let el = skim_el::ElNetwork::from_dir(&dir, &args.el_bootnode).map_err(|e| anyhow::anyhow!("{e}"))?;
        (cl, el)
    };
    tracing::info!(network = %cl_net.name, chain_id = el_net.chain_id(), preset = cl_net.preset.name, slot_ms = cl_net.slot_ms,
        gloas_epoch = cl_net.gloas_fork_epoch, cl_bootnodes = cl_net.bootnodes.len(), el_bootnodes = el_net.bootnodes.len(), "network");
    skim_sim::env::set_chain_id(el_net.chain_id());
    skim_cl::network::set_current(cl_net.clone());
    let checkpoint_url = args.checkpoint_url.clone().or_else(|| cl_net.default_checkpoint_url().map(str::to_string));
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    if now < cl_net.gloas_time() {
        let left = cl_net.gloas_time() - now;
        tracing::warn!(network = %cl_net.name, gloas_epoch = cl_net.gloas_fork_epoch, in_hours = left / 3600,
            "Glamsterdam isn't active on this network yet: skim follows it from the fork on (no BALs before it); until then nothing is applied and state reads wait");
    }
    let default_dir = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".skim").join(&cl_net.name);
    let persist = if args.ephemeral {
        None
    } else {
        let dir = args.data_dir.clone().unwrap_or_else(|| default_dir.clone());
        Some(Arc::new(skim_state::persist::Persist::open(dir.join("restart.redb"))?))
    };
    let node_key = match &persist {
        Some(p) => Some(crate::persist::node_key(p)?),
        None => None,
    };
    let net = ElNet::start(&el_net, args.el_port, args.trusted_peer.clone(), false, node_key)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let saved = match &persist {
        Some(p) => {
            let known = crate::persist::load_peers(p);
            tracing::info!(n = known.len(), "dialing saved EL peers");
            net.add_peers(&known);
            crate::persist::spawn_peer_saver(p.clone(), net.clone());
            p.load().unwrap_or_else(|e| {
                tracing::warn!("restart snapshot unreadable, starting empty: {e}");
                None
            })
        }
        None => None,
    };
    if let Some(s) = &saved {
        tracing::info!(block = s.number, keys = s.values.len(), "restart snapshot found; will verify and replay it to the head");
    }
    let mut saved = saved;
    // The recent-BAL index before the first applied block is filled once, after it applies.
    let mut bals_seeded = false;
    let transport = ElTransport::new(net.clone());
    for p in &args.exclude_snap_peer {
        transport.exclude(*p);
    }
    let rej = transport.rejections.clone();
    tokio::spawn(async move {
        use std::sync::atomic::Ordering::Relaxed;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            tracing::info!(no_proof = rej.no_proof.load(Relaxed), bad_proof = rej.bad_proof.load(Relaxed),
                contradiction = rej.contradiction.load(Relaxed), root_unavailable = rej.root_unavailable.load(Relaxed),
                "SNAP answers rejected by verification");
        }
    });
    let sched = Scheduler::new(
        transport,
        skim_snap::Config {
            no_peer_wait: std::time::Duration::from_secs(args.state_wait),
            hedge_adaptive: args.snap_hedge != "fixed",
            ..Default::default()
        },
    );
    let store = match &args.archive {
        Some(p) => {
            let ar = skim_state::archive::Archive::open(p)?;
            tracing::info!(path = %p.display(), last_block = ?ar.last_block()?, segments = ?ar.segments()?,
                archived_keys = ar.archived_keys()?.len(), "archive opened");
            Store::with_archive(args.window, ar)?
        }
        None => Store::new(args.window),
    };
    let sim = Simulator::new(
        store.clone(),
        sched,
        Predictor {
            learned: Learned::with_limits(8, 0.5, 256, learned::Limits {
                patterns: if args.no_learned_patterns { 0 } else { learned::Limits::default().patterns },
                ..Default::default()
            }),
            v3: vec![],
            base_words: 2,
            reach: Default::default(),
        },
    );
    *sim.state_wait.write() = std::time::Duration::from_secs(args.state_wait);
    let cfg: Config = match &args.config {
        Some(p) => serde_yaml_ng::from_str(&std::fs::read_to_string(p)?)?,
        None => Config::default(),
    };
    if !cfg.state.on_demand.enabled {
        tracing::warn!("on_demand.enabled=false: unsubscribed state is still fetched for correctness, with a 1-block TTL");
    }
    sim.max_rounds.store(cfg.state.on_demand.max_fetch_rounds.max(1), std::sync::atomic::Ordering::Relaxed);
    *sim.on_demand.write() = skim_state::Retention::Ttl(if cfg.state.on_demand.enabled { cfg.state.on_demand.cache_ttl_blocks } else { 1 });
    let subs = Arc::new(skim_sim::subscription::Subscriptions::new(cfg.state.subscriptions, cfg.state.limits.max_subscribed_keys.0)?);
    // The limit is enforced by the store itself, for every path that subscribes keys.
    store.set_max_subscribed(cfg.state.limits.max_subscribed_keys.0);
    store.set_max_on_demand(cfg.state.limits.max_on_demand_keys.0);
    tracing::info!(subscriptions = subs.status().len(), declared_keys = subs.total_keys(), "state subscriptions loaded");
    if subs.status().is_empty() {
        tracing::info!("no subscriptions: every state read is fetched on demand (verified) and kept current while it's used; pin hot addresses in a --config file for the lowest latency");
    }
    let mut ncfg = NodeConfig::from_network(&cl_net, args.cl_port);
    ncfg.explicit_peers.extend(args.cl_peer.iter().cloned());
    ncfg.mesh_n = args.cl_mesh;
    ncfg.target_peers = args.cl_peers;
    ncfg.max_peers = args.cl_peers + args.cl_peers / 4;
    let (cl, mut rx) = ClNode::start(ncfg).await?;
    let cl_stats = cl.stats.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let s = cl_stats.lock().unwrap();
            tracing::info!(peers = s.connected, outbound = s.outbound, mesh = ?s.mesh, topic_peers = ?s.topic_peers, "CL gossip");
        }
    });
    let light = crate::light::Light::new(cl.clone(), persist.clone());
    light.start(args.checkpoint, checkpoint_url);
    let chaindata = crate::chaindata::ChainData::new(net.clone(), args.max_logs_range, skim_sim::env::chain_id());
    {
        use crate::chaindata::{TxIndexConfig, TxIndexMode};
        let mode = match args.tx_index {
            TxIndexArg::Pinned => TxIndexMode::Pinned,
            TxIndexArg::All => TxIndexMode::All,
        };
        let pinned_store = store.clone();
        let pinned: crate::chaindata::PinnedFn = Arc::new(move || pinned_store.pinned_addresses().into_iter().collect());
        chaindata.set_tx_index(TxIndexConfig { mode, blocks: args.tx_index_blocks }, Some(pinned));
    }
    if args.history == HistoryMode::Local {
        anyhow::ensure!(!args.ephemeral, "--history local needs a data dir (not --ephemeral)");
        let dir = args.data_dir.clone().unwrap_or_else(|| default_dir.clone());
        match crate::history::HeaderStore::open(dir.join("history.redb")) {
            Ok(s) => chaindata.set_history(s),
            Err(e) => tracing::warn!("post-fork header store unavailable ({e}); logs cover about a day"),
        }
    }
    let dash = Arc::new(crate::dashboard::Dashboard::default());
    if !args.no_serve {
        net.serve(Arc::new(crate::serving::Served { chain: chaindata.clone(), store: store.clone() }));
    }
    let (ws_events, _) = tokio::sync::broadcast::channel::<crate::rpc::WsEvent>(256);
    let rpc = Arc::new(Rpc {
        sim: sim.clone(),
        subs: Some(subs.clone()),
        chain: Some(chaindata.clone()),
        light: Some(light.clone()),
        require_verified: args.require_verified,
        events: ws_events.clone(),
        call_cache: Default::default(),
        cache_calls: !args.no_call_cache,
        cl_stats: Some(cl.stats.clone()),
        dash: dash.clone(),
    });
    crate::rpc::DEBUG_RPC.store(args.debug_rpc, std::sync::atomic::Ordering::Relaxed);
    tokio::spawn(serve(rpc.clone(), args.rpc, (args.cors != "none").then(|| args.cors.clone())));
    if let (Some(addr), Some(handle)) = (args.metrics, metrics_handle) {
        let top = args.metrics_addresses;
        tokio::spawn(async move {
            if let Err(e) = crate::metrics::serve(addr, handle, rpc, top).await {
                tracing::error!("metrics endpoint: {e}");
            }
        });
    }
    if store.archive().is_some() {
        spawn_archive_healer(store.clone(), net.clone(), args.archive_backfill);
    }
    // Subscription maintenance: one pass at a time, always on the newest head. Heads that
    // arrive during a pass are coalesced (the next pass runs once, on the latest), never
    // dropped, so readiness and on_block results catch up instead of falling behind.
    let (maint_tx, mut maint_rx) = tokio::sync::watch::channel::<Option<skim_state::BlockRef>>(None);
    {
        let (sim, subs, events) = (sim.clone(), subs.clone(), ws_events.clone());
        tokio::spawn(async move {
            while maint_rx.changed().await.is_ok() {
                // No waiting for a restore: subscribed key sets are small, so fetching them
                // fresh is usually faster than replaying a long snapshot gap. Both run; the
                // restore fills in only what is still unknown when it finishes.
                let Some(b) = *maint_rx.borrow_and_update() else { continue };
                let t0 = std::time::Instant::now();
                subs.maintain(&sim, &b).await;
                metrics::histogram!("skim_subscription_maintenance_seconds").record(t0.elapsed().as_secs_f64());
                let _ = events.send(crate::rpc::WsEvent::Subscriptions(b.number));
            }
        });
    }
    let v3 = cfg.predictors.uniswap_v3.map(Arc::new);
    if let Some(v) = &v3 {
        sim.predictor.lock().base_words = v.words;
    }
    let v3_state = Arc::new(V3State::default());
    let sim2 = sim.clone();
    let store2 = store.clone();
    let cd2 = chaindata.clone();
    let cd3 = chaindata.clone();
    let chain = Chain {
        store: store.clone(),
        net: net.clone(),
        on_header: Box::new(move |h| {
            sim2.insert_header(h.clone());
            store2.note_header(h.hash_slow(), alloy_rlp::encode(h));
        }),
        // Chain data (the canonical index, transaction inclusion) follows the store: only a
        // block it applied is canonical there too.
        on_applied: Box::new(move |h| cd2.note_header(h)),
        on_rollback: Box::new(move |n| cd3.rollback_to(n)),
        lookback: 32,
        cl: Some(Arc::new(cl.clone())),
    };
    tracing::info!(network = %cl_net.name, "following the chain from CL gossip; RPC answers once the first block is applied");

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        let ev = tokio::select! {
            ev = rx.recv() => match ev { Some(ev) => ev, None => break },
            _ = tokio::signal::ctrl_c() => break,
            _ = term.recv() => break,
        };
        let ClEvent::Gossip { topic, data, received, .. } = ev else { continue };
        if topic.starts_with("light_client_") {
            let (light, store) = (light.clone(), store.clone());
            tokio::spawn(async move { light.on_gossip(&topic, &data, &store).await });
            continue;
        }
        if topic != "execution_payload" {
            continue;
        }
        let Ok(env) = Envelope::parse(&data) else { continue };
        let v = verify::verify_payload(&env);
        if !v.ok {
            tracing::warn!(number = env.payload.block_number, "payload hash mismatch; ignored");
            continue;
        }
        let Ok(bal) = skim_bal::decode(env.payload.block_access_list) else { continue };
        let number = v.header.number;
        // Payload arrival relative to the start of its slot (the block timestamp).
        let arrival_ms = received
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1e3 - v.header.timestamp as f64 * 1e3)
            .unwrap_or(f64::NAN);
        let writes = BlockWrites::from_bal(&bal);
        let served = (!args.no_serve).then(|| {
            (env.payload.slot_number, skim_cl::serve::Served {
                root: env.beacon_block_root,
                block_hash: v.computed_hash,
                parent_hash: env.payload.parent_hash,
                ssz: data.clone(),
            })
        });
        chaindata.note_payload(
            Arc::new(v.header.clone()),
            v.computed_hash,
            env.payload.transactions.iter().map(|t| alloy_primitives::Bytes::copy_from_slice(t)).collect(),
            env.payload
                .withdrawals
                .iter()
                .map(|w| alloy_consensus::private::alloy_eips::eip4895::Withdrawal {
                    index: w.index,
                    validator_index: w.validator_index,
                    address: w.address,
                    amount: w.amount_gwei,
                })
                .collect(),
            writes.touched.iter().copied().collect(),
            Arc::new(bal),
            alloy_primitives::Bytes::copy_from_slice(env.payload.block_access_list),
        );
        match chain.place(v.header, Arc::new(writes)).await {
            Placed::Applied { ready, reorged_to, filled } => {
                if let Some((slot, e)) = served {
                    cl.serve_envelope(slot, e);
                }
                if let Some(h) = chaindata.cached_header(ready.block.number.into()) {
                    net.set_head(&h);
                }
                tracing::info!(number, arrival_ms = arrival_ms as i64, changed = ready.changed_keys.len(), ?reorged_to, filled, "state ready");
                dash.record_block(crate::dashboard::BlockStat { number, arrival_ms, changed: ready.changed_keys.len() });
                if arrival_ms.is_finite() {
                    metrics::histogram!("skim_block_arrival_seconds").record(arrival_ms / 1e3);
                }
                metrics::gauge!("skim_block_changed_keys").set(ready.changed_keys.len() as f64);
                light.on_block(&store);
                let _ = ws_events.send(crate::rpc::WsEvent::NewHead(ready.block));
                if !bals_seeded {
                    bals_seeded = true;
                    let (store, cl, b, w) = (store.clone(), cl.clone(), ready.block, args.window);
                    tokio::spawn(async move { crate::persist::seed_recent_bals(store, &cl, b, w).await });
                }
                if let Some(s) = saved.take() {
                    // Runs alongside subscription maintenance (see the maintenance task).
                    let (store, net, cl, b) = (store.clone(), net.clone(), cl.clone(), ready.block);
                    tokio::spawn(crate::persist::restore(store, net, cl, s, b));
                }
                if let Some(p) = &persist
                    && number % 4 == 0
                {
                    crate::persist::save_now(p, &store, &ready.block);
                }
                if let Some(v) = &v3 {
                    tokio::spawn(v3_tick(sim.clone(), v.clone(), v3_state.clone(), ready.block));
                }
                // Keep subscriptions current and their readiness fresh (one pass at a time).
                let _ = maint_tx.send(Some(ready.block));
            }
            Placed::Reanchored(_) => tracing::warn!(number, "re-anchored"),
            Placed::Duplicate => {}
            Placed::Deferred(why) => tracing::debug!(number, "deferred: {why}"),
        }
    }
    if let (Some(p), Some(head)) = (&persist, store.head()) {
        crate::persist::save_now(p, &store, &head);
        tracing::info!(block = head.number, "shutting down; restart snapshot saved");
    }
    Ok(())
}

#[derive(Default)]
struct V3State {
    registered: std::sync::atomic::AtomicBool,
    busy: std::sync::atomic::AtomicBool,
    pools: parking_lot::Mutex<Vec<skim_sim::prefetch::V3Pool>>,
}

/// Per block: register the configured V3 pools once (tickSpacing, fee, tokens, read through the
/// simulator like any call), then keep each pool's tick window warm with a budgeted,
/// low-priority prefetch. One pass at a time.
async fn v3_tick(sim: Arc<Simulator>, cfg: Arc<V3Config>, st: Arc<V3State>, b: skim_state::BlockRef) {
    use std::sync::atomic::Ordering::SeqCst;
    if st.busy.swap(true, SeqCst) {
        return;
    }
    if !st.registered.load(SeqCst) {
        let mut ok = 0;
        for &pool in &cfg.pools {
            let word = |sel: [u8; 4]| {
                let sim = sim.clone();
                async move {
                    let header = sim.header(&b)?;
                    let tx = revm::context::TxEnv::builder()
                        .kind(revm::primitives::TxKind::Call(pool))
                        .data(alloy_primitives::Bytes::copy_from_slice(&sel))
                        .gas_limit(header.gas_limit)
                        .gas_price(0)
                        .build()
                        .ok()?;
                    let out = sim.call(&b, tx, skim_sim::Mode::Resolve { max_rounds: sim.max_rounds.load(std::sync::atomic::Ordering::Relaxed) }).await.ok()?;
                    match out.result {
                        revm::context::result::ExecutionResult::Success { output, .. } if output.data().len() == 32 => {
                            Some(alloy_primitives::Bytes::copy_from_slice(output.data()))
                        }
                        _ => None,
                    }
                }
            };
            let (Some(sp), Some(fee), Some(t0), Some(t1)) = (
                word([0xd0, 0xc9, 0x3a, 0x7c]).await, // tickSpacing()
                word([0xdd, 0xca, 0x3f, 0x43]).await, // fee()
                word([0x0d, 0xfe, 0x16, 0x81]).await, // token0()
                word([0xd2, 0x12, 0x20, 0xa7]).await, // token1()
            ) else {
                tracing::warn!(%pool, "uniswap_v3: not a V3 pool (or state unavailable yet); retrying next block");
                continue;
            };
            let Some(spacing) = skim_sim::prefetch::decode_spacing(&sp) else { continue };
            let p = skim_sim::prefetch::V3Pool { address: pool, spacing };
            let meta = skim_sim::predictor::V3Meta {
                pool: p,
                fee: alloy_primitives::U256::from_be_slice(&fee).to(),
                token0: alloy_primitives::Address::from_slice(&t0[12..]),
                token1: alloy_primitives::Address::from_slice(&t1[12..]),
            };
            let mut pred = sim.predictor.lock();
            if !pred.v3.iter().any(|m| m.pool.address == pool) {
                pred.v3.push(meta);
                st.pools.lock().push(p);
                tracing::info!(%pool, spacing, fee = meta.fee, "uniswap_v3 predictor: pool registered");
            }
            ok += 1;
        }
        if ok == cfg.pools.len() {
            st.registered.store(true, SeqCst);
        }
    }
    let at = if sim.miss_at_parent { sim.store.block(b.number.saturating_sub(1)).unwrap_or(b) } else { b };
    let pools = st.pools.lock().clone();
    for p in pools {
        let window = sim.predictor.lock().window(&p.address);
        let ttl = skim_state::Retention::Ttl(cfg.window_ttl_blocks);
        skim_sim::prefetch::refresh_pool(&sim.store, &sim.sched, &b, &at, &p, window, cfg.prefetch_budget, ttl).await;
    }
    st.busy.store(false, SeqCst);
}

/// Heals gaps between archive segments (e.g. after a restart) from eth/71 headers + BALs,
/// retrying until EL peers can serve them. Gaps it can't fill stay uncovered, never guessed.
fn spawn_archive_healer(store: Arc<Store>, net: ElNet, backfill: u64) {
    tokio::spawn(async move {
        let mut target_from: Option<u64> = None;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            let Some(ar) = store.archive() else { return };
            let Ok(segs) = ar.segments() else { continue };
            // M4.2: backfill once segments are contiguous and keys exist. Re-run when keys whose
            // coverage starts after the target appear (late subscriptions).
            if backfill > 0 && segs.len() == 1 {
                let s = segs[0].0;
                let tf = *target_from.get_or_insert(s.saturating_sub(backfill).max(1));
                let late = ar.coverages().map(|c| c.iter().any(|(_, c, ext)| *ext && *c > tf)).unwrap_or(false);
                if late {
                    match backfill_archive(&store, &net, tf).await {
                        Ok(r) => tracing::info!(from = tf, ?r, "archive backfilled from retained BALs"),
                        Err(e) => tracing::debug!(from = tf, "archive backfill not done yet: {e}"),
                    }
                }
            }
            for w in segs.windows(2) {
                let ((a_start, a_end), (b_start, _)) = (w[0], w[1]);
                if a_end + 1 == b_start {
                    // Adjacent but not merged: b_start doesn't link to a_end (a fork the archive
                    // missed, e.g. while skim was offline). Drop the orphaned tail of segment a.
                    match reconcile(&store, &net, a_start, a_end, b_start).await {
                        Ok(n) => tracing::info!(from = n, to = a_end, "archive: orphaned blocks removed; gap will be refilled from the current chain"),
                        Err(e) => tracing::debug!(at = b_start, "archive boundary not reconciled yet: {e}"),
                    }
                    continue;
                }
                match heal_gap(&store, &net, a_end + 1, b_start - 1).await {
                    Ok(()) => tracing::info!(from = a_end + 1, to = b_start - 1, "archive gap healed from eth/71"),
                    Err(e) if e.to_string().contains("does not link to the previous segment") => {
                        match reconcile(&store, &net, a_start, a_end, b_start).await {
                            Ok(n) => tracing::info!(from = n, to = a_end, "archive: orphaned blocks removed; gap will be refilled from the current chain"),
                            Err(e) => tracing::debug!(at = a_end, "archive boundary not reconciled yet: {e}"),
                        }
                    }
                    Err(e) => tracing::debug!(from = a_end + 1, to = b_start - 1, "archive gap not healed yet: {e}"),
                }
            }
        }
    });
}

async fn heal_gap(store: &Store, net: &ElNet, from: u64, to: u64) -> anyhow::Result<()> {
    use reth_eth_wire_types::BlockHashOrNumber;
    let ar = store.archive().ok_or_else(|| anyhow::anyhow!("no archive"))?;
    // The key set this job covers, fixed now: keys archived during the scan wait for the next.
    let scope = ar.archived_keys()?;
    let first_parent = ar.hash(from - 1)?.ok_or_else(|| anyhow::anyhow!("no hash at {}", from - 1))?;
    let peers: Vec<_> = net.peers().into_iter().filter(|(_, p)| p.eth_version >= 71).map(|(id, _)| id).collect();
    if peers.is_empty() {
        anyhow::bail!("no EL peers yet");
    }
    // Collect (header, writes, code) for the whole gap, verified, before touching the archive.
    let mut collected: Vec<(alloy_consensus::Header, Vec<(skim_bal::StateKey, alloy_primitives::U256)>, Vec<u8>)> = Vec::new();
    let mut next = from;
    'outer: while next <= to {
        let count = (to - next + 1).min(64);
        for &peer in &peers {
            let Ok(hs) = net.headers(peer, BlockHashOrNumber::Number(next), count).await.0 else { continue };
            if hs.is_empty() || hs[0].number != next {
                continue;
            }
            let hashes: Vec<alloy_primitives::B256> = hs.iter().map(|h| h.hash_slow()).collect();
            let Ok(bals) = net.bals(peer, hashes).await.0 else { continue };
            for (h, raw) in hs.into_iter().zip(bals.0) {
                let raw = raw.ok_or_else(|| anyhow::anyhow!("BAL {} unavailable", h.number))?;
                let bal = skim_bal::decode_verified(&raw, h.block_access_list_hash.unwrap_or_default())?;
                let w = BlockWrites::from_bal(&bal);
                for (ch, c) in &w.code {
                    if w.writes.iter().any(|(k, v)| scope.contains(k) && alloy_primitives::B256::from(*v) == *ch) {
                        ar.put_code(*ch, c)?;
                    }
                }
                let aw: Vec<_> = w.writes.iter().filter(|(k, _)| scope.contains(k)).copied().collect();
                let rlp = alloy_rlp::encode(&h);
                next = h.number + 1;
                collected.push((h, aw, rlp));
            }
            continue 'outer;
        }
        anyhow::bail!("no EL peer served headers+BALs from {next}");
    }
    // The current chain must continue the archived one at `from - 1`, or the archive holds a
    // fork the chain has left: reconciliation, not a gap fill.
    if collected.first().is_some_and(|(h, _, _)| h.parent_hash != first_parent) {
        anyhow::bail!("first block does not link to the previous segment");
    }
    let blocks: Vec<skim_state::archive::ArchiveBlock<'_>> = collected
        .iter()
        .map(|(h, aw, rlp)| skim_state::archive::ArchiveBlock { number: h.number, hash: h.hash_slow(), parent_hash: h.parent_hash, header_rlp: rlp, writes: aw })
        .collect();
    // Links inside the gap and at both ends, the key scope and the segment layout are checked in
    // the archive's committing transaction.
    ar.fill_gap(&blocks, &scope)?;
    Ok(())
}

/// The archive's segment `a_start..=a_end` is followed by blocks (from `b_start`) of a chain
/// that doesn't continue it. Walks the current chain back from block `b_start`'s parent with EL
/// headers to the newest block the archive agrees with, and removes the archived blocks after
/// it (the orphaned fork). Returns the first removed block. The gap left behind is filled by the
/// normal gap fill, with the current chain's blocks.
async fn reconcile(store: &Store, net: &ElNet, a_start: u64, a_end: u64, b_start: u64) -> anyhow::Result<u64> {
    use reth_eth_wire_types::BlockHashOrNumber;
    let ar = store.archive().ok_or_else(|| anyhow::anyhow!("no archive"))?;
    let rlp = ar.header_rlp(b_start)?.ok_or_else(|| anyhow::anyhow!("no header at {b_start}"))?;
    let mut want = <alloy_consensus::Header as alloy_rlp::Decodable>::decode(&mut rlp.as_slice())?.parent_hash;
    let mut n = b_start - 1;
    let peers: Vec<_> = net.peers().into_iter().filter(|(_, p)| p.eth_version >= 69).map(|(id, _)| id).collect();
    anyhow::ensure!(!peers.is_empty(), "no EL peers yet");
    // Above a_end the archive has nothing to compare (a gap): walk down to a_end first.
    let ancestor = 'walk: loop {
        let mut progressed = false;
        for &peer in &peers {
            let Ok(hs) = net.headers_falling(peer, BlockHashOrNumber::Hash(want), 64).await.0 else { continue };
            if hs.is_empty() {
                continue;
            }
            for h in hs {
                // Each header must be the one asked for (by hash and height): a linked walk.
                if h.number != n || h.hash_slow() != want {
                    break;
                }
                progressed = true;
                if h.number <= a_end && ar.hash(h.number)? == Some(want) {
                    break 'walk h.number;
                }
                if h.number <= a_start {
                    break 'walk a_start - 1; // the whole segment is on the abandoned fork
                }
                want = h.parent_hash;
                n -= 1;
            }
            if progressed {
                break;
            }
        }
        anyhow::ensure!(progressed, "no EL peer served headers back from {n}");
    };
    anyhow::ensure!(ancestor < a_end, "the archive agrees with the chain at {a_end}; nothing to reconcile");
    ar.truncate_range(ancestor + 1, a_end)?;
    Ok(ancestor + 1)
}

/// M4.2: scans BALs from `from` up to the latest coverage start of any archived key and extends
/// coverage backwards (see `Archive::backfill`).
async fn backfill_archive(store: &Store, net: &ElNet, from: u64) -> anyhow::Result<skim_state::archive::BackfillReport> {
    use reth_eth_wire_types::BlockHashOrNumber;
    let ar = store.archive().ok_or_else(|| anyhow::anyhow!("no archive"))?;
    // The key set this job covers, fixed now (see `Archive::backfill`).
    let scope = ar.archived_keys()?;
    let segs = ar.segments()?;
    let (s, e) = *segs.first().ok_or_else(|| anyhow::anyhow!("empty archive"))?;
    let c_max = ar.coverages()?.iter().filter(|(_, _, ext)| *ext).map(|(_, c, _)| *c).max().unwrap_or(s);
    // Include block c itself: a base is c's post-state, and c may have changed the key.
    let scan_end = c_max.max(s.saturating_sub(1)).min(e);
    if from > scan_end {
        return Ok(Default::default());
    }
    let s_parent = match ar.header_rlp(s)? {
        Some(rlp) => Some(<alloy_consensus::Header as alloy_rlp::Decodable>::decode(&mut rlp.as_slice())?.parent_hash),
        None => None,
    };
    let peers: Vec<_> = net.peers().into_iter().filter(|(_, p)| p.eth_version >= 71).map(|(id, _)| id).collect();
    anyhow::ensure!(!peers.is_empty(), "no EL peers yet");
    let mut collected: Vec<(u64, alloy_primitives::B256, Vec<u8>, Vec<(skim_bal::StateKey, alloy_primitives::U256)>, alloy_primitives::B256)> = Vec::new();
    let mut next = from;
    'outer: while next <= scan_end {
        let count = (scan_end - next + 1).min(32);
        for &peer in &peers {
            let Ok(hs) = net.headers(peer, BlockHashOrNumber::Number(next), count).await.0 else { continue };
            if hs.is_empty() || hs[0].number != next {
                continue;
            }
            let hashes: Vec<alloy_primitives::B256> = hs.iter().map(|h| h.hash_slow()).collect();
            let Ok(bals) = net.bals(peer, hashes).await.0 else { continue };
            let mut progressed = false;
            for (h, raw) in hs.into_iter().zip(bals.0) {
                let Some(raw) = raw else { break }; // pruned or truncated: retry from here
                let bal = skim_bal::decode_verified(&raw, h.block_access_list_hash.unwrap_or_default())?;
                let w = BlockWrites::from_bal(&bal);
                let aw: Vec<_> = w.writes.iter().filter(|(k, _)| scope.contains(k)).copied().collect();
                next = h.number + 1;
                progressed = true;
                collected.push((h.number, h.hash_slow(), alloy_rlp::encode(&h), aw, h.parent_hash));
            }
            if progressed {
                continue 'outer;
            }
        }
        anyhow::bail!("no EL peer served headers+BALs from {next}");
    }
    for w in collected.windows(2) {
        anyhow::ensure!(w[1].4 == w[0].1, "backfill chain does not link at {}", w[1].0);
    }
    let blocks: Vec<skim_state::archive::ArchiveBlock<'_>> = collected
        .iter()
        .map(|(n, h, rlp, aw, p)| skim_state::archive::ArchiveBlock { number: *n, hash: *h, parent_hash: *p, header_rlp: rlp, writes: aw })
        .collect();
    Ok(ar.backfill(&blocks, s_parent, &scope)?)
}
