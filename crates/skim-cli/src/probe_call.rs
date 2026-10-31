//! M0 Spike C (PROJECT.md §1, §7.2, §10): cold-call economics vs a hosted RPC, with the
//! adaptive SNAP scheduler (`skim-snap`) doing every fetch.
//!
//! For each call at a fresh Sepolia head H:
//! - **hinted:** fresh state cache, persistent bytecode cache, and the access set learned for
//!   the call *shape* (target + selector) from earlier blocks and *other inputs*. Round 1 fetches
//!   it as one wave. This tests whether learned sets generalize across inputs.
//! - **cold:** empty cache, rounds of recording revm pass -> one scheduler wave of the misses.
//! - **warm:** local re-execution.
//! - **Infura:** `eth_call` at `{blockHash: H}`, first call (new TLS connection) and warm repeats.
//! - **correctness:** every skim output must byte-match Infura's.

use std::{
    collections::{HashMap, HashSet},
    sync::Mutex as StdMutex,
    time::{Duration, Instant},
};

use alloy_consensus::Header;
use alloy_primitives::{address, Address, Bytes, B256, U256};
use revm::{
    bytecode::Bytecode,
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::block::BlobExcessGasAndPrice,
    database::WrapDatabaseRef,
    primitives::{hardfork::SpecId, KECCAK_EMPTY},
    state::AccountInfo,
    Context, DatabaseRef, ExecuteEvm, MainBuilder, MainContext,
};
use skim_el::ElNet;
use skim_snap::{el::ElTransport, Key, Scheduler, Value};

use crate::stats::Samples;
use skim_sim::learned::{AccessKey, Learned, Source};

#[derive(clap::Args)]
pub struct Args {
    /// Measurement iterations (each at a fresh head).
    #[arg(long, default_value_t = 6)]
    iterations: usize,
    /// Warm Infura repeats per call per iteration.
    #[arg(long, default_value_t = 2)]
    infura_repeats: usize,
    #[arg(long, default_value_t = 30398)]
    port: u16,
    /// Minimum snap/2 peers before measuring.
    #[arg(long, default_value_t = 1)]
    min_snap_peers: usize,
    /// Scheduler: max in-flight requests per peer.
    #[arg(long, default_value_t = 8)]
    per_peer_inflight: usize,
    /// Scheduler: hedge a request to another peer after this many ms.
    #[arg(long, default_value_t = 1200)]
    hedge_ms: u64,
    /// Trusted peers (enode URLs), e.g. a local node as the control case. Repeatable.
    #[arg(long = "trusted")]
    trusted: Vec<String>,
    /// Use trusted peers only.
    #[arg(long)]
    trusted_only: bool,
    /// Scheduler: use only the N fastest peers (peer-count experiment).
    #[arg(long)]
    max_peers: Option<usize>,
    /// Skip cold runs after the first iteration (soak: steady-state hinted calls only).
    #[arg(long)]
    soak: bool,
    /// Learned sets: keep keys seen in at least this fraction of recent runs.
    #[arg(long, default_value_t = 0.5)]
    min_freq: f64,
    /// Learned sets: cap on core keys in the first wave.
    #[arg(long, default_value_t = 256)]
    core_cap: usize,
    /// Peer-dependency experiment: after this many iterations, stop using the peer that has
    /// served the most requests so far.
    #[arg(long)]
    drop_top_after: Option<usize>,
    /// Per-run JSONL (hinted/cold runs, peer snapshots).
    #[arg(long)]
    out: Option<String>,
}

fn to_misses(keys: &[AccessKey]) -> Misses {
    let mut m = Misses::default();
    for k in keys {
        match *k {
            AccessKey::Account(a) => {
                m.accounts.insert(a);
            }
            AccessKey::Storage(a, s) => {
                m.storage.insert((a, s));
            }
        }
    }
    m
}

fn to_keys(m: &Misses) -> Vec<AccessKey> {
    m.accounts.iter().map(|a| AccessKey::Account(*a)).chain(m.storage.iter().map(|(a, s)| AccessKey::Storage(*a, *s))).collect()
}

pub(crate) const WETH_ADDR: Address = WETH;
pub(crate) const USDC_ADDR: Address = USDC;

/// `n` new-input V3 single-hop quotes for block `seed`: random direction, fee tier (0.05%,
/// 0.3% or 1%) and log-uniform amount (0.0001–100 WETH, or 0.001–100,000 USDC).
pub(crate) fn random_quotes(seed: u64, n: usize) -> Vec<Call> {
    let mut x = seed.wrapping_mul(0x9E3779B97F4A7C15) | 1;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let pad = |h: &str| format!("{h:0>64}");
    let (w, u) = ("fff9976782d46cc05630d1f6ebab18b2324d6b14", "1c7d4b196cb0c7b01d743fbc6116a902379c7238");
    (0..n)
        .map(|_| {
            let r = next();
            let weth_in = r & 1 == 0;
            let fee: u32 = [500, 3000, 10000][((r >> 1) % 3) as usize];
            let frac = (next() % 1_000_000) as f64 / 1_000_000.0;
            let (tin, tout, amount) = if weth_in {
                (w, u, 10f64.powf(14.0 + 6.0 * frac)) // 1e14..1e20 wei
            } else {
                (u, w, 10f64.powf(3.0 + 8.0 * frac)) // 1e3..1e11 (6 decimals)
            };
            let amt = amount as u128;
            let data = format!("c6a5026a{}{}{}{}{}", pad(tin), pad(tout), pad(&format!("{amt:x}")), pad(&format!("{fee:x}")), pad("0"));
            Call {
                name: format!("rand {} {amt} fee{fee}", if weth_in { "WETH->USDC" } else { "USDC->WETH" }),
                to: QUOTER,
                data: Bytes::from(alloy_primitives::hex::decode(data).unwrap()),
            }
        })
        .collect()
}

const QUOTER: Address = address!("Ed1f6473345F45b75F8179591dd5bA1888cf2FB3");
const WETH: Address = address!("fFf9976782d46CC05630D1f6eBAb18b2324d6B14");
const USDC: Address = address!("1c7D4B196Cb0C7B01d743Fbc6116a902379C7238");

pub(crate) struct Call {
    pub(crate) name: String,
    pub(crate) to: Address,
    pub(crate) data: Bytes,
}

pub(crate) fn calls() -> Vec<Call> {
    let pad = |h: &str| format!("{h:0>64}");
    let w = "fff9976782d46cc05630d1f6ebab18b2324d6b14";
    let u = "1c7d4b196cb0c7b01d743fbc6116a902379c7238";
    let holder = pad("3289680dd4d6c10bb19b899729cda5eef58aeff1");
    let hx = |s: &str| Bytes::from(alloy_primitives::hex::decode(s).unwrap());
    let amt = |a: u128| pad(&format!("{a:x}"));
    let single = |tin: &str, tout: &str, a: u128| {
        hx(&format!("c6a5026a{}{}{}{}{}", pad(tin), pad(tout), amt(a), pad("bb8"), pad("0")))
    };
    let path = format!("{w}000bb8{u}0001f4{w}");
    let multi = format!("cdca1753{}{}{}{:0<192}", pad("40"), amt(10u128.pow(16)), pad(&format!("{:x}", path.len() / 2)), path);
    let c = |name: &str, to, data| Call { name: name.into(), to, data };
    vec![
        c("erc20_balanceOf (WETH)", WETH, hx(&format!("70a08231{holder}"))),
        c("proxied_balanceOf (USDC)", USDC, hx(&format!("70a08231{holder}"))),
        c("v3_single 0.01 WETH->USDC", QUOTER, single(w, u, 10u128.pow(16))),
        c("v3_single 1 WETH->USDC", QUOTER, single(w, u, 10u128.pow(18))),
        c("v3_single 100 WETH->USDC", QUOTER, single(w, u, 100 * 10u128.pow(18))),
        c("v3_single 10 USDC->WETH", QUOTER, single(u, w, 10 * 10u128.pow(6))),
        c("v3_multihop 0.01 WETH", QUOTER, hx(&multi)),
    ]
}

// ---------- sparse state + recording database ----------

#[derive(Default)]
pub(crate) struct Sparse {
    pub(crate) accounts: HashMap<Address, Option<AccountInfo>>,
    pub(crate) storage: HashMap<(Address, U256), U256>,
    pub(crate) code: HashMap<B256, Bytecode>,
}

#[derive(Default)]
pub(crate) struct Misses {
    pub(crate) accounts: HashSet<Address>,
    pub(crate) storage: HashSet<(Address, U256)>,
    pub(crate) code: HashSet<B256>,
}

impl Misses {
    fn len(&self) -> usize {
        self.accounts.len() + self.storage.len() + self.code.len()
    }
}

/// Unknown keys read as placeholders (absent account, zero slot, empty code) and are recorded.
/// Results of a pass with misses are never reported (§7.2 discovery pass).
struct Recorder<'a> {
    st: &'a Sparse,
    misses: StdMutex<Misses>,
    /// Every key read, hit or miss: the call's access set.
    touched: StdMutex<Misses>,
}

impl DatabaseRef for Recorder<'_> {
    type Error = std::convert::Infallible;

    fn basic_ref(&self, a: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.touched.lock().unwrap().accounts.insert(a);
        match self.st.accounts.get(&a) {
            Some(info) => Ok(info.clone()),
            None => {
                self.misses.lock().unwrap().accounts.insert(a);
                Ok(None)
            }
        }
    }

    fn code_by_hash_ref(&self, h: B256) -> Result<Bytecode, Self::Error> {
        match self.st.code.get(&h) {
            Some(c) => Ok(c.clone()),
            None => {
                self.misses.lock().unwrap().code.insert(h);
                Ok(Bytecode::default())
            }
        }
    }

    fn storage_ref(&self, a: Address, slot: U256) -> Result<U256, Self::Error> {
        self.touched.lock().unwrap().storage.insert((a, slot));
        match self.st.storage.get(&(a, slot)) {
            Some(v) => Ok(*v),
            None => {
                self.misses.lock().unwrap().storage.insert((a, slot));
                Ok(U256::ZERO)
            }
        }
    }

    fn block_hash_ref(&self, _n: u64) -> Result<B256, Self::Error> {
        Ok(B256::ZERO)
    }
}

pub(crate) fn block_env(h: &Header) -> BlockEnv {
    BlockEnv {
        number: U256::from(h.number),
        beneficiary: h.beneficiary,
        timestamp: U256::from(h.timestamp),
        gas_limit: h.gas_limit,
        basefee: h.base_fee_per_gas.unwrap_or_default(),
        difficulty: U256::ZERO,
        prevrandao: Some(h.mix_hash),
        blob_excess_gas_and_price: Some(BlobExcessGasAndPrice::new_with_spec(
            h.excess_blob_gas.unwrap_or_default(),
            SpecId::AMSTERDAM,
        )),
        ..Default::default()
    }
}

/// eth_call semantics; returns (output, missed keys, access set).
fn execute(st: &Sparse, h: &Header, to: Address, data: &Bytes) -> (Option<Bytes>, Misses, Misses) {
    let rec = Recorder { st, misses: Default::default(), touched: Default::default() };
    let mut cfg = CfgEnv::new_with_spec(SpecId::AMSTERDAM);
    cfg.chain_id = 11155111;
    cfg.disable_nonce_check = true;
    cfg.disable_balance_check = true;
    cfg.disable_base_fee = true;
    cfg.disable_eip3607 = true;
    cfg.disable_block_gas_limit = true;
    cfg.disable_fee_charge = true; // no fees => the per-block coinbase is never loaded
    let out = {
        let mut evm = Context::mainnet()
            .with_db(WrapDatabaseRef(&rec))
            .with_block(block_env(h))
            .with_cfg(cfg)
            .build_mainnet();
        let tx = TxEnv::builder()
            .caller(Address::ZERO)
            .kind(revm::primitives::TxKind::Call(to))
            .data(data.clone())
            .gas_limit(30_000_000)
            .chain_id(Some(11155111))
            .build()
            .unwrap();
        evm.transact(tx).ok().map(|r| r.result.into_output().unwrap_or_default())
    };
    (out, rec.misses.into_inner().unwrap(), rec.touched.into_inner().unwrap())
}

// ---------- fetch waves through the scheduler ----------

#[derive(Default, Clone, Copy)]
pub(crate) struct RoundCost {
    keys: usize,
    rtts: usize,
}

/// One wave: every miss in parallel through the scheduler, then the code of newly seen accounts
/// (a second, dependent RTT unless the bytecode cache already has it).
async fn resolve_round(
    sched: &Scheduler<ElTransport>,
    root: B256,
    st: &mut Sparse,
    m: Misses,
) -> anyhow::Result<RoundCost> {
    let mut keys: Vec<Key> = m.accounts.iter().map(|&address| Key::Account { root, address }).collect();
    keys.extend(m.storage.iter().map(|&(address, slot)| Key::Storage { root, address, slot }));
    keys.extend(m.code.iter().map(|&h| Key::Code(h)));
    let n = keys.len();
    let mut rtts = 1;
    let mut chained = Vec::new();
    for (k, r) in keys.iter().zip(sched.fetch(&keys).await) {
        let v = r.map_err(|e| anyhow::anyhow!("{k:?}: {e}"))?;
        match (k, v) {
            (Key::Account { address, .. }, Value::Account(a)) => {
                let info = a.map(|a| AccountInfo {
                    balance: a.balance,
                    nonce: a.nonce,
                    code_hash: a.code_hash,
                    code: None,
                    ..Default::default()
                });
                if let Some(i) = &info
                    && i.code_hash != KECCAK_EMPTY && !st.code.contains_key(&i.code_hash) {
                        chained.push(Key::Code(i.code_hash));
                    }
                st.accounts.insert(*address, info);
            }
            (Key::Storage { address, slot, .. }, Value::Storage(v)) => {
                st.storage.insert((*address, *slot), v);
            }
            (Key::Code(h), Value::Code(c)) => {
                st.code.insert(*h, Bytecode::new_raw(c));
            }
            (k, v) => anyhow::bail!("mismatched answer {v:?} for {k:?}"),
        }
    }
    if !chained.is_empty() {
        rtts += 1;
        for (k, r) in chained.iter().zip(sched.fetch(&chained).await) {
            if let (Key::Code(h), Ok(Value::Code(c))) = (k, r) {
                st.code.insert(*h, Bytecode::new_raw(c));
            }
        }
    }
    Ok(RoundCost { keys: n, rtts })
}

pub(crate) struct ColdRun {
    pub(crate) output: Option<Bytes>,
    pub(crate) rounds: Vec<RoundCost>,
    wall: Duration,
    pub(crate) access: Misses,
}

/// `hints`: a learned access set for this call shape, fetched as round 1.
pub(crate) async fn cold_run(
    sched: &Scheduler<ElTransport>,
    h: &Header,
    call: &Call,
    st: &mut Sparse,
    hints: Option<&Misses>,
) -> anyhow::Result<ColdRun> {
    let start = Instant::now();
    let mut rounds = Vec::new();
    if let Some(hint) = hints {
        let m = Misses { accounts: hint.accounts.clone(), storage: hint.storage.clone(), code: HashSet::new() };
        rounds.push(resolve_round(sched, h.state_root, st, m).await?);
    }
    loop {
        let (out, misses, access) = execute(st, h, call.to, &call.data);
        if misses.len() == 0 {
            return Ok(ColdRun { output: out, rounds, wall: start.elapsed(), access });
        }
        if rounds.len() >= 16 {
            anyhow::bail!("did not converge after 16 rounds");
        }
        rounds.push(resolve_round(sched, h.state_root, st, misses).await?);
    }
}

// ---------- Infura baseline ----------

async fn infura_call(client: &reqwest::Client, url: &str, hash: B256, to: Address, data: &Bytes) -> anyhow::Result<(Bytes, Duration)> {
    let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "eth_call",
        "params": [{"to": to, "data": data}, {"blockHash": hash}]});
    let start = Instant::now();
    let v: serde_json::Value = client.post(url).json(&body).send().await?.json().await?;
    let took = start.elapsed();
    let r = v["result"].as_str().ok_or_else(|| anyhow::anyhow!("infura error: {}", v["error"]))?;
    Ok((alloy_primitives::hex::decode(r)?.into(), took))
}

pub(crate) fn infura_url() -> anyhow::Result<String> {
    if let Ok(u) = std::env::var("INFURA_SEPOLIA_URL") {
        return Ok(u);
    }
    let env = std::fs::read_to_string(".env").unwrap_or_default();
    env.lines()
        .find_map(|l| l.strip_prefix("INFURA_SEPOLIA_URL=").map(|s| s.trim().to_string()))
        .ok_or_else(|| anyhow::anyhow!("set INFURA_SEPOLIA_URL (env or .env)"))
}

// ---------- driver ----------

#[derive(Default)]
struct PerCall {
    cold_wall: Samples,
    rounds: Vec<usize>,
    rtts: Vec<usize>,
    keys: Vec<usize>,
    hinted_wall: Samples,
    hinted_rounds: Vec<usize>,
    hinted_keys: Vec<usize>,
    hinted_exact_k: Vec<usize>,
    hinted_core_k: Vec<usize>,
    warm_exec: Samples,
    infura_first: Samples,
    infura_warm: Samples,
    match_ok: u32,
    match_fail: u32,
    errors: u32,
}

fn median(v: &[usize]) -> Option<usize> {
    let mut v = v.to_vec();
    v.sort();
    v.get(v.len() / 2).copied()
}

fn check(r: &mut PerCall, name: &str, ours: &Option<Bytes>, infura: &Bytes) {
    if ours.as_ref() == Some(infura) {
        r.match_ok += 1;
    } else {
        r.match_fail += 1;
        tracing::warn!(call = name, skim = ?ours, infura = %infura, "OUTPUT MISMATCH");
    }
}

pub async fn run(args: Args) -> anyhow::Result<()> {
    let url = infura_url()?;
    let trusted = args.trusted.iter().map(|s| s.parse()).collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("bad --trusted enode: {e:?}"))?;
    let net = ElNet::start_sepolia(args.port, trusted, args.trusted_only)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let sched = Scheduler::new(
        ElTransport::new(net.clone()),
        skim_snap::Config {
            per_peer_inflight: args.per_peer_inflight,
            max_peers: args.max_peers,
            hedge_after: Duration::from_millis(args.hedge_ms),
            ..Default::default()
        },
    );
    let client = reqwest::Client::new();
    let calls = calls();
    let mut res: Vec<PerCall> = calls.iter().map(|_| PerCall::default()).collect();
    // Learned access sets per call shape (union over inputs and blocks), and the bytecode cache.
    let mut learned = Learned::new(8, args.min_freq, args.core_cap);
    let mut out = args.out.as_ref().map(|p| std::io::BufWriter::new(std::fs::File::create(p).unwrap()));
    let mut log = |v: serde_json::Value| {
        if let Some(o) = out.as_mut() {
            use std::io::Write;
            let _ = serde_json::to_writer(&mut *o, &v);
            let _ = o.write_all(b"\n");
            let _ = o.flush();
        }
    };
    let mut dropped: Option<String> = None;
    let mut code_cache: HashMap<B256, Bytecode> = HashMap::new();

    let mut last: Option<u64> = None;
    for it in 0..args.iterations {
        while net.peers().iter().filter(|(_, p)| p.snap2 || p.snap1).count() < args.min_snap_peers {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let h = loop {
            if let Some(h) = net.head().filter(|h| last.is_none_or(|n| h.number > n)) {
                break h;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        last = Some(h.number);
        let hash = h.hash_slow();
        let snap_peers = net.peers().iter().filter(|(_, p)| p.snap2 || p.snap1).count();
        tracing::info!(iteration = it, number = h.number, snap_peers, "measuring");
        if args.drop_top_after == Some(it)
            && let Some((top, ps)) = sched.peer_stats().into_iter().max_by_key(|(_, p)| p.ok) {
                sched.transport().exclude(top);
                tracing::warn!(peer = %top, served = ps.ok, ewma_ms = ps.ewma_ms, "EXPERIMENT: excluded the top-serving peer");
                dropped = Some(format!("{top}"));
            }
        let window = if dropped.is_some() { "after" } else { "before" };
        log(serde_json::json!({"ev": "peers", "it": it, "window": window, "peers": sched.peer_stats().into_iter()
            .map(|(id, p)| serde_json::json!({"peer": format!("{id:.10}"), "ok": p.ok, "ewma_ms": p.ewma_ms, "limit": p.limit}))
            .collect::<Vec<_>>()}));

        for (i, call) in calls.iter().enumerate() {
            let r = &mut res[i];
            let infura = match infura_call(&reqwest::Client::new(), &url, hash, call.to, &call.data).await {
                Ok((out, d)) => {
                    r.infura_first.push(d);
                    Some(out)
                }
                Err(e) => {
                    tracing::warn!(call = %call.name, "infura: {e}");
                    None
                }
            };
            for _ in 0..args.infura_repeats {
                if let Ok((_, d)) = infura_call(&client, &url, hash, call.to, &call.data).await {
                    r.infura_warm.push(d);
                }
            }

            if let Some((source, keys)) = learned.predict(call.to, &call.data) {
                let hint = to_misses(&keys);
                let mut st = Sparse { code: code_cache.clone(), ..Default::default() };
                match cold_run(&sched, &h, call, &mut st, Some(&hint)).await {
                    Ok(c) => {
                        r.hinted_wall.push(c.wall);
                        r.hinted_rounds.push(c.rounds.len());
                        r.hinted_keys.push(c.rounds.iter().map(|x| x.keys).sum());
                        match source {
                            Source::Exact => r.hinted_exact_k.push(c.rounds.len()),
                            Source::Core | Source::Pattern => r.hinted_core_k.push(c.rounds.len()),
                        }
                        if let Some(inf) = &infura {
                            check(r, &call.name, &c.output, inf);
                        }
                        tracing::info!(call = %call.name, ?source, predicted = keys.len(), rounds = c.rounds.len(),
                            ms = c.wall.as_millis() as u64, "hinted");
                        log(serde_json::json!({"ev": "hinted", "it": it, "window": window, "call": call.name,
                            "ms": c.wall.as_secs_f64() * 1e3, "rounds": c.rounds.len(), "ok": true}));
                        code_cache.extend(st.code.iter().map(|(k, v)| (*k, v.clone())));
                        learned.record(call.to, &call.data, to_keys(&c.access));
                    }
                    Err(e) => {
                        r.errors += 1;
                        tracing::warn!(call = %call.name, "hinted run failed: {e}");
                        log(serde_json::json!({"ev": "hinted", "it": it, "window": window, "call": call.name, "ok": false}));
                    }
                }
                if args.soak && it > 0 {
                    continue;
                }
            }

            let mut st = Sparse::default();
            match cold_run(&sched, &h, call, &mut st, None).await {
                Ok(c) => {
                    r.cold_wall.push(c.wall);
                    r.rounds.push(c.rounds.len());
                    r.rtts.push(c.rounds.iter().map(|x| x.rtts).sum());
                    r.keys.push(c.rounds.iter().map(|x| x.keys).sum());
                    if let Some(inf) = &infura {
                        check(r, &call.name, &c.output, inf);
                    }
                    let t = Instant::now();
                    for _ in 0..20 {
                        let _ = std::hint::black_box(execute(&st, &h, call.to, &call.data));
                    }
                    r.warm_exec.push(t.elapsed() / 20);
                    code_cache.extend(st.code.iter().map(|(k, v)| (*k, v.clone())));
                    learned.record(call.to, &call.data, to_keys(&c.access));
                    tracing::info!(call = %call.name, rounds = c.rounds.len(),
                        rtts = c.rounds.iter().map(|x| x.rtts).sum::<usize>(),
                        keys = c.rounds.iter().map(|x| x.keys).sum::<usize>(),
                        ms = c.wall.as_millis() as u64, "cold");
                }
                Err(e) => {
                    r.errors += 1;
                    tracing::warn!(call = %call.name, "cold run failed: {e}");
                }
            }
        }
    }

    let out: Vec<_> = calls
        .iter()
        .zip(&res)
        .map(|(c, r)| {
            serde_json::json!({
                "call": c.name,
                "cold": {"wall": r.cold_wall.json(), "k_median": median(&r.rounds), "rtts_median": median(&r.rtts), "keys_median": median(&r.keys)},
                "hinted": {"wall": r.hinted_wall.json(), "k_median": median(&r.hinted_rounds), "keys_median": median(&r.hinted_keys),
                    "k_exact": r.hinted_exact_k, "k_core": r.hinted_core_k},
                "warm_exec_us": r.warm_exec.pct(50.0).map(|ms| ms * 1e3),
                "infura_first": r.infura_first.json(),
                "infura_warm": r.infura_warm.json(),
                "output_match": r.match_ok, "output_mismatch": r.match_fail, "errors": r.errors,
            })
        })
        .collect();
    let summary = serde_json::json!({
        "calls": out,
        "scheduler": sched.stats(),
        "peers": sched.peer_stats().into_iter().map(|(id, p)| (format!("{id:.8}"), p)).collect::<HashMap<_, _>>(),
    });
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

// ---------- peer-count sweep (probe-wave) ----------

#[derive(clap::Args)]
pub struct WaveArgs {
    #[arg(long, default_value_t = 5)]
    iterations: usize,
    /// Peer counts to compare (0 = all available).
    #[arg(long, value_delimiter = ',', default_value = "1,2,4,0")]
    sweep: Vec<usize>,
    #[arg(long, default_value_t = 4)]
    min_snap_peers: usize,
    #[arg(long, default_value_t = 8)]
    per_peer_inflight: usize,
    #[arg(long, default_value_t = 30393)]
    port: u16,
    /// Concurrency modes to compare for each peer count: fixed cap and/or AIMD.
    #[arg(long, value_delimiter = ',', default_value = "fixed,aimd")]
    modes: Vec<String>,
}

/// Learns the access set of a large call once, then at each new head fetches that same wave
/// with schedulers limited to N peers, all over the same live peer pool.
pub async fn run_wave(args: WaveArgs) -> anyhow::Result<()> {
    let net = ElNet::start_sepolia(args.port, Vec::new(), false).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let snap_peers = |net: &ElNet| net.peers().iter().filter(|(_, p)| p.snap2 || p.snap1).count();
    while snap_peers(&net) < args.min_snap_peers || net.head().is_none() {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let call = calls().into_iter().find(|c| c.name.starts_with("v3_single 100")).unwrap();
    let full = Scheduler::new(ElTransport::new(net.clone()), skim_snap::Config::default());
    let h = net.head().unwrap();
    let mut st = Sparse::default();
    let learned = cold_run(&full, &h, &call, &mut st, None).await?.access;
    let n_keys = learned.accounts.len() + learned.storage.len();
    tracing::info!(keys = n_keys, "wave key set learned");

    let mut results: HashMap<(usize, String), (Samples, u64, u64)> = HashMap::new(); // wall, ok, failed
    let mut last = h.number;
    for it in 0..args.iterations {
        let h = loop {
            if let Some(h) = net.head().filter(|h| h.number > last) {
                break h;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        last = h.number;
        let mut keys: Vec<Key> =
            learned.accounts.iter().map(|&address| Key::Account { root: h.state_root, address }).collect();
        keys.extend(learned.storage.iter().map(|&(address, slot)| Key::Storage { root: h.state_root, address, slot }));
        for (&n, mode) in args.sweep.iter().flat_map(|n| args.modes.iter().map(move |m| (n, m))) {
            let sched = Scheduler::new(
                ElTransport::new(net.clone()),
                skim_snap::Config {
                    per_peer_inflight: args.per_peer_inflight,
                    adaptive: mode == "aimd",
                    max_peers: (n > 0).then_some(n),
                    deadline: Duration::from_secs(60),
                    ..Default::default()
                },
            );
            // Let the fresh scheduler measure peers briefly so `max_peers` picks fast ones.
            let warm: Vec<Key> = keys.iter().take(16).copied().collect();
            let _ = sched.fetch(&warm).await;
            let t = Instant::now();
            let r = sched.fetch(&keys).await;
            let wall = t.elapsed();
            let ok = r.iter().filter(|x| x.is_ok()).count() as u64;
            let e = results.entry((n, mode.clone())).or_insert_with(|| (Samples::default(), 0, 0));
            e.0.push(wall);
            e.1 += ok;
            e.2 += r.len() as u64 - ok;
            tracing::info!(iteration = it, peers = n, %mode, live = snap_peers(&net), keys = keys.len(), ok,
                ms = wall.as_millis() as u64, keys_per_s = (keys.len() as f64 / wall.as_secs_f64()) as u64, "wave");
        }
    }
    let mut keys_sorted: Vec<_> = results.keys().cloned().collect();
    keys_sorted.sort();
    let out: Vec<_> = keys_sorted
        .iter()
        .map(|k| {
            let (w, ok, failed) = &results[k];
            serde_json::json!({"max_peers": k.0, "mode": k.1, "wave_keys": n_keys, "wall": w.json(), "ok": ok, "failed": failed,
                "keys_per_s_p50": w.pct(50.0).map(|ms| n_keys as f64 / (ms / 1e3))})
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
