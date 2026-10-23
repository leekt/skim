//! M0 Spike B (PROJECT.md §10): can Sepolia EL peers feed a partial-state follower?
//!
//! For each new head found by header polling it measures:
//! - header lag from slot start (`el_header_poll_lag_ms`)
//! - eth/71 BAL latency, availability and commitment checks across several peers
//! - snap/2 root-servable lag (header seen -> a peer answers at the new root)
//! - snap/2 account and storage point-lookup latency
//!
//! Self-check without a reference node: the EIP-2935 history contract stores the parent hash of
//! block n in slot (n-1) % 8191, so both the BAL of n and SNAP at root(n) must yield
//! `header(n).parent_hash`.

use std::{
    collections::HashMap,
    fs::File,
    io::{BufWriter, Write},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use alloy_consensus::Header;
use alloy_primitives::{address, keccak256, Address, B256, U256};
use alloy_rlp::Decodable;
use parking_lot::Mutex;
use reth_eth_wire_types::BlockHashOrNumber;
use skim_bal::{BlockWrites, StateKey};
use skim_el::{ElNet, Peer, ReqError};

use crate::stats::Samples;

const HISTORY: Address = address!("0000F90827F1C53a10cb7A02335B175320002935");
const HISTORY_WINDOW: u64 = 8191;

#[derive(clap::Args)]
pub struct Args {
    /// How long to run, in seconds.
    #[arg(long, default_value_t = 600)]
    duration: u64,
    /// RLPx/discovery port.
    #[arg(long, default_value_t = 30399)]
    port: u16,
    /// Peers to query in parallel for each BAL / SNAP measurement.
    #[arg(long, default_value_t = 3)]
    fanout: usize,
    /// Trusted peers (enode URLs), kept connected as the state-provider pool. Repeatable.
    #[arg(long = "trusted")]
    trusted: Vec<String>,
    /// Connect to trusted peers only (Spike B2).
    #[arg(long)]
    trusted_only: bool,
    /// Advertise each new head (Status + BlockRangeUpdate) instead of genesis.
    #[arg(long)]
    advertise_head: bool,
    /// JSONL file with every sample.
    #[arg(long, default_value = "probe-el.jsonl")]
    out: String,
}

#[derive(Default)]
struct Stats {
    blocks: u64,
    reorgs_or_gaps: u64,
    header_lag: Samples,
    header_req: Samples,
    bal_ok: Samples,
    bal_unavailable: u64,
    bal_bad: u64,
    bal_err: u64,
    bal_selfcheck_ok: u64,
    bal_selfcheck_fail: u64,
    snap_account: Samples,
    snap_root_unavailable: u64,
    snap_err: u64,
    root_servable_lag: Samples,
    root_never_served: u64,
    snap_storage: Samples,
    snap_selfcheck_ok: u64,
    snap_selfcheck_fail: u64,
}

/// Per-peer results, kept after the session closes so the pool can be chosen from them.
#[derive(Default)]
struct PeerCard {
    client: String,
    enode: String,
    bal: Samples,
    bal_fail: u32,
    snap_account: Samples,
    snap_storage: Samples,
    never_served: u32,
}

struct Ctx {
    cards: Mutex<HashMap<Peer, PeerCard>>,
    net: ElNet,
    stats: Mutex<Stats>,
    out: Mutex<BufWriter<File>>,
    fanout: usize,
}

impl Ctx {
    fn card<R>(&self, peer: Peer, f: impl FnOnce(&mut PeerCard) -> R) -> R {
        let mut cards = self.cards.lock();
        let c = cards.entry(peer).or_default();
        if c.client.is_empty() {
            if let Some((_, p)) = self.net.peers().into_iter().find(|(id, _)| *id == peer) {
                c.client = p.client;
                c.enode = format!("enode://{}@{}", hex(peer.as_slice()), p.remote_addr);
            }
        }
        f(c)
    }

    fn log(&self, v: serde_json::Value) {
        let mut o = self.out.lock();
        let _ = serde_json::to_writer(&mut *o, &v);
        let _ = o.write_all(b"\n");
        let _ = o.flush();
    }

    /// Peers negotiated at eth/71+, optionally requiring snap/2, oldest sessions first.
    fn peers(&self, snap: bool) -> Vec<Peer> {
        let mut v: Vec<_> = self
            .net
            .peers()
            .into_iter()
            .filter(|(_, p)| p.eth_version >= 71 && (!snap || p.snap2 || p.snap1))
            .collect();
        v.sort_by_key(|(_, p)| p.connected_at);
        v.into_iter().map(|(id, _)| id).collect()
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64() * 1e3
}

fn err_kind(e: &ReqError) -> &'static str {
    match e {
        ReqError::Timeout => "timeout",
        ReqError::Peer(_) => "peer",
        ReqError::Unexpected => "unexpected",
    }
}

pub async fn run(args: Args) -> anyhow::Result<()> {
    let trusted = args.trusted.iter().map(|s| s.parse()).collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("bad --trusted enode: {e:?}"))?;
    let net = ElNet::start_sepolia(args.port, trusted, args.trusted_only)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let ctx = Arc::new(Ctx {
        net,
        stats: Mutex::default(),
        cards: Mutex::default(),
        out: Mutex::new(BufWriter::new(File::create(&args.out)?)),
        fanout: args.fanout,
    });
    tracing::info!(out = %args.out, "probe-el started; discovering Sepolia peers");

    let deadline = Instant::now() + Duration::from_secs(args.duration);
    let mut head: Option<Header> = None;
    let mut rr = 0usize;
    let mut last_report = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(500));

    while Instant::now() < deadline {
        tick.tick().await;
        if last_report.elapsed() > Duration::from_secs(30) {
            report(&ctx, false);
            last_report = Instant::now();
        }

        let peers = ctx.peers(false);
        if peers.is_empty() {
            continue;
        }
        rr = rr.wrapping_add(1);
        let peer = peers[rr % peers.len()];

        let start = match &head {
            Some(h) => BlockHashOrNumber::Number(h.number + 1),
            None => {
                // Bootstrap from the highest head any peer reported in its status.
                let best = ctx.net.peers().into_iter().filter_map(|(_, p)| p.status_latest).max();
                match best {
                    Some(n) => BlockHashOrNumber::Number(n),
                    None => continue,
                }
            }
        };
        let (res, took) = ctx.net.headers(peer, start, 16).await;
        let Ok(headers) = res else { continue };
        if headers.is_empty() {
            continue;
        }
        ctx.stats.lock().header_req.push(took);

        for h in headers {
            if let Some(prev) = &head {
                if h.parent_hash != prev.hash_slow() {
                    // Probe-level handling only: the real chain tracker keeps a DAG (§4).
                    ctx.stats.lock().reorgs_or_gaps += 1;
                    tracing::warn!(number = h.number, "parent mismatch; re-anchoring head");
                    head = None;
                    break;
                }
            }
            let lag = now_ms() - (h.timestamp as f64) * 1e3;
            {
                let mut s = ctx.stats.lock();
                s.blocks += 1;
                s.header_lag.push_ms(lag);
            }
            tracing::info!(number = h.number, lag_ms = lag as u64, "new head");
            ctx.log(serde_json::json!({"ev": "header", "number": h.number, "lag_ms": lag}));
            if args.advertise_head {
                ctx.net.advertise_head(&h);
            }
            tokio::spawn(measure_block(ctx.clone(), h.clone(), Instant::now()));
            head = Some(h);
        }
    }

    tokio::time::sleep(Duration::from_secs(15)).await; // let in-flight measurements finish
    report(&ctx, true);
    Ok(())
}

async fn measure_block(ctx: Arc<Ctx>, h: Header, seen: Instant) {
    let hash = h.hash_slow();
    let slot = U256::from((h.number - 1) % HISTORY_WINDOW);
    let expected = U256::from_be_bytes(h.parent_hash.0);
    tokio::join!(measure_bal(&ctx, &h, hash, slot, expected), measure_snap(&ctx, &h, slot, expected, seen));
}

async fn measure_bal(ctx: &Ctx, h: &Header, hash: B256, slot: U256, expected: U256) {
    let Some(commit) = h.block_access_list_hash else {
        tracing::warn!(number = h.number, "header has no block_access_list_hash (pre-Amsterdam?)");
        return;
    };
    let peers: Vec<_> = ctx.peers(false).into_iter().take(ctx.fanout).collect();
    let futs = peers.iter().map(|&p| async move { (p, ctx.net.bals(p, vec![hash]).await) });
    for (peer, (res, took)) in futures::future::join_all(futs).await {
        let outcome = match res {
            Err(e) => {
                ctx.stats.lock().bal_err += 1;
                ctx.card(peer, |c| c.bal_fail += 1);
                err_kind(&e).to_string()
            }
            Ok(bals) => match bals.0.into_iter().next().flatten() {
                None => {
                    ctx.stats.lock().bal_unavailable += 1;
                    ctx.card(peer, |c| c.bal_fail += 1);
                    "unavailable".into()
                }
                Some(raw) => match skim_bal::decode_verified(&raw, commit) {
                    Err(e) => {
                        ctx.stats.lock().bal_bad += 1;
                        format!("bad: {e}")
                    }
                    Ok(bal) => {
                        let w = BlockWrites::from_bal(&bal);
                        let ok = w.get(&StateKey::Storage(HISTORY, slot)) == Some(expected);
                        ctx.card(peer, |c| c.bal.push(took));
                        let mut s = ctx.stats.lock();
                        s.bal_ok.push(took);
                        if ok { s.bal_selfcheck_ok += 1 } else { s.bal_selfcheck_fail += 1 }
                        format!("ok bytes={} accounts={} writes={} selfcheck={ok}", raw.len(), bal.len(), w.writes.len())
                    }
                },
            },
        };
        ctx.log(serde_json::json!({"ev": "bal", "number": h.number, "peer": peer.to_string(),
            "ms": took.as_secs_f64() * 1e3, "outcome": outcome}));
    }
}

/// Polls each snap/2 peer at the new root until it answers, then looks up one storage slot.
async fn measure_snap(ctx: &Ctx, h: &Header, slot: U256, expected: U256, seen: Instant) {
    let acct_hash = keccak256(HISTORY);
    let slot_hash = keccak256(slot.to_be_bytes::<32>());
    let root = h.state_root;
    let peers: Vec<_> = ctx.peers(true).into_iter().take(ctx.fanout).collect();
    let futs = peers.iter().map(|&peer| async move {
        let give_up = seen + Duration::from_secs(20);
        loop {
            let (res, took) = ctx.net.account_range(peer, root, acct_hash, 4096).await;
            match res {
                Err(e) => {
                    ctx.stats.lock().snap_err += 1;
                    ctx.log(serde_json::json!({"ev": "snap_account", "number": h.number,
                        "peer": peer.to_string(), "ms": took.as_secs_f64() * 1e3, "outcome": err_kind(&e)}));
                    return;
                }
                // Empty response without proof: this peer cannot serve the root (yet).
                Ok(m) if m.accounts.is_empty() && m.proof.is_empty() => {
                    ctx.stats.lock().snap_root_unavailable += 1;
                    if Instant::now() > give_up {
                        ctx.stats.lock().root_never_served += 1;
                        ctx.card(peer, |c| c.never_served += 1);
                        ctx.log(serde_json::json!({"ev": "root_never_served", "number": h.number, "peer": peer.to_string()}));
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Ok(m) => {
                    let present = m.accounts.first().is_some_and(|a| a.hash == acct_hash);
                    let lag = seen.elapsed();
                    ctx.card(peer, |c| c.snap_account.push(took));
                    {
                        let mut s = ctx.stats.lock();
                        s.snap_account.push(took);
                        s.root_servable_lag.push(lag);
                    }
                    let client = ctx.net.peers().into_iter().find(|(id, _)| *id == peer).map(|(_, p)| p.client).unwrap_or_default();
                    ctx.log(serde_json::json!({"ev": "snap_account", "number": h.number,
                        "peer": peer.to_string(), "client": client, "ms": took.as_secs_f64() * 1e3,
                        "root_servable_lag_ms": lag.as_secs_f64() * 1e3, "present": present,
                        "accounts": m.accounts.len(), "proof_nodes": m.proof.len()}));
                    if present {
                        measure_storage(ctx, h, peer, acct_hash, slot_hash, expected).await;
                    }
                    return;
                }
            }
        }
    });
    futures::future::join_all(futs).await;
}

async fn measure_storage(ctx: &Ctx, h: &Header, peer: Peer, acct: B256, slot_hash: B256, expected: U256) {
    let (res, took) = ctx.net.storage_range(peer, h.state_root, acct, slot_hash, 4096).await;
    let outcome = match res {
        Err(e) => {
            ctx.stats.lock().snap_err += 1;
            err_kind(&e).to_string()
        }
        Ok(m) => {
            let first = m.slots.first().and_then(|s| s.first());
            let value = first
                .filter(|d| d.hash == slot_hash)
                .and_then(|d| U256::decode(&mut d.data.as_ref()).ok());
            let ok = value == Some(expected);
            ctx.card(peer, |c| c.snap_storage.push(took));
            let mut s = ctx.stats.lock();
            s.snap_storage.push(took);
            if ok { s.snap_selfcheck_ok += 1 } else { s.snap_selfcheck_fail += 1 }
            format!("selfcheck={ok} slots={} proof_nodes={}", m.slots.first().map_or(0, |s| s.len()), m.proof.len())
        }
    };
    ctx.log(serde_json::json!({"ev": "snap_storage", "number": h.number, "peer": peer.to_string(),
        "ms": took.as_secs_f64() * 1e3, "outcome": outcome}));
}

fn report(ctx: &Ctx, final_: bool) {
    let peers = ctx.net.peers();
    let eth71 = peers.iter().filter(|(_, p)| p.eth_version >= 71).count();
    let snap2 = peers.iter().filter(|(_, p)| p.snap2).count();
    let snap1 = peers.iter().filter(|(_, p)| p.snap1).count();
    let mut clients: HashMap<String, usize> = HashMap::new();
    for (_, p) in &peers {
        *clients.entry(p.client.split('/').next().unwrap_or("?").to_string()).or_default() += 1;
    }
    let mut cards: Vec<_> = ctx
        .cards
        .lock()
        .values()
        .map(|c| serde_json::json!({"client": c.client, "enode": c.enode, "bal": c.bal.json(),
            "bal_fail": c.bal_fail, "snap_account": c.snap_account.json(),
            "snap_storage": c.snap_storage.json(), "never_served": c.never_served}))
        .collect();
    cards.sort_by_key(|c| std::cmp::Reverse(c["snap_storage"]["n"].as_u64().unwrap_or(0)));
    let s = ctx.stats.lock();
    let summary = serde_json::json!({
        "final": final_,
        "peers": {"connected": peers.len(), "eth71": eth71, "snap2": snap2, "snap1": snap1, "clients": clients},
        "blocks": s.blocks,
        "reorgs_or_gaps": s.reorgs_or_gaps,
        "per_peer": cards,
        "el_header_poll_lag": s.header_lag.json(),
        "header_request": s.header_req.json(),
        "bal": {"ok": s.bal_ok.json(), "unavailable": s.bal_unavailable, "bad": s.bal_bad,
                "errors": s.bal_err, "selfcheck_ok": s.bal_selfcheck_ok, "selfcheck_fail": s.bal_selfcheck_fail},
        "snap": {"account": s.snap_account.json(), "storage": s.snap_storage.json(),
                 "root_servable_lag": s.root_servable_lag.json(),
                 "root_unavailable_responses": s.snap_root_unavailable,
                 "root_never_served": s.root_never_served, "errors": s.snap_err,
                 "selfcheck_ok": s.snap_selfcheck_ok, "selfcheck_fail": s.snap_selfcheck_fail},
    });
    tracing::info!(
        "peers={} eth71={eth71} snap2={snap2} snap1={snap1} blocks={} | header_lag {} | bal {} unavail={} bad={} | snap acct {} storage {} servable_lag {} never={}",
        peers.len(), s.blocks, s.header_lag.summary(), s.bal_ok.summary(), s.bal_unavailable, s.bal_bad,
        s.snap_account.summary(), s.snap_storage.summary(), s.root_servable_lag.summary(), s.root_never_served
    );
    drop(s);
    ctx.log(serde_json::json!({"ev": "summary", "data": summary}));
    if final_ {
        println!("{}", serde_json::to_string_pretty(&summary).unwrap());
    }
}
