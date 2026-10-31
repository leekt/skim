//! P3 benchmark driver: the same per-block workload `skim follow` runs (7 fixed calls + the same
//! seeded random V3 quotes), sent over JSON-RPC to one or more endpoints (skim and/or a full
//! node), fixed to the **same block hash**.
//!
//! Per block, per endpoint:
//! - **readiness:** time from the block's slot start until the endpoint first answers `eth_call` at
//!   that block (polled every 50 ms from slot start, independently of the workload)
//! - **latency:** each call's round trip, plus a hash of its result for cross-endpoint equality
//! - **throughput** (every `--throughput-every` blocks): `--burst` calls at concurrency 1, 10, 100
//!
//! Resource sampling (RSS/footprint/CPU/disk/network) runs externally (`data/sample-procs.sh`).

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use alloy_primitives::keccak256;
use futures::{StreamExt, stream};

use crate::{
    probe_call::{Call, calls, random_quotes},
    stats::Samples,
};

#[derive(clap::Args)]
pub struct Args {
    /// Endpoints as name=url (repeatable), e.g. skim=http://127.0.0.1:8546
    #[arg(long = "endpoint", required = true)]
    endpoints: Vec<String>,
    /// Endpoint whose head drives the run (default: the first).
    #[arg(long)]
    driver: Option<String>,
    #[arg(long, default_value_t = 300)]
    blocks: u64,
    #[arg(long, default_value_t = 8)]
    random_quotes: usize,
    /// Run the concurrency sweep every N blocks (0 = never).
    #[arg(long, default_value_t = 50)]
    throughput_every: u64,
    /// Calls per concurrency level in a sweep.
    #[arg(long, default_value_t = 300)]
    burst: usize,
    #[arg(long, default_value = "data/bench-node.jsonl")]
    out: String,
    /// Concurrency levels of the throughput sweep.
    #[arg(long, value_delimiter = ',', default_value = "1,10,100")]
    sweep_levels: Vec<usize>,
}

const GENESIS: u64 = 1655733600;

fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64() * 1e3
}

async fn rpc(client: &reqwest::Client, url: &str, method: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    Ok(client
        .post(url)
        .json(&serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
        .send()
        .await?
        .json()
        .await?)
}

/// Hash of a call's outcome: result bytes, or (error code, revert data).
fn outcome_hash(v: &serde_json::Value) -> Option<String> {
    if let Some(r) = v.get("result") {
        return Some(format!("{}", keccak256(r.to_string())));
    }
    v.get("error").filter(|e| e["code"] == 3).map(|e| format!("revert:{}", keccak256(e["data"].to_string())))
}

async fn call_at(client: &reqwest::Client, url: &str, c: &Call, hash: &str) -> (f64, Option<String>) {
    let t = Instant::now();
    let r = rpc(client, url, "eth_call", serde_json::json!([{"to": c.to, "data": c.data}, {"blockHash": hash}])).await;
    (t.elapsed().as_secs_f64() * 1e3, r.ok().as_ref().and_then(outcome_hash))
}

#[derive(Default)]
struct Ep {
    latency: Samples,
    readiness: Samples,
    ok: u64,
    err: u64,
    throughput: HashMap<usize, Vec<f64>>,
    /// Blocks this endpoint didn't have within 12 s of the driver.
    unavailable_blocks: u64,
}

pub async fn run(args: Args) -> anyhow::Result<()> {
    use std::io::Write;
    // A request that never returns would stall the whole run (seen: 40 min at block 109).
    let client = reqwest::Client::builder().pool_max_idle_per_host(256).timeout(Duration::from_secs(30)).build()?;
    let eps: Vec<(String, String)> = args
        .endpoints
        .iter()
        .map(|s| s.split_once('=').map(|(a, b)| (a.to_string(), b.to_string())).ok_or_else(|| anyhow::anyhow!("bad endpoint {s}")))
        .collect::<Result<_, _>>()?;
    let driver = args.driver.clone().unwrap_or_else(|| eps[0].0.clone());
    let driver_url = eps.iter().find(|(n, _)| *n == driver).map(|(_, u)| u.clone()).ok_or_else(|| anyhow::anyhow!("no driver"))?;
    let mut out = std::io::BufWriter::new(std::fs::File::create(&args.out)?);
    let mut stats: HashMap<String, Ep> = eps.iter().map(|(n, _)| (n.clone(), Ep::default())).collect();
    let (mut agree, mut disagree) = (0u64, 0u64);
    let mut last = 0u64;
    let mut done = 0u64;

    while done < args.blocks {
        let head = rpc(&client, &driver_url, "eth_getBlockByNumber", serde_json::json!(["latest", false])).await;
        let Ok(head) = head else {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        };
        let n = u64::from_str_radix(head["result"]["number"].as_str().unwrap_or("0x0").trim_start_matches("0x"), 16).unwrap_or(0);
        if n <= last {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        last = n;
        done += 1;
        let hash = head["result"]["hash"].as_str().unwrap_or_default().to_string();
        let ts = u64::from_str_radix(head["result"]["timestamp"].as_str().unwrap_or("0x0").trim_start_matches("0x"), 16).unwrap_or(0);

        // Readiness for the *next* block on every endpoint: poll by number from its slot start.
        for (name, url) in &eps {
            let (client, url, name, next) = (client.clone(), url.clone(), name.clone(), n + 1);
            let slot_start = (ts + 12) as f64 * 1e3;
            let out_path = format!("{}.ready", args.out);
            tokio::spawn(async move {
                let wait = slot_start - now_ms();
                if wait > 0.0 {
                    tokio::time::sleep(Duration::from_secs_f64(wait / 1e3)).await;
                }
                let give_up = Instant::now() + Duration::from_secs(60);
                while Instant::now() < give_up {
                    let r = rpc(&client, &url, "eth_getBlockByNumber", serde_json::json!([format!("{next:#x}"), false])).await;
                    if let Ok(v) = r
                        && let Some(t) = v["result"]["timestamp"].as_str() {
                            let bts = u64::from_str_radix(t.trim_start_matches("0x"), 16).unwrap_or(0);
                            let ready = now_ms() - bts as f64 * 1e3;
                            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&out_path) {
                                let _ = writeln!(f, "{}", serde_json::json!({"endpoint": name, "number": next, "ready_ms": ready}));
                            }
                            return;
                        }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            });
        }

        // Workload at this block hash on every endpoint.
        let mut all = calls();
        all.extend(random_quotes(n, args.random_quotes));
        let mut per_call: Vec<HashMap<String, Option<String>>> = vec![HashMap::new(); all.len()];
        // Each endpoint is queried only once it has this block (an endpoint that sees the block
        // later answers "unavailable", which is a refusal, not a disagreement). The wait is
        // recorded; an endpoint that doesn't have the block within 12 s is skipped for it.
        let mut avail_ms: HashMap<String, Option<f64>> = HashMap::new();
        for (name, url) in &eps {
            let t = Instant::now();
            let mut got = None;
            while t.elapsed() < Duration::from_secs(12) {
                if let Ok(v) = rpc(&client, url, "eth_getBlockByHash", serde_json::json!([hash, false])).await
                    && v["result"].is_object()
                {
                    got = Some(t.elapsed().as_secs_f64() * 1e3);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            avail_ms.insert(name.clone(), got);
        }
        for (name, url) in &eps {
            let st = stats.get_mut(name).unwrap();
            if avail_ms[name].is_none() {
                st.unavailable_blocks += 1;
                continue;
            }
            for (i, c) in all.iter().enumerate() {
                let (ms, h) = call_at(&client, url, c, &hash).await;
                st.latency.push_ms(ms);
                if h.is_some() { st.ok += 1 } else { st.err += 1 }
                per_call[i].insert(name.clone(), h);
            }
        }
        if eps.len() > 1 && avail_ms.values().all(Option::is_some) {
            for m in &per_call {
                let vals: Vec<_> = m.values().collect();
                if vals.iter().all(|v| v.is_some() && *v == vals[0]) { agree += 1 } else { disagree += 1 }
            }
        }
        let _ = writeln!(out, "{}", serde_json::json!({"number": n, "hash": hash, "slot": (ts - GENESIS) / 12, "avail_ms": avail_ms, "results": per_call}));
        let _ = out.flush();

        // Concurrency sweep.
        if args.throughput_every > 0 && done.is_multiple_of(args.throughput_every) {
            let all = Arc::new(all);
            for (name, url) in &eps {
                for &conc in &args.sweep_levels {
                    let t = Instant::now();
                    let n_ok = stream::iter(0..args.burst)
                        .map(|i| {
                            let (client, url, all, hash) = (client.clone(), url.clone(), all.clone(), hash.clone());
                            async move { call_at(&client, &url, &all[i % all.len()], &hash).await.1.is_some() }
                        })
                        .buffer_unordered(conc)
                        .filter(|ok| futures::future::ready(*ok))
                        .count()
                        .await;
                    let rps = n_ok as f64 / t.elapsed().as_secs_f64();
                    stats.get_mut(name).unwrap().throughput.entry(conc).or_default().push(rps);
                    tracing::info!(endpoint = %name, conc, rps = rps as u64, ok = n_ok, "throughput");
                }
            }
        }
        if done.is_multiple_of(25) {
            for (name, st) in &stats {
                tracing::info!(endpoint = %name, done, ok = st.ok, err = st.err, "eth_call {}", st.latency.summary());
            }
        }
    }

    // Readiness summary from the side file.
    if let Ok(s) = std::fs::read_to_string(format!("{}.ready", args.out)) {
        for l in s.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(l)
                && let (Some(n), Some(ms)) = (v["endpoint"].as_str(), v["ready_ms"].as_f64())
                    && let Some(st) = stats.get_mut(n) {
                        st.readiness.push_ms(ms);
                    }
        }
    }
    let summary: HashMap<_, _> = stats
        .iter()
        .map(|(n, s)| {
            let p99 = s.latency.pct(99.0);
            (n.clone(), serde_json::json!({
                "eth_call": s.latency.json(), "eth_call_p99_ms": p99, "ok": s.ok, "err": s.err, "unavailable_blocks": s.unavailable_blocks,
                "readiness_after_block_timestamp": s.readiness.json(),
                "throughput_rps_median": s.throughput.iter().map(|(c, v)| {
                    let mut v = v.clone();
                    v.sort_by(|a, b| a.total_cmp(b));
                    (c.to_string(), v.get(v.len() / 2).copied())
                }).collect::<HashMap<_, _>>(),
            }))
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&serde_json::json!({"blocks": done, "endpoints": summary,
        "cross_endpoint": {"agree": agree, "disagree": disagree}}))?);
    Ok(())
}
