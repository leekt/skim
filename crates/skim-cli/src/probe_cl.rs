//! M0 Spike A (PROJECT.md §3.2, §10): join Sepolia CL gossip with skim's own minimal stack and
//! measure when `SignedExecutionPayloadEnvelope`s (and beacon blocks) arrive.
//!
//! For each envelope it records arrival lag from slot start, decodes the payload fields and the
//! raw BAL, rebuilds the EL header (BAL hash included) and checks it against `block_hash`, and
//! checks `block_hash` against the bid in the beacon block of the same slot. Peer sessions are
//! logged with duration and goodbye reason to see whether peers keep a listen-mostly node.

use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use skim_cl::{
    ClEvent, ClNode, NodeConfig,
    node::SECONDS_PER_SLOT,
    rpc::goodbye_reason,
    ssz::{self, BidInfo, BlockInfo, Envelope},
    verify,
};

use crate::stats::Samples;

#[derive(clap::Args)]
pub struct Args {
    /// How long to run, in seconds.
    #[arg(long, default_value_t = 600)]
    duration: u64,
    /// libp2p TCP and discv5 UDP port (QUIC listens on port+1).
    #[arg(long, default_value_t = 9110)]
    port: u16,
    /// Peers to keep connected.
    #[arg(long, default_value_t = 50)]
    target_peers: usize,
    /// Fork digest (hex) for ENR filtering, topics and Status. Default: Sepolia Gloas.
    #[arg(long, default_value = "669e6c11")]
    fork_digest: String,
    /// Do not forward (Accept) parsed gossip; report Ignore instead.
    #[arg(long)]
    no_forward: bool,
    /// JSONL file with every sample.
    #[arg(long, default_value = "data/probe-cl.jsonl")]
    out: PathBuf,
    /// Save the first N decompressed envelopes / blocks as `<slot>.{envelope,block}.ssz` here.
    #[arg(long)]
    capture_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 16)]
    capture_n: usize,
    /// Extra peer to keep dialed (multiaddr with /p2p/<peer id>). Repeatable. For diagnostics.
    #[arg(long = "peer")]
    peers: Vec<String>,
}

#[derive(Default)]
struct Stats {
    envelopes: u64,
    envelope_parse_err: u64,
    blocks: u64,
    block_parse_err: u64,
    envelope_lag: Samples,
    block_lag: Samples,
    block_to_envelope: Samples,
    verified_ok: u64,
    verified_fail: u64,
    bal_decode_fail: u64,
    bid_match: u64,
    bid_mismatch: u64,
    bid_unseen: u64,
    timestamp_mismatch: u64,
    verify_ms: Samples,
    sessions_closed: u64,
    session_secs: Samples,
    goodbyes: BTreeMap<String, u64>,
    our_goodbyes: u64,
    connects: u64,
    peers_first_seen: Option<f64>,
}

fn unix_ms(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH).unwrap().as_secs_f64() * 1e3
}

fn parse_digest(s: &str) -> anyhow::Result<[u8; 4]> {
    let s = s.trim_start_matches("0x");
    anyhow::ensure!(s.len() == 8, "fork digest must be 4 bytes hex");
    let mut d = [0u8; 4];
    for (i, b) in d.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)?;
    }
    Ok(d)
}

pub async fn run(args: Args) -> anyhow::Result<()> {
    if let Some(dir) = args.out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    if let Some(dir) = &args.capture_dir {
        std::fs::create_dir_all(dir)?;
    }
    let mut out = BufWriter::new(File::create(&args.out)?);
    let mut log = |v: serde_json::Value| {
        let _ = serde_json::to_writer(&mut out, &v);
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    };

    let mut cfg = NodeConfig::sepolia(args.port);
    cfg.fork_digest = parse_digest(&args.fork_digest)?;
    cfg.target_peers = args.target_peers;
    cfg.max_peers = args.target_peers + args.target_peers / 2;
    cfg.forward = !args.no_forward;
    cfg.deliver_unchecked = true; // counts envelopes that fail the rebuild
    cfg.explicit_peers = args.peers.iter().map(|p| p.parse()).collect::<Result<_, _>>()?;
    let genesis = cfg.genesis_time;
    let slot_start_ms = |slot: u64| (genesis + slot * SECONDS_PER_SLOT) as f64 * 1e3;

    let (node, mut rx) = ClNode::start(cfg).await?;
    {
        let s = node.stats.lock().unwrap();
        tracing::info!(peer_id = %s.peer_id, enr = %s.local_enr, out = %args.out.display(), "probe-cl started");
    }

    let started = Instant::now();
    let deadline = started + Duration::from_secs(args.duration);
    let mut stats = Stats::default();
    // Per slot: block arrival (unix ms) and bid, for envelope linkage.
    let mut blocks: HashMap<u64, (f64, Option<BidInfo>)> = HashMap::new();
    let mut captured = (0usize, 0usize);
    let mut last_report = Instant::now();

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if last_report.elapsed() > Duration::from_secs(30) {
            report(&node, &stats, &mut log, false);
            last_report = Instant::now();
        }
        let wait = (deadline - now).min(Duration::from_secs(1));
        let Ok(Some(ev)) = tokio::time::timeout(wait, rx.recv()).await else { continue };
        match ev {
            ClEvent::Gossip { topic, data, received, from, .. } if topic == "beacon_block" => {
                let recv_ms = unix_ms(received);
                match BlockInfo::parse(&data) {
                    Ok(b) => {
                        stats.blocks += 1;
                        let lag = recv_ms - slot_start_ms(b.slot);
                        stats.block_lag.push_ms(lag);
                        blocks.insert(b.slot, (recv_ms, b.bid.clone()));
                        if let Some(dir) = &args.capture_dir {
                            if captured.1 < args.capture_n {
                                captured.1 += 1;
                                std::fs::write(dir.join(format!("{}.block.ssz", b.slot)), &*data)?;
                            }
                        }
                        tracing::info!(slot = b.slot, lag_ms = lag as i64, "beacon block");
                        log(serde_json::json!({
                            "ev": "block", "slot": b.slot, "lag_ms": lag, "recv_unix_ms": recv_ms,
                            "proposer": b.proposer_index, "parent_root": b.parent_root,
                            "bid_block_hash": b.bid.as_ref().map(|x| x.block_hash),
                            "bid_builder": b.bid.as_ref().map(|x| x.builder_index),
                            "bytes": data.len(), "from": from.to_string(),
                        }));
                    }
                    Err(e) => {
                        stats.block_parse_err += 1;
                        tracing::warn!(%e, bytes = data.len(), "beacon block parse failed");
                    }
                }
            }
            ClEvent::Gossip { topic, data, received, from, .. } if topic == "execution_payload" => {
                let recv_ms = unix_ms(received);
                let env = match Envelope::parse(&data) {
                    Ok(e) => e,
                    Err(e) => {
                        stats.envelope_parse_err += 1;
                        tracing::warn!(%e, bytes = data.len(), "envelope parse failed");
                        continue;
                    }
                };
                stats.envelopes += 1;
                let p = &env.payload;
                let slot = p.slot_number;
                let lag = recv_ms - slot_start_ms(slot);
                stats.envelope_lag.push_ms(lag);
                if p.timestamp != genesis + slot * SECONDS_PER_SLOT {
                    stats.timestamp_mismatch += 1;
                }

                let t0 = Instant::now();
                let v = verify::verify_payload(&env);
                let bal = skim_bal::decode(p.block_access_list);
                stats.verify_ms.push(t0.elapsed());
                if v.ok { stats.verified_ok += 1 } else { stats.verified_fail += 1 }
                let bal_accounts = match &bal {
                    Ok(b) => Some(b.len()),
                    Err(_) => {
                        stats.bal_decode_fail += 1;
                        None
                    }
                };

                let (bid_match, block_to_env) = match blocks.get(&slot) {
                    Some((bms, Some(bid))) => {
                        if bid.block_hash == p.block_hash { stats.bid_match += 1 } else { stats.bid_mismatch += 1 }
                        (Some(bid.block_hash == p.block_hash), Some(recv_ms - bms))
                    }
                    Some((bms, None)) => (None, Some(recv_ms - bms)),
                    None => {
                        stats.bid_unseen += 1;
                        (None, None)
                    }
                };
                if let Some(d) = block_to_env {
                    stats.block_to_envelope.push_ms(d);
                }
                if let Some(dir) = &args.capture_dir {
                    if captured.0 < args.capture_n {
                        captured.0 += 1;
                        std::fs::write(dir.join(format!("{slot}.envelope.ssz")), &*data)?;
                    }
                }
                if !v.ok {
                    tracing::warn!(slot, computed = %v.computed_hash, expected = %p.block_hash, "EL header rebuild mismatch");
                }
                tracing::info!(
                    slot, number = p.block_number, lag_ms = lag as i64, txs = p.transactions.len(),
                    bal_bytes = p.block_access_list.len(), bal_accounts, verified = v.ok, ?bid_match,
                    "envelope"
                );
                log(serde_json::json!({
                    "ev": "envelope", "slot": slot, "lag_ms": lag, "recv_unix_ms": recv_ms,
                    "block_number": p.block_number, "block_hash": p.block_hash,
                    "parent_hash": p.parent_hash, "state_root": p.state_root, "timestamp": p.timestamp,
                    "beacon_block_root": env.beacon_block_root, "builder_index": env.builder_index,
                    "txs": p.transactions.len(), "withdrawals": p.withdrawals.len(), "gas_used": p.gas_used,
                    "bal_bytes": p.block_access_list.len(), "bal_hash": v.bal_hash, "bal_accounts": bal_accounts,
                    "header_verified": v.ok, "computed_hash": v.computed_hash, "bid_match": bid_match,
                    "block_to_envelope_ms": block_to_env, "bytes": data.len(), "from": from.to_string(),
                }));
                blocks.retain(|s, _| *s + 64 > slot);
            }
            ClEvent::Gossip { .. } => {}
            ClEvent::PeerConnected { peer, dialer, addr } => {
                stats.connects += 1;
                if stats.peers_first_seen.is_none() {
                    stats.peers_first_seen = Some(started.elapsed().as_secs_f64());
                }
                log(serde_json::json!({"ev": "connect", "peer": peer.to_string(), "dialer": dialer,
                    "addr": addr.to_string(), "t": started.elapsed().as_secs_f64()}));
            }
            ClEvent::PeerStatus { peer, status, relevant } => {
                log(serde_json::json!({"ev": "status", "peer": peer.to_string(), "relevant": relevant,
                    "head_slot": status.head_slot, "finalized_epoch": status.finalized_epoch,
                    "fork_digest": ssz::hex(&status.fork_digest)}));
            }
            ClEvent::PeerDisconnected(s) => {
                stats.sessions_closed += 1;
                stats.session_secs.push(s.duration);
                if let Some(g) = s.goodbye {
                    *stats.goodbyes.entry(format!("{g}:{}", goodbye_reason(g))).or_default() += 1;
                }
                if s.our_goodbye.is_some() {
                    stats.our_goodbyes += 1;
                }
                log(serde_json::json!({"ev": "session", "peer": s.peer.to_string(), "dialer": s.dialer,
                    "secs": s.duration.as_secs_f64(), "agent": s.agent, "goodbye": s.goodbye,
                    "our_goodbye": s.our_goodbye, "cause": s.cause, "got_status": s.got_status,
                    "gossip_first": s.gossip_first, "t": started.elapsed().as_secs_f64()}));
            }
        }
    }
    report(&node, &stats, &mut log, true);
    Ok(())
}

fn report(node: &ClNode, s: &Stats, log: &mut impl FnMut(serde_json::Value), final_: bool) {
    let n = node.stats.lock().unwrap().clone();
    let mut ages = Samples::default();
    for a in &n.connected_ages_s {
        ages.push_ms(a * 1e3);
    }
    let summary = serde_json::json!({
        "final": final_,
        "peers": {"connected": n.connected, "outbound": n.outbound, "inbound": n.inbound,
                  "mesh": n.mesh, "topic_peers": n.topic_peers, "connects": s.connects,
                  "first_peer_after_s": s.peers_first_seen,
                  "connected_session_age": ages.json(),
                  "connected_over_300s": n.connected_ages_s.iter().filter(|a| **a > 300.0).count(),
                  "dials": n.dials, "dial_failures": n.dial_failures,
                  "discovered": n.discovered, "discovered_matching": n.discovered_matching},
        "sessions_closed": {"n": s.sessions_closed, "secs": s.session_secs.json(),
                            "goodbyes": s.goodbyes, "our_goodbyes": s.our_goodbyes},
        "rpc": {"status_in": n.status_in, "status_out_ok": n.status_out_ok,
                "status_out_fail": n.status_out_fail, "irrelevant": n.irrelevant,
                "metadata_served": n.metadata_served, "pings_served": n.pings_served,
                "empty_served": n.empty_served, "goodbyes_received": n.goodbyes_received,
                "attestations_relayed": n.attestations,
                "best_head_slot": n.best_status.map(|b| b.head_slot)},
        "envelopes": {"n": s.envelopes, "parse_err": s.envelope_parse_err, "lag": s.envelope_lag.json(),
                      "header_verified": s.verified_ok, "header_mismatch": s.verified_fail,
                      "bal_decode_fail": s.bal_decode_fail, "bid_match": s.bid_match,
                      "bid_mismatch": s.bid_mismatch, "block_unseen": s.bid_unseen,
                      "timestamp_mismatch": s.timestamp_mismatch, "verify_ms": s.verify_ms.json(),
                      "block_to_envelope": s.block_to_envelope.json()},
        "blocks": {"n": s.blocks, "parse_err": s.block_parse_err, "lag": s.block_lag.json()},
    });
    tracing::info!(
        "peers={} (out {} in {}) mesh={:?} | envelopes {} lag {} verified={}/{} bid_match={} | blocks lag {} | closed sessions {} {}",
        n.connected, n.outbound, n.inbound, n.mesh, s.envelopes, s.envelope_lag.summary(), s.verified_ok,
        s.envelopes, s.bid_match, s.block_lag.summary(), s.sessions_closed, s.session_secs.summary()
    );
    log(serde_json::json!({"ev": "summary", "data": summary}));
    if final_ {
        println!("{}", serde_json::to_string_pretty(&summary).unwrap());
    }
}
