//! The listener node: discv5 for eth2 peers, libp2p (TCP+noise+yamux, QUIC listen) with
//! gossipsub and the minimal req/resp set. Runs in its own task and hands gossip and peer events
//! to the caller over a channel.

use std::{
    collections::{HashMap, HashSet},
    net::Ipv4Addr,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use alloy_primitives::{B256, Bytes};
use discv5::{
    ConfigBuilder, Discv5, Enr, ListenConfig,
};
use enr::{CombinedKey, CombinedPublicKey, EnrPublicKey, NodeId};
use futures::StreamExt;
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, Swarm, gossipsub, identify,
    identity::{self, Keypair},
    multiaddr::Protocol,
    request_response::{self, OutboundRequestId, ProtocolSupport},
    swarm::{NetworkBehaviour, SwarmEvent, dial_opts::{DialOpts, PeerCondition}},
};
use tokio::sync::mpsc;

use crate::{
    gossip::{self, SnappyTransform},
    rpc::{self, Chunk, Codec, MetaData, Status},
    ssz,
};

/// Sepolia constants (consensus `config.yaml`).
pub const SEPOLIA_GENESIS_TIME: u64 = 1655733600;
pub const SECONDS_PER_SLOT: u64 = 12;
pub const SLOTS_PER_EPOCH: u64 = 32;
/// Gloas fork digest on Sepolia (checked against a local Lighthouse's topics and ENR).
pub const SEPOLIA_GLOAS_DIGEST: [u8; 4] = [0x66, 0x9e, 0x6c, 0x11];
/// `ENRForkID.next_fork_version` / `next_fork_epoch` with no fork scheduled after Gloas.
pub const SEPOLIA_GLOAS_VERSION: [u8; 4] = [0x90, 0x00, 0x00, 0x76];
pub const FAR_FUTURE_EPOCH: u64 = u64::MAX;
pub const CUSTODY_REQUIREMENT: u64 = 4;
pub const ATTESTATION_SUBNET_COUNT: usize = 64;
/// SSZ size of `SingleAttestation` (the attestation-subnet message since Electra).
pub const SINGLE_ATTESTATION_LEN: usize = 240;

/// Sepolia CL bootnodes (eth-clients/sepolia `metadata/bootstrap_nodes.yaml`).
pub const SEPOLIA_BOOTNODES: &[&str] = &[
    "enr:-KG4QCK5YeEoL55e2hoS6nCregwx0Zd6NQ3rhVDfeg5Q8ozUNmUYTskpmuqo2WYFo3z24-cWC9qrU3yYKDSJ299lh8sBgmlkgnY0gmlwhNRj2kKDaXA2kCoAHKALAA0CAAAAAAAAAF6Jc2VjcDI1NmsxoQLzqnTxu_nlM8V_semraAjfbH9HZpcbVUCXH2qanVPsroN1ZHCCTruEdWRwNoJOuw",
    "enr:-KG4QF0FvRL7Eqc4oURFhOkS0V6guntLnw54dYgTruM7z9TAMWhRpCrxZ7Pd536-q4qlwdW13czht8_UEWwGyJesu1gBgmlkgnY0gmlwhIHUpj2DaXA2kCYEqIAABAHQAAAAA2GdUACJc2VjcDI1NmsxoQL5iA7gNCs4SDmnXz8Isacq0EJbJfvV_uJlccoHxHU5ZYN1ZHCCI4yEdWRwNoIjjA",
    "enr:-KG4QI4reJ1D_BwCwg6EKAuo2HEWoIVVNjphtOTJP2gzPVLSTYM3NFwp39TAKw-7QiQ2NVts7DK4rjJR2BEcAwh3BckBgmlkgnY0gmlwhJB-_BiDaXA2kCQAYYABAADQAAAAAYEgYAGJc2VjcDI1NmsxoQLXzHa5K0M3F4pqErIhleMByA8votAUhUXylRT6SWX2HoN1ZHCCI4yEdWRwNoIjjA",
    "enr:-KG4QI1KOrogxK8u3Oc0QLdgkNTbAPuAMtixa6Vx05N-Bl7IOCVURUvqZ2N6JA97ts7YG1B4D3hQvZ9uQlCPYVjy1DABgmlkgnY0gmlwhLKc14yDaXA2kCoBBP8A9DxKAAAAAAAAAAGJc2VjcDI1NmsxoQLB0ZhHGRmVwXja_4o-GRN1VVJYRI11F45CTAlu1s00Q4N1ZHCCI4yEdWRwNoIjjA",
    "enr:-KG4QKU4YfXfB3_BVI7u0VvXnSJI6cqo-tCRm-Ggh3XxBImcYvrUoKUDbIjJjG9-QphuH6gzScdf69t597M0nHut4kABgmlkgnY0gmlwhAXfXlGDaXA2kCoBBP8C8BytAAAAAAAAAAGJc2VjcDI1NmsxoQL0y83XKpPgvY7XReWg9S8bdI2UUIe5dE0N7rjOIIj4xYN1ZHCCI4yEdWRwNoIjjA",
    "enr:-Iu4QKvMF7Ne_RSQoZGvavTuZ1QA5_Pgeb0nq_hrjhU8s0UDV3KhcMXJkGwOWhsDGZL3ISjL0CTP-hfoTjZtEtCEwR4BgmlkgnY0gmlwhAOAaySJc2VjcDI1NmsxoQNta5b_bexSSwwrGW2Re24MjfMntzFd0f2SAxQtMj3ueYN0Y3CCIyiDdWRwgiMo",
    "enr:-KG4QJejf8KVtMeAPWFhN_P0c4efuwu1pZHELTveiXUeim6nKYcYcMIQpGxxdgT2Xp9h-M5pr9gn2NbbwEAtxzu50Y8BgmlkgnY0gmlwhEEVkQCDaXA2kCoBBPnAEJg4AAAAAAAAAAGJc2VjcDI1NmsxoQLEh_eVvk07AQABvLkTGBQTrrIOQkzouMgSBtNHIRUxOIN1ZHCCIyiEdWRwNoIjKA",
];

#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub fork_digest: [u8; 4],
    pub genesis_time: u64,
    /// TCP and discv5 UDP port.
    pub port: u16,
    /// QUIC UDP port (0 disables QUIC listening).
    pub quic_port: u16,
    pub target_peers: usize,
    pub max_peers: usize,
    /// Gossipsub mesh degree (D) per topic; D_lo = 2D/3, D_hi = 3D/2. More mesh peers means more
    /// eager-push paths, so a payload arrives over the fastest of more routes.
    pub mesh_n: usize,
    pub bootnodes: Vec<String>,
    /// Status to advertise before any peer's status has been learned.
    pub initial_status: Option<Status>,
    /// Gossip topic names to subscribe to (e.g. `beacon_block`, `execution_payload`).
    pub topics: Vec<String>,
    /// Report parsed messages as `Accept` (forwarded) rather than `Ignore`.
    pub forward: bool,
    /// Peers to dial directly and redial when they drop (multiaddrs ending in `/p2p/<id>`).
    pub explicit_peers: Vec<Multiaddr>,
    /// Number of attestation subnets to join and advertise (ENR + MetaData `attnets`).
    /// Lighthouse prunes peers without a long-lived subnet first when it is over its peer target
    /// (`prune_excess_peers`), so a pure listener gets dropped within one heartbeat.
    pub attestation_subnets: usize,
}

impl NodeConfig {
    pub fn sepolia(port: u16) -> Self {
        Self {
            fork_digest: SEPOLIA_GLOAS_DIGEST,
            genesis_time: SEPOLIA_GENESIS_TIME,
            port,
            quic_port: port + 1,
            target_peers: 50,
            max_peers: 70,
            mesh_n: 8,
            bootnodes: SEPOLIA_BOOTNODES.iter().map(|s| s.to_string()).collect(),
            initial_status: None,
            topics: vec![
                "beacon_block".into(),
                "execution_payload".into(),
                "light_client_finality_update".into(),
                "light_client_optimistic_update".into(),
            ],
            forward: true,
            explicit_peers: Vec::new(),
            attestation_subnets: 2,
        }
    }

    pub fn current_slot(&self) -> u64 {
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        now.saturating_sub(self.genesis_time) / SECONDS_PER_SLOT
    }
}

#[derive(Debug, Clone)]
pub enum ClEvent {
    Gossip {
        topic: String,
        /// Snappy-decompressed SSZ.
        data: Arc<Vec<u8>>,
        received: SystemTime,
        from: PeerId,
        message_id: Vec<u8>,
    },
    PeerConnected { peer: PeerId, dialer: bool, addr: Multiaddr },
    PeerDisconnected(SessionEnd),
    PeerStatus { peer: PeerId, status: Status, relevant: bool },
}

#[derive(Debug, Clone)]
pub struct SessionEnd {
    pub peer: PeerId,
    pub dialer: bool,
    pub duration: Duration,
    pub agent: Option<String>,
    /// Goodbye reason the peer sent us, if any.
    pub goodbye: Option<u64>,
    /// Goodbye we sent (e.g. irrelevant network).
    pub our_goodbye: Option<u64>,
    pub cause: Option<String>,
    pub got_status: bool,
    pub gossip_first: u64,
}

/// One connected consensus peer.
#[derive(Debug, Clone)]
pub struct ClPeer {
    pub id: String,
    /// libp2p identify agent, e.g. "Lighthouse/v8.3.0-…".
    pub agent: Option<String>,
    /// We dialed it (else it dialed us).
    pub outbound: bool,
    pub age_s: f64,
    /// Gossip messages it delivered to us first.
    pub gossip_first: u64,
    pub head_slot: Option<u64>,
}

/// Counters readable while the node runs.
#[derive(Debug, Default, Clone)]
pub struct NodeStats {
    pub connected: usize,
    pub outbound: usize,
    pub inbound: usize,
    pub mesh: HashMap<String, usize>,
    pub topic_peers: HashMap<String, usize>,
    /// Unique ENRs seen by discovery, and how many carry our fork digest.
    pub discovered: u64,
    pub discovered_matching: u64,
    /// Ages (seconds) of the currently connected peers' sessions.
    pub connected_ages_s: Vec<f64>,
    pub dials: u64,
    pub dial_failures: u64,
    pub status_in: u64,
    pub status_out_ok: u64,
    pub status_out_fail: u64,
    pub irrelevant: u64,
    pub metadata_served: u64,
    pub pings_served: u64,
    pub empty_served: u64,
    /// `ExecutionPayloadEnvelopesByRange` / `ByRoot` requests answered, and envelopes sent.
    pub envelope_range_reqs: u64,
    pub envelope_root_reqs: u64,
    pub envelopes_served: u64,
    /// Oldest slot held for serving.
    pub envelopes_from_slot: Option<u64>,
    /// Connected peers (refreshed every 2 s).
    pub peer_list: Vec<ClPeer>,
    pub goodbyes_received: u64,
    pub attestations: u64,
    pub best_status: Option<Status>,
    pub local_enr: String,
    pub peer_id: String,
}

type SubFilter = gossipsub::MaxCountSubscriptionFilter<gossipsub::WhitelistSubscriptionFilter>;

#[derive(NetworkBehaviour)]
struct Behaviour {
    gossipsub: gossipsub::Behaviour<SnappyTransform, SubFilter>,
    identify: identify::Behaviour,
    status: request_response::Behaviour<Codec>,
    metadata: request_response::Behaviour<Codec>,
    ping: request_response::Behaviour<Codec>,
    goodbye: request_response::Behaviour<Codec>,
    empty: request_response::Behaviour<Codec>,
    envelopes: request_response::Behaviour<rpc::EnvelopesCodec>,
    envelopes_by_root: request_response::Behaviour<rpc::EnvelopesCodec>,
    lc_bootstrap: request_response::Behaviour<rpc::EnvelopesCodec>,
    lc_updates: request_response::Behaviour<rpc::EnvelopesCodec>,
}

/// Requests from the owner of a [`ClNode`] to its swarm task.
enum Cmd {
    Envelopes {
        start_slot: u64,
        count: u64,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<Vec<Vec<u8>>>>,
        /// Told which peer was asked (diagnostics), before the answer arrives.
        asked: Option<tokio::sync::oneshot::Sender<PeerId>>,
    },
    Bootstrap { root: [u8; 32], reply: tokio::sync::oneshot::Sender<anyhow::Result<Vec<Vec<u8>>>> },
    LcUpdates { start_period: u64, count: u64, reply: tokio::sync::oneshot::Sender<anyhow::Result<Vec<Vec<u8>>>> },
    Serve { slot: u64, env: crate::serve::Served },
    EnvelopesByRoot { roots: Vec<B256>, reply: tokio::sync::oneshot::Sender<anyhow::Result<Vec<Vec<u8>>>> },
}

struct PeerState {
    since: Instant,
    dialer: bool,
    agent: Option<String>,
    status: Option<Status>,
    goodbye: Option<u64>,
    our_goodbye: Option<u64>,
    gossip_first: u64,
}

/// Discovery bookkeeping bounds: dial cooldowns are pruned past this many entries, and the
/// distinct-node set behind the `discovered` stats resets at this size.
const MAX_DIAL_RECORDS: usize = 1024;
const MAX_SEEN_NODES: usize = 65_536;

#[derive(Clone)]
pub struct ClNode {
    pub stats: Arc<Mutex<NodeStats>>,
    pub local_peer_id: PeerId,
    cmd: mpsc::Sender<Cmd>,
}

impl ClNode {
    /// `ExecutionPayloadEnvelopesByRange(start_slot, count)` from one connected peer whose head
    /// is at or past the range. Returns each envelope's SSZ (unverified: callers check them as
    /// they do gossip). Empty slots and pruned ranges simply yield fewer envelopes.
    pub async fn envelopes_by_range(&self, start_slot: u64, count: u64) -> anyhow::Result<Vec<Vec<u8>>> {
        self.envelopes_by_range_from(start_slot, count, Duration::from_secs(20)).await.1
    }

    /// [`Self::envelopes_by_range`] that also reports which peer was asked (None if there was
    /// no candidate), with a caller-chosen timeout.
    pub async fn envelopes_by_range_from(&self, start_slot: u64, count: u64, timeout: Duration) -> (Option<PeerId>, anyhow::Result<Vec<Vec<u8>>>) {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let (asked, asked_rx) = tokio::sync::oneshot::channel();
        let count = count.min(rpc::MAX_REQUEST_PAYLOADS);
        if self.cmd.send(Cmd::Envelopes { start_slot, count, reply, asked: Some(asked) }).await.is_err() {
            return (None, Err(anyhow::anyhow!("CL node stopped")));
        }
        let res = match tokio::time::timeout(timeout, rx).await {
            Err(_) => Err(anyhow::anyhow!("envelopes request timed out after {} ms", timeout.as_millis())),
            Ok(Err(_)) => Err(anyhow::anyhow!("CL node dropped the request")),
            Ok(Ok(r)) => r,
        };
        (asked_rx.await.ok(), res)
    }

    /// `ExecutionPayloadEnvelopesByRoot(beacon block roots)` from one connected peer (SSZ,
    /// unverified).
    pub async fn envelopes_by_root(&self, roots: Vec<B256>) -> anyhow::Result<Vec<Vec<u8>>> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.cmd.send(Cmd::EnvelopesByRoot { roots, reply }).await.map_err(|_| anyhow::anyhow!("CL node stopped"))?;
        tokio::time::timeout(Duration::from_secs(20), rx).await.map_err(|_| anyhow::anyhow!("envelopes request timed out"))??
    }

    /// Offers a verified envelope (SSZ of `SignedExecutionPayloadEnvelope`) to peers' range/root
    /// requests. Call only for envelopes whose payload was checked and applied.
    pub fn serve_envelope(&self, slot: u64, env: crate::serve::Served) {
        let _ = self.cmd.try_send(Cmd::Serve { slot, env });
    }

    /// `LightClientBootstrap(block_root)` from one connected peer (SSZ, unverified).
    /// `LightClientUpdatesByRange(start_period, count)` from one connected peer: the SSZ of each
    /// `LightClientUpdate` (unverified; see `light::full_update`).
    pub async fn light_client_updates(&self, start_period: u64, count: u64) -> anyhow::Result<Vec<Vec<u8>>> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.cmd.send(Cmd::LcUpdates { start_period, count, reply }).await.map_err(|_| anyhow::anyhow!("CL node stopped"))?;
        tokio::time::timeout(Duration::from_secs(20), rx).await.map_err(|_| anyhow::anyhow!("light client updates timed out"))??
    }

    pub async fn light_client_bootstrap(&self, root: [u8; 32]) -> anyhow::Result<Vec<u8>> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.cmd.send(Cmd::Bootstrap { root, reply }).await.map_err(|_| anyhow::anyhow!("CL node stopped"))?;
        let mut chunks = tokio::time::timeout(Duration::from_secs(20), rx).await.map_err(|_| anyhow::anyhow!("bootstrap timed out"))???;
        chunks.pop().ok_or_else(|| anyhow::anyhow!("peer has no bootstrap for that root"))
    }
}

fn rr_config() -> request_response::Config {
    request_response::Config::default()
        .with_request_timeout(Duration::from_secs(10))
        .with_max_concurrent_streams(64)
}

fn rr(protocols: &[&'static str]) -> request_response::Behaviour<Codec> {
    request_response::Behaviour::new(
        protocols.iter().map(|p| (StreamProtocol::new(p), ProtocolSupport::Full)),
        rr_config(),
    )
}

/// The ENR `eth2` value: SSZ `ENRForkID { fork_digest, next_fork_version, next_fork_epoch }`.
pub fn enr_fork_id(digest: [u8; 4]) -> [u8; 16] {
    let mut v = [0u8; 16];
    v[..4].copy_from_slice(&digest);
    v[4..8].copy_from_slice(&SEPOLIA_GLOAS_VERSION);
    v[8..].copy_from_slice(&FAR_FUTURE_EPOCH.to_le_bytes());
    v
}

/// Fork digest from a peer's ENR, if it carries an `eth2` field.
pub fn enr_digest(enr: &Enr) -> Option<[u8; 4]> {
    let v: Bytes = enr.get_decodable("eth2")?.ok()?;
    v.get(..4)?.try_into().ok()
}

/// libp2p peer id and TCP multiaddr of a discovered ENR.
pub fn enr_dial(enr: &Enr) -> Option<(PeerId, Multiaddr)> {
    let CombinedPublicKey::Secp256k1(_) = enr.public_key() else { return None };
    let pk = identity::secp256k1::PublicKey::try_from_bytes(&enr.public_key().encode()).ok()?;
    let peer = identity::PublicKey::from(pk).to_peer_id();
    let sock = enr.tcp4_socket()?;
    let mut ma = Multiaddr::empty();
    ma.push(Protocol::Ip4(*sock.ip()));
    ma.push(Protocol::Tcp(sock.port()));
    ma.push(Protocol::P2p(peer));
    Some((peer, ma))
}

impl ClNode {
    /// Starts discv5 and the libp2p swarm in a background task.
    pub async fn start(cfg: NodeConfig) -> anyhow::Result<(Self, mpsc::Receiver<ClEvent>)> {
        let keypair = Keypair::generate_secp256k1();
        let secret = keypair.clone().try_into_secp256k1().unwrap().secret().to_bytes();
        let mut sk = secret;
        let enr_key = CombinedKey::secp256k1_from_bytes(&mut sk)
            .map_err(|e| anyhow::anyhow!("enr key: {e:?}"))?;
        let local_peer_id = keypair.public().to_peer_id();

        // Attestation subnets: a simple function of the node id (the spec's
        // `compute_subscribed_subnets` shuffles by epoch; peers do not check this).
        let node_id = Enr::empty(&enr_key).map_err(|e| anyhow::anyhow!("enr: {e:?}"))?.node_id();
        let first = node_id.raw()[0] as usize % ATTESTATION_SUBNET_COUNT;
        let subnets: Vec<usize> =
            (0..cfg.attestation_subnets).map(|i| (first + i) % ATTESTATION_SUBNET_COUNT).collect();
        let mut attnets = [0u8; 8];
        for &n in &subnets {
            attnets[n / 8] |= 1 << (n % 8);
        }
        let mut cfg = cfg;
        for &n in &subnets {
            cfg.topics.push(format!("beacon_attestation_{n}"));
        }

        let mut b = Enr::builder();
        b.udp4(cfg.port).tcp4(cfg.port);
        if cfg.quic_port != 0 {
            b.add_value("quic", &cfg.quic_port);
        }
        b.add_value("eth2", &Bytes::copy_from_slice(&enr_fork_id(cfg.fork_digest)));
        b.add_value("attnets", &Bytes::copy_from_slice(&attnets));
        b.add_value("syncnets", &Bytes::copy_from_slice(&[0u8]));
        b.add_value("cgc", &CUSTODY_REQUIREMENT);
        b.add_value("nfd", &Bytes::copy_from_slice(&[0u8; 4]));
        let enr = b.build(&enr_key).map_err(|e| anyhow::anyhow!("enr: {e:?}"))?;

        let dcfg = ConfigBuilder::new(ListenConfig::Ipv4 { ip: Ipv4Addr::UNSPECIFIED, port: cfg.port })
            .build();
        let mut discv5 = Discv5::new(enr.clone(), enr_key, dcfg).map_err(|e| anyhow::anyhow!(e))?;
        for s in &cfg.bootnodes {
            match s.parse::<Enr>() {
                Ok(e) => {
                    if let Err(err) = discv5.add_enr(e) {
                        tracing::debug!(%err, "bootnode not added");
                    }
                }
                Err(err) => tracing::warn!(%err, "bad bootnode ENR"),
            }
        }
        discv5.start().await.map_err(|e| anyhow::anyhow!("discv5 start: {e:?}"))?;

        let gcfg = gossipsub::ConfigBuilder::default()
            .max_transmit_size(gossip::MAX_PAYLOAD_SIZE)
            .heartbeat_interval(Duration::from_millis(700))
            .mesh_n(cfg.mesh_n)
            .mesh_n_low((cfg.mesh_n * 2 / 3).max(1))
            .mesh_n_high(cfg.mesh_n * 3 / 2)
            .mesh_outbound_min((cfg.mesh_n / 3).max(1))
            .gossip_lazy(6)
            .fanout_ttl(Duration::from_secs(60))
            .history_length(12)
            .history_gossip(3)
            .flood_publish(false)
            .max_publish_messages(500)
            .max_control_messages_sent(500)
            .validate_messages()
            .validation_mode(gossipsub::ValidationMode::Anonymous)
            .duplicate_cache_time(Duration::from_secs(SLOTS_PER_EPOCH * SECONDS_PER_SLOT * 2))
            .message_id_fn(gossip::gossipsub_message_id)
            .build()
            .map_err(|e| anyhow::anyhow!("gossipsub config: {e:?}"))?;
        // eth2 peers announce hundreds of topics (attestation, sync, column subnets) in one RPC;
        // the default filter (max 100) would drop those RPCs, including their subscriptions to
        // our topics. Track only our topics, but accept large subscription batches.
        let ours = cfg
            .topics
            .iter()
            .map(|t| gossipsub::IdentTopic::new(gossip::topic(cfg.fork_digest, t)).hash())
            .collect();
        let filter = gossipsub::MaxCountSubscriptionFilter {
            filter: gossipsub::WhitelistSubscriptionFilter(ours),
            max_subscribed_topics: 1024,
            max_subscriptions_per_request: 4096,
        };
        let mut gs = gossipsub::Behaviour::new_with_subscription_filter_and_transform(
            gossipsub::MessageAuthenticity::Anonymous,
            gcfg,
            filter,
            SnappyTransform,
        )
        .map_err(|e| anyhow::anyhow!(e))?;
        for t in &cfg.topics {
            gs.subscribe(&gossipsub::IdentTopic::new(gossip::topic(cfg.fork_digest, t)))?;
        }

        let empty: Vec<&'static str> = rpc::EMPTY_SERVED.to_vec();
        let behaviour = Behaviour {
            gossipsub: gs,
            identify: identify::Behaviour::new(
                identify::Config::new("eth2/1.0.0".into(), keypair.public())
                    .with_agent_version(format!("skim/{}", env!("CARGO_PKG_VERSION"))),
            ),
            status: rr(&[rpc::STATUS_V2, rpc::STATUS_V1]),
            metadata: rr(&[rpc::METADATA_V3, rpc::METADATA_V2]),
            ping: rr(&[rpc::PING_V1]),
            goodbye: rr(&[rpc::GOODBYE_V1]),
            empty: rr(&empty),
            // Both directions: skim requests ranges on catch-up and answers from `served`.
            envelopes: request_response::Behaviour::with_codec(
                rpc::EnvelopesCodec { digest: cfg.fork_digest },
                [(StreamProtocol::new(rpc::ENVELOPES_BY_RANGE), ProtocolSupport::Full)],
                request_response::Config::default().with_request_timeout(Duration::from_secs(15)),
            ),
            envelopes_by_root: request_response::Behaviour::with_codec(
                rpc::EnvelopesCodec { digest: cfg.fork_digest },
                [(StreamProtocol::new(rpc::ENVELOPES_BY_ROOT), ProtocolSupport::Full)],
                rr_config(),
            ),
            lc_bootstrap: request_response::Behaviour::new(
                [(StreamProtocol::new(rpc::LIGHT_CLIENT_BOOTSTRAP), ProtocolSupport::Outbound)],
                request_response::Config::default().with_request_timeout(Duration::from_secs(15)),
            ),
            lc_updates: request_response::Behaviour::new(
                [(StreamProtocol::new(rpc::LIGHT_CLIENT_UPDATES_BY_RANGE), ProtocolSupport::Outbound)],
                request_response::Config::default().with_request_timeout(Duration::from_secs(15)),
            ),
        };

        let mut swarm = libp2p::SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_tcp(
                libp2p::tcp::Config::default().nodelay(true),
                libp2p::noise::Config::new,
                libp2p::yamux::Config::default,
            )?
            .with_quic()
            .with_behaviour(|_| behaviour)?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();
        swarm.listen_on(format!("/ip4/0.0.0.0/tcp/{}", cfg.port).parse()?)?;
        if cfg.quic_port != 0 {
            swarm.listen_on(format!("/ip4/0.0.0.0/udp/{}/quic-v1", cfg.quic_port).parse()?)?;
        }

        let stats = Arc::new(Mutex::new(NodeStats {
            local_enr: enr.to_base64(),
            peer_id: local_peer_id.to_string(),
            best_status: cfg.initial_status,
            ..Default::default()
        }));
        let (tx, rx) = mpsc::channel(4096);
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let runner = Runner {
            cmd_rx,
            env_reqs: HashMap::new(),
            env_rr: 0,
            served: Default::default(),
            cfg,
            swarm,
            discv5: Arc::new(discv5),
            tx,
            stats: stats.clone(),
            peers: HashMap::new(),
            last_dial: HashMap::new(),
            status_reqs: HashMap::new(),
            best: None,
            metadata: MetaData {
                seq_number: 1,
                attnets,
                syncnets: 0,
                custody_group_count: CUSTODY_REQUIREMENT,
            },
            pending_disconnect: Vec::new(),
            seen_nodes: HashSet::new(),
        };
        tokio::spawn(runner.run());
        Ok((Self { stats, local_peer_id, cmd: cmd_tx }, rx))
    }
}

struct Runner {
    cmd_rx: mpsc::Receiver<Cmd>,
    env_reqs: HashMap<OutboundRequestId, tokio::sync::oneshot::Sender<anyhow::Result<Vec<Vec<u8>>>>>,
    env_rr: usize,
    served: crate::serve::EnvelopeCache,
    cfg: NodeConfig,
    swarm: Swarm<Behaviour>,
    discv5: Arc<Discv5>,
    tx: mpsc::Sender<ClEvent>,
    stats: Arc<Mutex<NodeStats>>,
    peers: HashMap<PeerId, PeerState>,
    last_dial: HashMap<PeerId, Instant>,
    status_reqs: HashMap<OutboundRequestId, PeerId>,
    best: Option<Status>,
    metadata: MetaData,
    pending_disconnect: Vec<(PeerId, Instant)>,
    seen_nodes: HashSet<NodeId>,
}

impl Runner {
    fn our_status(&self) -> Status {
        self.best.or(self.cfg.initial_status).unwrap_or(Status {
            fork_digest: self.cfg.fork_digest,
            finalized_root: B256::ZERO,
            finalized_epoch: 0,
            head_root: B256::ZERO,
            head_slot: 0,
            earliest_available_slot: Some(0),
        })
    }

    /// Adopts a peer's status as ours if it is on our fork and has the best plausible head.
    fn learn_status(&mut self, s: &Status) -> bool {
        if s.fork_digest != self.cfg.fork_digest {
            return false;
        }
        let now = self.cfg.current_slot();
        if s.head_slot > now + 1 {
            return false;
        }
        let mut b = self.best.unwrap_or(*s);
        if s.head_slot >= b.head_slot {
            b.head_slot = s.head_slot;
            b.head_root = s.head_root;
        }
        if s.finalized_epoch >= b.finalized_epoch {
            b.finalized_epoch = s.finalized_epoch;
            b.finalized_root = s.finalized_root;
        }
        // We serve no history: claim nothing older than the head.
        b.earliest_available_slot = Some(b.head_slot);
        self.best = Some(b);
        self.stats.lock().unwrap().best_status = Some(b);
        true
    }

    /// Keeps our advertised head current from gossip: an envelope names its beacon block root
    /// and (via `payload.slot_number`) the block's slot. Peers rank "synced" peers above
    /// "behind" ones when pruning, so a stale head costs sessions.
    fn advance_head(&mut self, slot: u64, root: B256) {
        if slot > self.cfg.current_slot() + 1 {
            return;
        }
        let mut s = self.our_status();
        if slot <= s.head_slot {
            return;
        }
        s.head_slot = slot;
        s.head_root = root;
        s.earliest_available_slot = Some(slot);
        self.best = Some(s);
        self.stats.lock().unwrap().best_status = Some(s);
    }

    async fn run(mut self) {
        let (found_tx, mut found_rx) = mpsc::channel::<Vec<Enr>>(16);
        let mut query_in_flight = 0usize;
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        // Discovered ENRs arrive here as queries progress; query results only come at the end
        // of a query (up to the 60 s query timeout).
        let mut disc_events = match self.discv5.event_stream().await {
            Ok(rx) => rx,
            Err(e) => {
                tracing::warn!(?e, "no discv5 event stream");
                mpsc::channel(1).1
            }
        };
        loop {
            tokio::select! {
                Some(ev) = disc_events.recv() => match ev {
                    discv5::Event::Discovered(enr) | discv5::Event::SessionEstablished(enr, _) => {
                        self.on_found(vec![enr]);
                    }
                    _ => {}
                },
                ev = self.swarm.select_next_some() => self.on_swarm(ev).await,
                Some(cmd) = self.cmd_rx.recv() => self.on_cmd(cmd),
                Some(enrs) = found_rx.recv() => {
                    query_in_flight = query_in_flight.saturating_sub(1);
                    self.on_found(enrs);
                }
                _ = tick.tick() => {
                    self.refresh_stats();
                    self.dial_explicit();
                    let due: Vec<PeerId> = self.pending_disconnect.iter()
                        .filter(|(_, t)| t.elapsed() > Duration::from_secs(1)).map(|(p, _)| *p).collect();
                    self.pending_disconnect.retain(|(_, t)| t.elapsed() <= Duration::from_secs(1));
                    for p in due {
                        let _ = self.swarm.disconnect_peer_id(p);
                    }
                    if self.peers.len() < self.cfg.target_peers && query_in_flight < 2 {
                        query_in_flight += 1;
                        let digest = self.cfg.fork_digest;
                        let fut = self.discv5.find_node_predicate(
                            NodeId::random(),
                            Box::new(move |e: &Enr| enr_digest(e) == Some(digest) && e.tcp4().is_some()),
                            32,
                        );
                        let tx = found_tx.clone();
                        tokio::spawn(async move {
                            let r = fut.await.unwrap_or_default();
                            let _ = tx.send(r).await;
                        });
                    }
                }
            }
        }
    }

    fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Envelopes { start_slot, count, reply, asked } => {
                // Peers whose head covers the range, rotated so retries spread out.
                let mut cands: Vec<PeerId> = self
                    .peers
                    .iter()
                    // Peers whose head reaches into the range (handshake heads lag the chain).
                    .filter(|(_, p)| p.status.is_some_and(|s| s.head_slot >= start_slot))
                    .map(|(id, _)| *id)
                    .collect();
                if cands.is_empty() {
                    let _ = reply.send(Err(anyhow::anyhow!("no CL peer with a head past slot {}", start_slot + count - 1)));
                    return;
                }
                cands.sort();
                self.env_rr = self.env_rr.wrapping_add(1);
                let peer = cands[self.env_rr % cands.len()];
                let mut ssz = Vec::with_capacity(16);
                ssz.extend_from_slice(&start_slot.to_le_bytes());
                ssz.extend_from_slice(&count.to_le_bytes());
                let id = self.swarm.behaviour_mut().envelopes.send_request(&peer, ssz);
                if let Some(a) = asked {
                    let _ = a.send(peer);
                }
                self.env_reqs.insert(id, reply);
            }
            Cmd::EnvelopesByRoot { roots, reply } => {
                let mut cands: Vec<PeerId> = self.peers.iter().filter(|(_, p)| p.status.is_some()).map(|(id, _)| *id).collect();
                if cands.is_empty() {
                    let _ = reply.send(Err(anyhow::anyhow!("no CL peers yet")));
                    return;
                }
                cands.sort();
                self.env_rr = self.env_rr.wrapping_add(1);
                let peer = cands[self.env_rr % cands.len()];
                let req: Vec<u8> = roots.iter().take(rpc::MAX_REQUEST_PAYLOADS as usize).flat_map(|r| r.0).collect();
                let id = self.swarm.behaviour_mut().envelopes_by_root.send_request(&peer, req);
                self.env_reqs.insert(id, reply);
            }
            Cmd::Serve { slot, env } => {
                self.served.insert(slot, env);
                self.stats.lock().unwrap().envelopes_from_slot = self.served.oldest_slot();
            }
            Cmd::LcUpdates { start_period, count, reply } => {
                let mut cands: Vec<PeerId> = self.peers.iter().filter(|(_, p)| p.status.is_some()).map(|(id, _)| *id).collect();
                if cands.is_empty() {
                    let _ = reply.send(Err(anyhow::anyhow!("no CL peers yet")));
                    return;
                }
                cands.sort();
                self.env_rr = self.env_rr.wrapping_add(1);
                let peer = cands[self.env_rr % cands.len()];
                let mut req = Vec::with_capacity(16);
                req.extend_from_slice(&start_period.to_le_bytes());
                req.extend_from_slice(&count.to_le_bytes());
                let id = self.swarm.behaviour_mut().lc_updates.send_request(&peer, req);
                self.env_reqs.insert(id, reply);
            }
            Cmd::Bootstrap { root, reply } => {
                let mut cands: Vec<PeerId> = self.peers.iter().filter(|(_, p)| p.status.is_some()).map(|(id, _)| *id).collect();
                if cands.is_empty() {
                    let _ = reply.send(Err(anyhow::anyhow!("no CL peers yet")));
                    return;
                }
                cands.sort();
                self.env_rr = self.env_rr.wrapping_add(1);
                let peer = cands[self.env_rr % cands.len()];
                let id = self.swarm.behaviour_mut().lc_bootstrap.send_request(&peer, root.to_vec());
                self.env_reqs.insert(id, reply);
            }
        }
    }

    fn on_found(&mut self, enrs: Vec<Enr>) {
        for enr in enrs {
            let matching = enr_digest(&enr) == Some(self.cfg.fork_digest);
            // Distinct nodes for the `discovered` stats: bounded, so after a reset a node may be
            // counted again (the stat is distinct nodes since the last reset).
            if self.seen_nodes.len() >= MAX_SEEN_NODES {
                self.seen_nodes.clear();
            }
            if self.seen_nodes.insert(enr.node_id()) {
                let mut s = self.stats.lock().unwrap();
                s.discovered += 1;
                s.discovered_matching += matching as u64;
            }
            if !matching {
                continue;
            }
            let Some((peer, addr)) = enr_dial(&enr) else { continue };
            if self.peers.contains_key(&peer) || self.peers.len() >= self.cfg.target_peers {
                continue;
            }
            if self.last_dial.get(&peer).is_some_and(|t| t.elapsed() < Duration::from_secs(120)) {
                continue;
            }
            if self.last_dial.len() >= MAX_DIAL_RECORDS {
                // Only cooldowns still running matter; the rest would grow with every peer seen.
                self.last_dial.retain(|_, t| t.elapsed() < Duration::from_secs(120));
            }
            self.last_dial.insert(peer, Instant::now());
            let opts = DialOpts::peer_id(peer)
                .addresses(vec![addr])
                .condition(PeerCondition::DisconnectedAndNotDialing)
                .build();
            if self.swarm.dial(opts).is_ok() {
                self.stats.lock().unwrap().dials += 1;
            }
        }
    }

    fn dial_explicit(&mut self) {
        for addr in self.cfg.explicit_peers.clone() {
            let Some(Protocol::P2p(peer)) = addr.iter().last() else { continue };
            if self.peers.contains_key(&peer)
                || self.last_dial.get(&peer).is_some_and(|t| t.elapsed() < Duration::from_secs(10))
            {
                continue;
            }
            self.last_dial.insert(peer, Instant::now());
            let opts = DialOpts::peer_id(peer)
                .addresses(vec![addr])
                .condition(PeerCondition::DisconnectedAndNotDialing)
                .build();
            if self.swarm.dial(opts).is_ok() {
                self.stats.lock().unwrap().dials += 1;
            }
        }
    }

    fn refresh_stats(&mut self) {
        let gs = &self.swarm.behaviour().gossipsub;
        let mut mesh = HashMap::new();
        let mut topic_peers = HashMap::new();
        for t in &self.cfg.topics {
            let h = gossipsub::IdentTopic::new(gossip::topic(self.cfg.fork_digest, t)).hash();
            mesh.insert(t.clone(), gs.mesh_peers(&h).count());
            topic_peers.insert(t.clone(), gs.all_peers().filter(|(_, ts)| ts.contains(&&h)).count());
        }
        let mut s = self.stats.lock().unwrap();
        s.connected = self.peers.len();
        s.outbound = self.peers.values().filter(|p| p.dialer).count();
        s.inbound = s.connected - s.outbound;
        s.mesh = mesh;
        s.topic_peers = topic_peers;
        s.connected_ages_s = self.peers.values().map(|p| p.since.elapsed().as_secs_f64()).collect();
        s.peer_list = self
            .peers
            .iter()
            .map(|(id, p)| ClPeer {
                id: id.to_string(),
                agent: p.agent.clone(),
                outbound: p.dialer,
                age_s: p.since.elapsed().as_secs_f64(),
                gossip_first: p.gossip_first,
                head_slot: p.status.map(|s| s.head_slot),
            })
            .collect();
    }

    fn send_goodbye(&mut self, peer: PeerId, reason: u64) {
        if let Some(p) = self.peers.get_mut(&peer) {
            p.our_goodbye = Some(reason);
        }
        self.swarm.behaviour_mut().goodbye.send_request(&peer, reason.to_le_bytes().to_vec());
        // Give the goodbye a moment to go out, then drop the connection (see `run`).
        self.pending_disconnect.push((peer, Instant::now()));
    }

    async fn on_swarm(&mut self, ev: SwarmEvent<BehaviourEvent>) {
        match ev {
            SwarmEvent::ConnectionEstablished { peer_id, endpoint, num_established, .. } => {
                if num_established.get() != 1 {
                    return;
                }
                let dialer = endpoint.is_dialer();
                if !dialer && self.peers.len() >= self.cfg.max_peers {
                    let _ = self.swarm.disconnect_peer_id(peer_id);
                    return;
                }
                self.peers.insert(
                    peer_id,
                    PeerState {
                        since: Instant::now(),
                        dialer,
                        agent: None,
                        status: None,
                        goodbye: None,
                        our_goodbye: None,
                        gossip_first: 0,
                    },
                );
                if dialer {
                    let st = self.our_status().encode(true);
                    let id = self.swarm.behaviour_mut().status.send_request(&peer_id, st);
                    self.status_reqs.insert(id, peer_id);
                }
                self.swarm.behaviour_mut().metadata.send_request(&peer_id, Vec::new());
                let addr = endpoint.get_remote_address().clone();
                let _ = self.tx.try_send(ClEvent::PeerConnected { peer: peer_id, dialer, addr });
            }
            SwarmEvent::ConnectionClosed { peer_id, num_established, cause, .. } => {
                if num_established != 0 {
                    return;
                }
                if let Some(p) = self.peers.remove(&peer_id) {
                    let _ = self.tx.try_send(ClEvent::PeerDisconnected(SessionEnd {
                        peer: peer_id,
                        dialer: p.dialer,
                        duration: p.since.elapsed(),
                        agent: p.agent,
                        goodbye: p.goodbye,
                        our_goodbye: p.our_goodbye,
                        cause: cause.map(|c| c.to_string()),
                        got_status: p.status.is_some(),
                        gossip_first: p.gossip_first,
                    }));
                }
            }
            SwarmEvent::OutgoingConnectionError { .. } => {
                self.stats.lock().unwrap().dial_failures += 1;
            }
            SwarmEvent::Behaviour(b) => self.on_behaviour(b).await,
            _ => {}
        }
    }

    fn on_peer_status(&mut self, peer: PeerId, s: Status) {
        let relevant = self.learn_status(&s);
        if let Some(p) = self.peers.get_mut(&peer) {
            p.status = Some(s);
        }
        let _ = self.tx.try_send(ClEvent::PeerStatus { peer, status: s, relevant });
        if s.fork_digest != self.cfg.fork_digest {
            self.stats.lock().unwrap().irrelevant += 1;
            self.send_goodbye(peer, 2);
        }
    }

    async fn on_behaviour(&mut self, ev: BehaviourEvent) {
        use request_response::{Event as R, Message as M};
        match ev {
            BehaviourEvent::Gossipsub(gossipsub::Event::Message {
                propagation_source,
                message_id,
                message,
            }) => {
                let received = SystemTime::now();
                let topic = message.topic.as_str().to_string();
                let name = topic.split('/').nth(3).unwrap_or("").to_string();
                let ok = match name.as_str() {
                    "execution_payload" => match ssz::Envelope::parse(&message.data) {
                        Ok(env) => {
                            self.advance_head(env.payload.slot_number, env.beacon_block_root);
                            true
                        }
                        Err(_) => false,
                    },
                    "beacon_block" => ssz::BlockInfo::parse(&message.data).is_ok(),
                    // Size check only here; signatures are verified by the light client.
                    "light_client_optimistic_update" => {
                        tracing::debug!(len = message.data.len(), "light_client_optimistic_update gossip");
                        message.data.len() == crate::light::OPTIMISTIC_UPDATE_LEN
                    }
                    "light_client_finality_update" => {
                        tracing::debug!(len = message.data.len(), "light_client_finality_update gossip");
                        message.data.len() == crate::light::FINALITY_UPDATE_LEN
                    }
                    // Syntactic check only: skim cannot verify attestations (no state/BLS).
                    n if n.starts_with("beacon_attestation_") => {
                        message.data.len() == SINGLE_ATTESTATION_LEN
                    }
                    _ => true,
                };
                // Light-client updates are received, not relayed: skim verifies them later.
                let acc = if ok && self.cfg.forward && !name.starts_with("light_client") {
                    gossipsub::MessageAcceptance::Accept
                } else {
                    gossipsub::MessageAcceptance::Ignore
                };
                self.swarm.behaviour_mut().gossipsub.report_message_validation_result(
                    &message_id,
                    &propagation_source,
                    acc,
                );
                if let Some(p) = self.peers.get_mut(&propagation_source) {
                    p.gossip_first += 1;
                }
                if name.starts_with("beacon_attestation_") {
                    self.stats.lock().unwrap().attestations += 1;
                    return;
                }
                let ev = ClEvent::Gossip {
                    topic: name,
                    data: Arc::new(message.data),
                    received,
                    from: propagation_source,
                    message_id: message_id.0,
                };
                if self.tx.send(ev).await.is_err() {
                    tracing::warn!("event receiver dropped");
                }
            }
            BehaviourEvent::Gossipsub(_) => {}
            BehaviourEvent::Identify(identify::Event::Received { peer_id, info, .. }) => {
                if let Some(p) = self.peers.get_mut(&peer_id) {
                    p.agent = Some(info.agent_version);
                }
            }
            BehaviourEvent::Identify(_) => {}
            BehaviourEvent::Status(R::Message { peer, message, .. }) => match message {
                M::Request { request, channel, .. } => {
                    self.stats.lock().unwrap().status_in += 1;
                    let theirs = Status::decode(&request);
                    // Answer with the best status learned so far; before anything is known,
                    // echo the requester's own status back (it is, by construction, relevant).
                    let ours = match (self.best, theirs) {
                        (Some(b), _) => b,
                        (None, Some(t)) => Status { earliest_available_slot: Some(t.head_slot), ..t },
                        (None, None) => self.our_status(),
                    };
                    let v2 = request.len() == 92;
                    let _ = self.swarm.behaviour_mut().status.send_response(
                        channel,
                        vec![Chunk { code: 0, data: ours.encode(v2) }],
                    );
                    if let Some(t) = theirs {
                        self.on_peer_status(peer, t);
                    }
                }
                M::Response { request_id, response } => {
                    self.status_reqs.remove(&request_id);
                    match response.first().filter(|c| c.code == 0).and_then(|c| Status::decode(&c.data)) {
                        Some(s) => {
                            self.stats.lock().unwrap().status_out_ok += 1;
                            self.on_peer_status(peer, s);
                        }
                        None => self.stats.lock().unwrap().status_out_fail += 1,
                    }
                }
            },
            BehaviourEvent::Status(R::OutboundFailure { request_id, error, peer, .. }) => {
                self.status_reqs.remove(&request_id);
                self.stats.lock().unwrap().status_out_fail += 1;
                tracing::debug!(%peer, %error, "status request failed");
            }
            BehaviourEvent::Metadata(R::Message { message: M::Request { channel, .. }, .. }) => {
                self.stats.lock().unwrap().metadata_served += 1;
                // The negotiated version is not visible here; v3 is what Fulu+ peers pick first.
                let data = self.metadata.encode(true);
                let _ = self.swarm.behaviour_mut().metadata.send_response(channel, vec![Chunk { code: 0, data }]);
            }
            BehaviourEvent::Ping(R::Message { message: M::Request { channel, .. }, .. }) => {
                self.stats.lock().unwrap().pings_served += 1;
                let data = self.metadata.seq_number.to_le_bytes().to_vec();
                let _ = self.swarm.behaviour_mut().ping.send_response(channel, vec![Chunk { code: 0, data }]);
            }
            BehaviourEvent::Goodbye(R::Message { peer, message: M::Request { request, .. }, .. }) => {
                let reason = request.get(..8).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
                self.stats.lock().unwrap().goodbyes_received += 1;
                if let Some(p) = self.peers.get_mut(&peer) {
                    p.goodbye = reason;
                }
                tracing::debug!(%peer, ?reason, "goodbye");
            }
            BehaviourEvent::Envelopes(R::Message { message: M::Request { request, channel, .. }, .. }) => {
                let out: Vec<Vec<u8>> = self.served.range(&request).iter().map(|e| e.to_vec()).collect();
                {
                    let mut s = self.stats.lock().unwrap();
                    s.envelope_range_reqs += 1;
                    s.envelopes_served += out.len() as u64;
                }
                let _ = self.swarm.behaviour_mut().envelopes.send_response(channel, out);
            }
            BehaviourEvent::EnvelopesByRoot(R::Message { message: M::Request { request, channel, .. }, .. }) => {
                let out: Vec<Vec<u8>> = self.served.by_root(&request).iter().map(|e| e.to_vec()).collect();
                {
                    let mut s = self.stats.lock().unwrap();
                    s.envelope_root_reqs += 1;
                    s.envelopes_served += out.len() as u64;
                }
                let _ = self.swarm.behaviour_mut().envelopes_by_root.send_response(channel, out);
            }
            BehaviourEvent::Envelopes(R::Message { message: M::Response { request_id, response }, .. }) => {
                if let Some(reply) = self.env_reqs.remove(&request_id) {
                    let _ = reply.send(Ok(response));
                }
            }
            BehaviourEvent::Envelopes(R::OutboundFailure { request_id, error, .. }) => {
                if let Some(reply) = self.env_reqs.remove(&request_id) {
                    let _ = reply.send(Err(anyhow::anyhow!("envelopes request failed: {error}")));
                }
            }
            BehaviourEvent::EnvelopesByRoot(R::Message { message: M::Response { request_id, response }, .. }) => {
                if let Some(reply) = self.env_reqs.remove(&request_id) {
                    let _ = reply.send(Ok(response));
                }
            }
            BehaviourEvent::EnvelopesByRoot(R::OutboundFailure { request_id, error, .. }) => {
                if let Some(reply) = self.env_reqs.remove(&request_id) {
                    let _ = reply.send(Err(anyhow::anyhow!("envelopes request failed: {error}")));
                }
            }
            BehaviourEvent::LcUpdates(R::Message { message: M::Response { request_id, response }, .. }) => {
                if let Some(reply) = self.env_reqs.remove(&request_id) {
                    let _ = reply.send(Ok(response));
                }
            }
            BehaviourEvent::LcUpdates(R::OutboundFailure { request_id, error, .. }) => {
                if let Some(reply) = self.env_reqs.remove(&request_id) {
                    let _ = reply.send(Err(anyhow::anyhow!("light client updates request failed: {error}")));
                }
            }
            BehaviourEvent::LcBootstrap(R::Message { message: M::Response { request_id, response }, .. }) => {
                if let Some(reply) = self.env_reqs.remove(&request_id) {
                    let _ = reply.send(Ok(response));
                }
            }
            BehaviourEvent::LcBootstrap(R::OutboundFailure { request_id, error, .. }) => {
                if let Some(reply) = self.env_reqs.remove(&request_id) {
                    let _ = reply.send(Err(anyhow::anyhow!("bootstrap request failed: {error}")));
                }
            }
            BehaviourEvent::Empty(R::Message { message: M::Request { channel, .. }, .. }) => {
                self.stats.lock().unwrap().empty_served += 1;
                let _ = self.swarm.behaviour_mut().empty.send_response(channel, Vec::new());
            }
            _ => {}
        }
    }
}
