//! JSON-RPC adapter (PROJECT.md §7.4), built only on `skim-sim`'s public API.
//!
//! HTTP POST, single or batch. Errors follow EIP-1474:
//! - `-32002` resource unavailable: block outside the window, or state peers couldn't serve
//! - `-32004` method not supported: out of scope for a simulation client
//! - `3` execution reverted, with the revert data
//!
//! Block tags: `latest` is the **state-ready** head. A block number inside the retained window
//! and EIP-1898 `{blockHash}` / `{blockNumber}` are accepted. `pending` reads the latest state,
//! except that `eth_getTransactionCount` counts transactions sent through skim and not yet
//! included. `safe` and `finalized` are the light client's verified and finalized blocks;
//! `earliest` is not tracked and answers `-32002`. Within one batch, `latest` is fixed to one
//! block.
//!
//! Transactions, receipts and logs come from [`crate::chaindata`].

use std::sync::Arc;

use alloy_consensus::Header;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_rpc_types_eth::{BlockId, BlockNumberOrTag, TransactionRequest};
use axum::{Json, Router, extract::State, routing::post};
use revm::{
    context::{TxEnv, result::ExecutionResult},
    primitives::TxKind,
};
use serde_json::{Value, json};
use skim_bal::StateKey;
use skim_sim::{Mode, SimError, Simulator};
use skim_state::{BlockRef, StateError};

use crate::chaindata::{ChainData, DataError};

/// `skim run --debug-rpc`: diagnostic methods for measurements (`skim_debugSetSnapHedge`).
/// Off by default: they don't exist on a normal node.
pub static DEBUG_RPC: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub const CLIENT_VERSION: &str = concat!("skim/", env!("CARGO_PKG_VERSION"), " (P2P-backed simulation; unverified state)");

#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }
    /// `-32002` with `data: {reason, retry}`: why the request can't be answered now, and whether
    /// asking again (shortly, unchanged) can succeed. Reasons are stable identifiers.
    fn unavailable_because(m: impl Into<String>, reason: &'static str, retry: bool) -> Self {
        Self { code: -32002, message: m.into(), data: Some(json!({"reason": reason, "retry": retry})) }
    }
    fn unavailable(m: impl Into<String>) -> Self {
        Self::unavailable_because(m, "unavailable", true)
    }
    fn unsupported(method: &str) -> Self {
        Self::new(-32004, format!("{method} is not supported: skim is a simulation client"))
    }
    fn params(m: impl Into<String>) -> Self {
        Self::new(-32602, m)
    }
}

impl From<SimError> for RpcError {
    fn from(e: SimError) -> Self {
        match e {
            SimError::Evm(m) => Self::new(-32000, m),
            SimError::Unsupported(m) => Self::params(format!("not supported: {m}")),
            SimError::Invalid { code, message } => Self::new(code, message),
            other => {
                let (reason, retry) = match &other {
                    SimError::NoStatePeers(_) => ("no_state_peers", true),
                    SimError::Incomplete(_) => ("state_incomplete", true),
                    SimError::StateUnavailable(_) => ("not_held", false),
                    SimError::NotArchived { .. } => ("not_archived", false),
                    SimError::NoHeader(_) => ("no_header", true),
                    SimError::State(StateError::OutOfWindow(_)) => ("outside_window", false),
                    SimError::State(StateError::Reorged { .. }) => ("reorged", true),
                    SimError::State(StateError::LimitReached(_)) => ("key_limit", false),
                    _ => ("unavailable", true),
                };
                Self::unavailable_because(other.to_string(), reason, retry)
            }
        }
    }
}

type R = Result<Value, RpcError>;

const OUT_OF_SCOPE: &[&str] = &[
    "eth_sendTransaction", // skim holds no keys: sign locally, then eth_sendRawTransaction
    "eth_newFilter",
    "eth_getFilterChanges",
    "eth_subscribe",
];

/// The EVM input for a mined transaction, exactly as signed.
fn signed_tx_env(tx: &skim_el::SignedTx, from: Address) -> Result<TxEnv, RpcError> {
    use alloy_consensus::{Transaction as _, private::alloy_eips::Typed2718};
    if tx.as_frame().is_some() {
        return Err(RpcError::unavailable("an EIP-8141 frame transaction can't be replayed: skim's EVM has no Hegota rules (docs/guide/hegota.md)"));
    }
    let mut b = TxEnv::builder()
        .caller(from)
        .kind(tx.kind())
        .data(tx.input().clone())
        .value(tx.value())
        .gas_limit(tx.gas_limit())
        .nonce(tx.nonce())
        .chain_id(tx.chain_id())
        .tx_type(Some(tx.ty()));
    b = match tx.gas_price() {
        Some(p) => b.gas_price(p),
        None => b.gas_price(tx.max_fee_per_gas()).gas_priority_fee(tx.max_priority_fee_per_gas()),
    };
    if let Some(al) = tx.access_list() {
        b = b.access_list(al.clone());
    }
    if let Some(auth) = tx.authorization_list() {
        b = b.authorization_list_signed(auth.to_vec());
    }
    if let Some(h) = tx.blob_versioned_hashes() {
        b = b.blob_hashes(h.to_vec()).max_fee_per_blob_gas(tx.max_fee_per_blob_gas().unwrap_or_default());
    }
    b.build().map_err(|e| RpcError::new(-32000, format!("transaction not replayable: {e:?}")))
}

/// One `eth_simulateV1` block: environment fields and each call's outcome (geth's shape).
fn simulated_block_json(b: &skim_sim::multi::SimBlockResult) -> Value {
    let number: u64 = b.env.number.saturating_to();
    let mut log_index = 0u64;
    let mut gas_total = 0u64;
    let calls: Vec<Value> = b
        .calls
        .iter()
        .enumerate()
        .map(|(i, r)| {
            gas_total += r.tx_gas_used();
            let logs: Vec<Value> = r
                .logs()
                .iter()
                .map(|l| {
                    let v = json!({"address": l.address, "topics": l.topics(), "data": l.data.data, "blockNumber": hex_u64(number),
                        "transactionIndex": hex_u64(i as u64), "logIndex": hex_u64(log_index), "removed": false});
                    log_index += 1;
                    v
                })
                .collect();
            let mut c = json!({"gasUsed": hex_u64(r.tx_gas_used()), "logs": logs,
                "returnData": r.output().cloned().unwrap_or_default(), "status": if r.is_success() { "0x1" } else { "0x0" }});
            match r {
                ExecutionResult::Revert { output, .. } => c["error"] = json!({"code": 3, "message": "execution reverted", "data": output}),
                ExecutionResult::Halt { reason, .. } => c["error"] = json!({"code": -32015, "message": format!("{reason:?}")}),
                ExecutionResult::Success { .. } => {}
            }
            c
        })
        .collect();
    json!({
        "number": hex_u64(number), "timestamp": hex_u64(b.env.timestamp.saturating_to()), "gasLimit": hex_u64(b.env.gas_limit),
        "gasUsed": hex_u64(gas_total), "miner": b.env.beneficiary, "baseFeePerGas": hex_u64(b.env.basefee),
        "prevRandao": b.env.prevrandao, "calls": calls,
    })
}

fn hex_u64(n: u64) -> Value {
    Value::String(format!("{n:#x}"))
}

fn param<T: serde::de::DeserializeOwned>(params: &Value, i: usize) -> Result<Option<T>, RpcError> {
    match params.get(i) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => serde_json::from_value(v.clone()).map(Some).map_err(|e| RpcError::params(format!("param {i}: {e}"))),
    }
}

fn need<T: serde::de::DeserializeOwned>(params: &Value, i: usize) -> Result<T, RpcError> {
    param(params, i)?.ok_or_else(|| RpcError::params(format!("missing param {i}")))
}

pub struct Rpc {
    pub sim: Arc<Simulator>,
    pub subs: Option<Arc<skim_sim::subscription::Subscriptions>>,
    pub chain: Option<Arc<ChainData>>,
    pub light: Option<Arc<crate::light::Light>>,
    /// `latest` = highest block the sync committee signed off on.
    pub require_verified: bool,
    /// Pushes for WebSocket subscriptions.
    pub events: tokio::sync::broadcast::Sender<WsEvent>,
    /// `eth_call` results by (block hash, request). State at a block hash never changes, so an
    /// identical call there always has the same outcome; apps polling the same call every block
    /// hit this. Only fully resolved outcomes (results and reverts) are cached.
    pub call_cache: parking_lot::Mutex<CallCache>,
    pub cache_calls: bool,
    /// Usage and latency history for `/dashboard`.
    pub dash: Arc<crate::dashboard::Dashboard>,
    /// Consensus node counters (envelopes served to peers).
    pub cl_stats: Option<Arc<std::sync::Mutex<skim_cl::NodeStats>>>,
}

/// Addresses a call read (kept with cached results so cache hits still count as usage).
type Reads = Arc<[Address]>;

/// A cached outcome: the result, or a revert's (code, message, data).
type CachedResult = Result<Value, (i64, String, Option<Value>)>;

#[derive(Default)]
pub struct CallCache {
    map: alloy_primitives::map::HashMap<(B256, B256), (CachedResult, Reads, usize)>,
    /// Estimated bytes the entries hold (see [`json_bytes`]), kept as entries come and go.
    bytes: usize,
}

/// Rough heap size of a JSON value: string and number payloads plus a fixed cost per node.
fn json_bytes(v: &Value) -> usize {
    32 + match v {
        Value::String(s) => s.len(),
        Value::Array(a) => a.iter().map(json_bytes).sum(),
        Value::Object(o) => o.iter().map(|(k, v)| k.len() + json_bytes(v)).sum(),
        _ => 0,
    }
}

impl CallCache {
    const MAX: usize = 20_000;
    /// Entries hold results of any size: the count alone doesn't bound memory.
    const MAX_BYTES: usize = 32 << 20;

    fn get(&self, k: &(B256, B256)) -> Option<(R, Reads)> {
        self.map.get(k).map(|(r, a, _)| (r.clone().map_err(|(code, message, data)| RpcError { code, message, data }), a.clone()))
    }

    fn put(&mut self, k: (B256, B256), r: &R, reads: Reads) {
        let v = match r {
            Ok(v) => Ok(v.clone()),
            Err(e) if e.code == 3 => Err((e.code, e.message.clone(), e.data.clone())),
            Err(_) => return, // unavailable / invalid: not a property of the block
        };
        if self.map.len() >= Self::MAX || self.bytes >= Self::MAX_BYTES {
            // Keys name a block hash: everything here is for recent blocks, and starting over
            // only costs re-executing calls that are asked again.
            self.map.clear();
            self.map.shrink_to(Self::MAX / 4);
            self.bytes = 0;
        }
        let size = 64 + reads.len() * 20 + match &v {
            Ok(v) => json_bytes(v),
            Err((_, m, d)) => m.len() + d.as_ref().map_or(0, json_bytes),
        };
        if let Some((_, _, old)) = self.map.insert(k, (v, reads, size)) {
            self.bytes -= old;
        }
        self.bytes += size;
    }

    pub fn part(&self) -> crate::chaindata::MemPart {
        crate::chaindata::MemPart { part: "rpc.call_cache", entries: self.map.len(), bytes: self.bytes, capacity: self.map.capacity() }
    }
}

/// Requests of one JSON-RPC batch answered at the same time.
const BATCH_CONCURRENCY: usize = 8;

/// What WebSocket subscribers are told about.
#[derive(Debug, Clone)]
pub enum WsEvent {
    /// A block's state is ready.
    NewHead(BlockRef),
    /// Subscriptions were maintained at this block (readiness and `on_block` results).
    Subscriptions(u64),
}

impl From<DataError> for RpcError {
    fn from(e: DataError) -> Self {
        match e {
            DataError::Unavailable(m) => Self::unavailable_because(m, "not_served", true),
            DataError::Expired(m) => Self::unavailable_because(m, "outside_retention", false),
            DataError::ChainChanged(m) => Self::unavailable_because(m, "chain_changed", true),
            DataError::Invalid(m) => Self::new(-32000, m),
            DataError::RangeTooLarge(n) => Self::new(-32005, format!("block range too large: at most {n} blocks per eth_getLogs")),
            DataError::Unsupported(m) => Self::new(-32004, m),
        }
    }
}

impl Rpc {
    fn resolve_block(&self, id: Option<BlockId>, latest: &Option<BlockRef>) -> Result<BlockRef, RpcError> {
        let store = &self.sim.store;
        let latest = || {
            // Past a fork skim doesn't implement, its head is frozen at the last block before
            // it: `latest` would be stale state reported as current. Explicit blocks and the
            // verified tags still answer. (Review: `latest` kept answering, with success.)
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            if let Some(fork) = crate::forks::get().and_then(|p| p.stopped(now)) {
                return Err(RpcError::unavailable_because(format!("the chain passed {fork}, which skim doesn't implement: `latest` is no longer followed"), "fork_unsupported", false));
            }
            latest.ok_or_else(|| RpcError::unavailable_because("not following the chain yet", "starting", true))
        };
        match id.unwrap_or(BlockId::Number(BlockNumberOrTag::Latest)) {
            BlockId::Number(BlockNumberOrTag::Latest) => latest(),
            BlockId::Number(BlockNumberOrTag::Number(n)) => store
                .block(n)
                .or_else(|| store.archived_block(n).map(|(b, _)| b))
                .ok_or_else(|| RpcError::unavailable_because(format!("block {n} is outside the retained window and not archived"), "outside_window", false)),
            // Pending state = latest state (skim keeps no mempool); see eth_getTransactionCount.
            BlockId::Number(BlockNumberOrTag::Pending) => latest(),
            BlockId::Number(t @ (BlockNumberOrTag::Safe | BlockNumberOrTag::Finalized)) => {
                // `safe`: signed by a sync-committee supermajority; `finalized`: finalized
                // beacon block's execution block. Both from the light client.
                let st = self.light.as_ref().map(|l| l.status()).unwrap_or_default();
                let v = if t == BlockNumberOrTag::Safe { st.verified_block } else { st.finalized_block };
                let v = v.ok_or_else(|| RpcError::unavailable_because(format!("no {t} block verified yet"), "not_verified", true))?;
                // The verified hash, at its height: never whichever block holds that height now.
                crate::light::resolve(store, &v).ok_or_else(|| {
                    RpcError::unavailable_because(format!("{t} block {} ({}) is not on the selected chain or outside the retained window", v.number, v.hash), "not_verified", true)
                })
            }
            BlockId::Number(t) => Err(RpcError::unavailable(format!("block tag {t} is not tracked yet"))),
            BlockId::Hash(h) => {
                let head = latest()?;
                (head.number.saturating_sub(512)..=head.number)
                    .rev()
                    .filter_map(|n| store.block(n))
                    .find(|b| b.hash == h.block_hash)
                    .or_else(|| store.archived_number(&h.block_hash).and_then(|n| store.archived_block(n)).map(|(b, _)| b))
                    .ok_or_else(|| RpcError::unavailable_because(format!("block {} is not in the window or the archive", h.block_hash), "outside_window", false))
            }
        }
    }

    /// Replays mined transactions of block `n` (one, or all) with a tracer. Each runs on its exact
    /// pre-state: the parent block's state plus the block's access-list changes before it, so no
    /// earlier transaction is re-executed. Every replay must reproduce the transaction's receipt
    /// (status and gas used); if it doesn't, the trace is refused.
    async fn replay(&self, n: u64, bh: B256, only: Option<usize>, opts: alloy_rpc_types_trace::geth::GethDebugTracingOptions) -> Result<Vec<(B256, Value)>, RpcError> {
        let store = &self.sim.store;
        let chain = self.chain()?;
        let block = store.block(n).filter(|b| b.hash == bh).ok_or_else(|| RpcError::unavailable_because(format!("block {n} is outside the retained window"), "outside_window", false))?;
        let parent = store.block(n - 1).ok_or_else(|| RpcError::unavailable_because(format!("parent of block {n} is outside the retained window"), "outside_window", false))?;
        let header = self.sim.header_arc(&block).ok_or_else(|| RpcError::unavailable(format!("no header for block {n}")))?;
        let bal = chain.block_bal(bh, &header).await?;
        let (raw, receipts) = chain.block_txs_and_receipts(bh, &header).await?;
        // Fetch up front, in one wave, what the access list says the replay will touch: the whole
        // block's keys for a block trace, or the keys the one transaction wrote.
        self.sim.prefetch_bal_keys(&parent, &bal, only.map(|i| i as u64)).await;
        let indices: Vec<usize> = match only {
            Some(i) => vec![i],
            None => (0..raw.len()).collect(),
        };
        let mut out = Vec::with_capacity(indices.len());
        for i in indices {
            let rawtx = raw.get(i).ok_or_else(|| RpcError::unavailable("transaction index out of range"))?;
            let (tx, from) = ChainData::decode_tx(rawtx)?;
            let prestate = skim_bal::BlockWrites::before(&bal, i as u64 + 1);
            let base = skim_sim::multi::TraceBase {
                state_at: parent,
                env: header.clone(),
                prestate: Some(&prestate),
                overrides: None,
                block_overrides: None,
                validating: true,
            };
            let (trace, plain) = self.sim.trace(&base, signed_tx_env(&tx, from)?, opts.clone()).await?;
            // The replay must reproduce what the chain recorded.
            let r = receipts.get(i).ok_or_else(|| RpcError::unavailable("receipt index out of range"))?;
            let gas = r.cumulative_gas_used - if i == 0 { 0 } else { receipts[i - 1].cumulative_gas_used };
            if plain.is_success() != r.success || plain.tx_gas_used() != gas {
                return Err(RpcError::new(
                    -32000,
                    format!("replay of tx {i} in block {n} diverged from its receipt (status {} vs {}, gas {} vs {gas}); refusing the trace", plain.is_success(), r.success, plain.tx_gas_used()),
                ));
            }
            out.push((alloy_primitives::keccak256(rawtx), serde_json::to_value(trace).unwrap()));
        }
        Ok(out)
    }

    /// `eth_call` without the result cache.
    /// The result, and the addresses the call read (for usage counts).
    async fn eth_call_uncached(&self, params: &Value, b: &BlockRef) -> (R, Reads) {
        match self.eth_call_inner(params, b).await {
            Ok((r, reads)) => (r, reads),
            Err(e) => (Err(e), Arc::from([])),
        }
    }

    async fn eth_call_inner(&self, params: &Value, b: &BlockRef) -> Result<(R, Reads), RpcError> {
        let sim = &self.sim;
        let req: TransactionRequest = need(params, 0)?;
        let header = self.header(b)?;
        let state = param::<alloy_rpc_types_eth::state::StateOverride>(params, 2)?;
        let block = param::<alloy_rpc_types_eth::BlockOverrides>(params, 3)?;
        if state.is_some() || block.is_some() {
            // Overrides: the call runs on an overlay; the store is never modified. (Review: block
            // overrides, parameter 3, were ignored.)
            let to: Reads = req.to.and_then(|k| k.to().copied()).into_iter().collect();
            let blocks = [skim_sim::multi::SimBlock { block, state, calls: vec![self.tx_env(req, &header)?.into()] }];
            let out = sim.simulate(b, &blocks, skim_sim::multi::Placement::SameBlock).await?;
            self.dash.record_hit(b.number, &to);
            return Ok((Self::exec_result(&out[0].calls[0]), to));
        }
        let out = sim.call(b, self.tx_env(req, &header)?, Mode::Resolve { max_rounds: sim.max_rounds.load(std::sync::atomic::Ordering::Relaxed) }).await?;
        self.dash.record_call(b.number, &out.access, &out.fetched_accounts, &out.fetched_storage);
        let mut reads: Vec<Address> = out.access.accounts.clone();
        reads.extend(out.access.storage.iter().map(|(a, _)| *a));
        reads.sort();
        reads.dedup();
        tracing::debug!(number = b.number, predicted_keys = out.predicted.as_ref().map_or(0, |p| p.keys),
            predicted_ms = out.predicted.as_ref().map_or(0.0, |p| p.wall_ms), rounds = out.rounds.len(),
            round_keys = out.rounds.iter().map(|r| r.keys).sum::<usize>(), "eth_call fetches");
        Ok((Self::exec_result(&out.result), reads.into()))
    }

    fn chain(&self) -> Result<&Arc<ChainData>, RpcError> {
        self.chain.as_ref().ok_or_else(|| RpcError::unavailable("chain data is not available in this mode"))
    }

    fn header(&self, b: &BlockRef) -> Result<std::sync::Arc<Header>, RpcError> {
        self.sim.header_arc(b).ok_or_else(|| RpcError::unavailable(format!("no header for block {}", b.number)))
    }

    /// Builds the execution for a call request. Every field that changes execution is honoured
    /// (fees, access list, EIP-7702 authorizations) or the request is rejected: nothing is
    /// silently dropped.
    fn tx_env(&self, req: TransactionRequest, header: &Header) -> Result<TxEnv, RpcError> {
        if req.blob_versioned_hashes.as_ref().is_some_and(|v| !v.is_empty()) || req.max_fee_per_blob_gas.is_some() || req.sidecar.is_some() {
            return Err(RpcError::params("blob transaction fields are not supported in calls"));
        }
        if req.gas_price.is_some() && (req.max_fee_per_gas.is_some() || req.max_priority_fee_per_gas.is_some()) {
            return Err(RpcError::params("both gasPrice and (maxFeePerGas or maxPriorityFeePerGas) specified"));
        }
        let gas = req.gas.unwrap_or(header.gas_limit).min(skim_sim::multi::call_gas_cap(header));
        let data = req.input.input().cloned().unwrap_or_default();
        let mut b = TxEnv::builder()
            .caller(req.from.unwrap_or_default())
            .kind(req.to.unwrap_or(TxKind::Create))
            .data(data)
            .value(req.value.unwrap_or_default())
            .gas_limit(gas)
            .nonce(req.nonce.unwrap_or_default())
            .chain_id(Some(skim_sim::env::chain_id()));
        // Fees: as nodes do for eth_call, 0 unless given (GASPRICE then sees the given price).
        b = match (req.gas_price, req.max_fee_per_gas, req.max_priority_fee_per_gas) {
            (Some(p), _, _) => b.gas_price(p),
            (None, None, None) => b.gas_price(0),
            (None, max, prio) => {
                let max = max.unwrap_or(0);
                b.gas_price(max).gas_priority_fee(Some(prio.unwrap_or(0).min(max)))
            }
        };
        if let Some(al) = req.access_list {
            b = b.access_list(al);
        }
        if let Some(auth) = req.authorization_list {
            if !auth.is_empty() {
                b = b.authorization_list_signed(auth);
            }
        }
        b.build().map_err(|e| RpcError::params(format!("invalid call: {e:?}")))
    }

    fn exec_result(r: &ExecutionResult) -> R {
        match r {
            ExecutionResult::Success { output, .. } => Ok(json!(Bytes::copy_from_slice(output.data()))),
            ExecutionResult::Revert { output, .. } => {
                Err(RpcError { code: 3, message: "execution reverted".into(), data: Some(json!(output)) })
            }
            ExecutionResult::Halt { reason, .. } => Err(RpcError::new(-32000, format!("execution halted: {reason:?}"))),
        }
    }

    pub async fn dispatch(&self, method: &str, params: &Value, latest: &Option<BlockRef>) -> R {
        let sim = &self.sim;
        match method {
            "web3_clientVersion" => Ok(json!(CLIENT_VERSION)),
            "eth_chainId" => Ok(hex_u64(skim_sim::env::chain_id())),
            "net_version" => Ok(json!(skim_sim::env::chain_id().to_string())),
            "net_listening" => Ok(json!(true)),
            // skim holds no keys: wallets and libraries that ask get an empty list, not an error.
            "eth_accounts" | "eth_requestAccounts" => Ok(json!([])),
            "skim_debugSetSnapHedge" if DEBUG_RPC.load(std::sync::atomic::Ordering::Relaxed) => {
                let mode: String = need(params, 0)?;
                let adaptive = match mode.as_str() {
                    "adaptive" => true,
                    "fixed" => false,
                    _ => return Err(RpcError::params("mode is `adaptive` or `fixed`")),
                };
                self.sim.sched.set_hedge_adaptive(adaptive);
                Ok(json!(mode))
            }
            "eth_syncing" => Ok(json!(latest.is_none())),
            "eth_blockNumber" => Ok(hex_u64(self.resolve_block(None, latest)?.number)),
            "eth_getBlockByNumber" | "eth_getBlockByHash" => {
                let id: BlockId = if method == "eth_getBlockByHash" {
                    BlockId::from(need::<B256>(params, 0)?)
                } else {
                    BlockId::Number(need::<BlockNumberOrTag>(params, 0)?)
                };
                let b = match self.resolve_block(Some(id), latest) {
                    Ok(b) => b,
                    Err(e) if e.code == -32002 => return Ok(Value::Null),
                    Err(e) => return Err(e),
                };
                let h = self.header(&b)?;
                let full: bool = param(params, 1)?.unwrap_or(false);
                let mut v = serde_json::to_value(alloy_rpc_types_eth::Header::new((*h).clone())).unwrap();
                let txs = match &self.chain {
                    Some(c) => c.block_transactions_json(b.number, b.hash, &h, full).await?,
                    None => Vec::new(),
                };
                v["transactions"] = Value::Array(txs);
                v["uncles"] = json!([]);
                Ok(v)
            }
            "eth_feeHistory" => {
                let count: alloy_primitives::U64 = need(params, 0)?;
                let newest = self.resolve_block(Some(BlockId::Number(need::<BlockNumberOrTag>(params, 1)?)), latest)?;
                let percentiles: Vec<f64> = param(params, 2)?.unwrap_or_default();
                Ok(self.chain()?.fee_history(count.to(), newest.number, &percentiles).await?)
            }
            "eth_getBlockReceipts" => {
                let id: BlockId = need(params, 0)?;
                let b = match self.resolve_block(Some(id), latest) {
                    Ok(b) => b,
                    Err(e) if e.code == -32002 => return Ok(Value::Null),
                    Err(e) => return Err(e),
                };
                let h = self.header(&b)?;
                Ok(self.chain()?.block_receipts_json(b.number, b.hash, &h).await?)
            }
            "eth_getTransactionByBlockNumberAndIndex" | "eth_getTransactionByBlockHashAndIndex" => {
                let id: BlockId = if method.ends_with("HashAndIndex") {
                    BlockId::from(need::<B256>(params, 0)?)
                } else {
                    BlockId::Number(need::<BlockNumberOrTag>(params, 0)?)
                };
                let i: alloy_primitives::U64 = need(params, 1)?;
                let Ok(b) = self.resolve_block(Some(id), latest) else { return Ok(Value::Null) };
                let h = self.header(&b)?;
                Ok(self.chain()?.transaction_at(b.number, b.hash, &h, i.to::<usize>()).await?)
            }
            "eth_getBlockTransactionCountByNumber" | "eth_getBlockTransactionCountByHash" => {
                let id: BlockId = if method.ends_with("ByHash") {
                    BlockId::from(need::<B256>(params, 0)?)
                } else {
                    BlockId::Number(need::<BlockNumberOrTag>(params, 0)?)
                };
                let Ok(b) = self.resolve_block(Some(id), latest) else { return Ok(Value::Null) };
                let h = self.header(&b)?;
                let n = self.chain()?.block_transactions_json(b.number, b.hash, &h, false).await?.len();
                Ok(hex_u64(n as u64))
            }
            // Post-merge blocks have no uncles.
            "eth_getUncleCountByBlockNumber" | "eth_getUncleCountByBlockHash" => Ok(hex_u64(0)),
            "eth_getUncleByBlockNumberAndIndex" | "eth_getUncleByBlockHashAndIndex" => Ok(Value::Null),
            "eth_getBalance" | "eth_getTransactionCount" | "eth_getCode" => {
                let a: Address = need(params, 0)?;
                let b = self.resolve_block(param(params, 1)?, latest)?;
                match method {
                    "eth_getBalance" => Ok(json!(sim.read(&b, StateKey::Balance(a)).await?)),
                    "eth_getTransactionCount" => {
                        let n: u64 = sim.read(&b, StateKey::Nonce(a)).await?.saturating_to();
                        let pending = matches!(param::<BlockId>(params, 1)?, Some(BlockId::Number(BlockNumberOrTag::Pending)));
                        Ok(hex_u64(match (&self.chain, pending) {
                            (Some(c), true) => c.pending_nonce(a, n),
                            _ => n,
                        }))
                    }
                    _ => Ok(json!(sim.code(&b, a).await?)),
                }
            }
            "eth_getStorageAt" => {
                let a: Address = need(params, 0)?;
                let slot: U256 = need(params, 1)?;
                let b = self.resolve_block(param(params, 2)?, latest)?;
                let v = sim.read(&b, StateKey::Storage(a, slot)).await?;
                Ok(json!(B256::from(v)))
            }
            "eth_call" => {
                let b = self.resolve_block(param(params, 1)?, latest)?;
                let p = |i: usize| params.get(i).unwrap_or(&Value::Null);
                let key = (b.hash, alloy_primitives::keccak256(format!("{}|{}|{}", p(0), p(2), p(3))));
                if !self.cache_calls {
                    return self.eth_call_uncached(params, &b).await.0;
                }
                let hit = self.call_cache.lock().get(&key);
                if let Some((r, reads)) = hit {
                    self.dash.record_hit(b.number, &reads);
                    return r;
                }
                let (r, reads) = self.eth_call_uncached(params, &b).await;
                self.call_cache.lock().put(key, &r, reads);
                r
            }
            "eth_estimateGas" => {
                let req: TransactionRequest = need(params, 0)?;
                // Refused rather than estimated without them. (Review: state overrides were
                // ignored.)
                if params.get(2).is_some_and(|v| !v.is_null()) || params.get(3).is_some_and(|v| !v.is_null()) {
                    return Err(RpcError::params("eth_estimateGas: state and block overrides are not supported"));
                }
                let b = self.resolve_block(param(params, 1)?, latest)?;
                let header = self.header(&b)?;
                // The caller's `gas` within the same cap as `eth_call` (`tx_env`). (Review: it
                // replaced the cap, so `gas = 2^64 - 1` on a looping contract ran without end,
                // holding a CPU slot.)
                let mut tx = self.tx_env(req, &header)?;
                let cap = tx.gas_limit;
                let first = sim.call(&b, tx.clone(), Mode::Resolve { max_rounds: sim.max_rounds.load(std::sync::atomic::Ordering::Relaxed) }).await?;
                Self::exec_result(&first.result)?;
                // State is now local: binary-search the lowest gas limit that still succeeds.
                let (mut lo, mut hi) = (first.result.tx_gas_used().saturating_sub(1), cap);
                while lo + 1 < hi {
                    let mid = lo + (hi - lo) / 2;
                    tx.gas_limit = mid;
                    let ok = matches!(sim.call(&b, tx.clone(), Mode::WarmOnly).await, Ok(o) if o.result.is_success());
                    if ok { hi = mid } else { lo = mid }
                }
                Ok(hex_u64(hi))
            }
            "eth_gasPrice" | "eth_maxPriorityFeePerGas" => {
                let b = self.resolve_block(None, latest)?;
                let base = self.header(&b)?.base_fee_per_gas.unwrap_or_default() as u128;
                let tip = 1_000_000_000u128;
                Ok(json!(U256::from(if method == "eth_gasPrice" { base + tip } else { tip })))
            }
            "eth_sendRawTransaction" => {
                let chain = self.chain()?;
                let raw: Bytes = need(params, 0)?;
                let (tx, from) = chain.decode_raw(&raw)?;
                // The early errors a node's pool would give, from verified state at the head.
                use alloy_consensus::Transaction as _;
                let b = self.resolve_block(None, latest)?;
                let nonce: u64 = sim.read(&b, StateKey::Nonce(from)).await?.saturating_to();
                if tx.nonce() < nonce {
                    return Err(RpcError::new(-32000, format!("nonce too low: next nonce {nonce}, tx nonce {}", tx.nonce())));
                }
                let balance = sim.read(&b, StateKey::Balance(from)).await?;
                let cost = U256::from(tx.gas_limit()) * U256::from(tx.max_fee_per_gas()) + tx.value();
                if balance < cost {
                    return Err(RpcError::new(-32000, format!("insufficient funds for gas * price + value: balance {balance}, cost {cost}")));
                }
                let (hash, peers) = chain.send(tx, from);
                if peers == 0 {
                    tracing::warn!(tx = %hash, "no EL peers connected; will re-broadcast");
                }
                Ok(json!(hash))
            }
            "eth_getTransactionByHash" => Ok(self.chain()?.transaction(need(params, 0)?).await?),
            "eth_getTransactionReceipt" => Ok(self.chain()?.receipt(need(params, 0)?).await?),
            "eth_getLogs" => {
                let chain = self.chain()?;
                let filter: alloy_rpc_types_eth::Filter = need(params, 0)?;
                let head = self.resolve_block(None, latest)?.number;
                let (from, to, at) = match filter.block_option {
                    alloy_rpc_types_eth::FilterBlockOption::AtBlockHash(h) => {
                        let n = chain.number_of(&h).ok_or_else(|| RpcError::unavailable(format!("block {h} is not known")))?;
                        (n, n, Some(h))
                    }
                    alloy_rpc_types_eth::FilterBlockOption::Range { from_block, to_block } => {
                        // `safe`/`finalized` as every other method resolves them; this keeps their
                        // own error reasons (`not_verified`).
                        for t in [from_block, to_block].into_iter().flatten() {
                            if matches!(t, BlockNumberOrTag::Safe | BlockNumberOrTag::Finalized) {
                                self.resolve_block(Some(BlockId::Number(t)), latest)?;
                            }
                        }
                        let verified = |t: BlockNumberOrTag| {
                            self.resolve_block(Some(BlockId::Number(t)), latest).map(|b| b.number).map_err(|e| DataError::Unavailable(e.message))
                        };
                        let (a, b) = ChainData::range(&filter, head, verified)?;
                        if a > head {
                            return Ok(json!([])); // nothing past the head yet
                        }
                        (a, b, None)
                    }
                };
                Ok(chain.logs(&filter, from, to, at).await?)
            }
            "eth_simulateV1" => {
                let payload: alloy_rpc_types_eth::simulate::SimulatePayload = need(params, 0)?;
                let b = self.resolve_block(param(params, 1)?, latest)?;
                let header = self.header(&b)?;
                let mut blocks = Vec::with_capacity(payload.block_state_calls.len());
                for blk in payload.block_state_calls {
                    let mut calls = Vec::with_capacity(blk.calls.len());
                    for c in blk.calls {
                        let auto_nonce = c.nonce.is_none();
                        let auto_fee = c.gas_price.is_none() && c.max_fee_per_gas.is_none() && c.max_priority_fee_per_gas.is_none();
                        let auto_gas = c.gas.is_none();
                        calls.push(skim_sim::multi::SimCall { tx: self.tx_env(c, &header)?, auto_nonce, auto_fee, auto_gas });
                    }
                    blocks.push(skim_sim::multi::SimBlock { block: blk.block_overrides, state: blk.state_overrides, calls });
                }
                let opts = skim_sim::multi::SimOptions { validation: payload.validation, trace_transfers: payload.trace_transfers };
                let out = sim.simulate_with(&b, &blocks, skim_sim::multi::Placement::NextBlocks, opts).await?;
                Ok(Value::Array(out.iter().map(simulated_block_json).collect()))
            }
            "eth_getProof" => {
                let a: Address = need(params, 0)?;
                // Storage keys may be short hex ("0x0"), and are echoed back exactly as given.
                let raw_keys: Vec<String> = need(params, 1)?;
                let keys: Vec<B256> = raw_keys
                    .iter()
                    .map(|k| k.parse::<U256>().map(|v| B256::from(v.to_be_bytes::<32>())).map_err(|e| RpcError::params(format!("storage key {k}: {e}"))))
                    .collect::<Result<_, _>>()?;
                let b = self.resolve_block(param(params, 2)?, latest)?;
                let slots: Vec<U256> = keys.iter().map(|k| U256::from_be_bytes(k.0)).collect();
                // Peers serve proofs only for recent state roots; the newest block may not be
                // served for a second or two after it arrives.
                let tr = sim.sched.transport();
                for attempt in 0..6 {
                    let peers: Vec<_> = tr.net.peers().into_iter().filter(|(_, p)| p.snap1 || p.snap2).map(|(id, _)| id).collect();
                    for peer in peers.into_iter().take(4) {
                        if let Ok(Some(p)) = tr.get_proof(peer, b.state_root, a, &slots).await {
                            // An absent account reports zero hashes (as geth and Nethermind do).
                            let (balance, nonce, code_hash, storage_hash) = match &p.account {
                                Some(acc) => (acc.balance, acc.nonce, acc.code_hash, acc.storage_root),
                                None => (U256::ZERO, 0, B256::ZERO, B256::ZERO),
                            };
                            let storage: Vec<Value> = raw_keys
                                .iter()
                                .zip(p.storage.iter())
                                .map(|(k, (_, v, proof))| json!({"key": k, "value": v, "proof": proof}))
                                .collect();
                            return Ok(json!({"address": a, "accountProof": p.account_proof, "balance": balance, "codeHash": code_hash,
                                "nonce": hex_u64(nonce), "storageHash": storage_hash, "storageProof": storage}));
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500 * (attempt + 1))).await;
                }
                Err(RpcError::unavailable_because(format!("no peer served a proof for block {} (peers keep only recent state roots)", b.number), "outside_retention", false))
            }
            "eth_createAccessList" => {
                let req: TransactionRequest = need(params, 0)?;
                let b = self.resolve_block(param(params, 1)?, latest)?;
                let overrides: Option<alloy_rpc_types_eth::state::StateOverride> = param(params, 2)?;
                let header = self.header(&b)?;
                let out = sim.create_access_list(&b, self.tx_env(req, &header)?, overrides).await?;
                let mut v = json!({"accessList": out.access_list, "gasUsed": hex_u64(out.gas_used)});
                match &out.result {
                    ExecutionResult::Revert { .. } => v["error"] = json!("execution reverted"),
                    ExecutionResult::Halt { reason, .. } => v["error"] = json!(format!("{reason:?}")),
                    ExecutionResult::Success { .. } => {}
                }
                Ok(v)
            }
            "debug_traceTransaction" => {
                let hash: B256 = need(params, 0)?;
                let opts: alloy_rpc_types_trace::geth::GethDebugTracingOptions = param(params, 1)?.unwrap_or_default();
                let chain = self.chain()?;
                let (n, bh, _, i) = chain.locate_tx(&hash).await?.ok_or_else(|| RpcError::unavailable_because("transaction not indexed (see --tx-index)", "not_indexed", false))?;
                Ok(self.replay(n, bh, Some(i as usize), opts).await?.into_iter().next().map(|(_, t)| t).unwrap_or(Value::Null))
            }
            "debug_traceBlockByNumber" | "debug_traceBlockByHash" => {
                let id: BlockId = if method.ends_with("ByHash") {
                    BlockId::from(need::<B256>(params, 0)?)
                } else {
                    BlockId::Number(need::<BlockNumberOrTag>(params, 0)?)
                };
                let opts: alloy_rpc_types_trace::geth::GethDebugTracingOptions = param(params, 1)?.unwrap_or_default();
                let b = self.resolve_block(Some(id), latest)?;
                let traces = self.replay(b.number, b.hash, None, opts).await?;
                Ok(Value::Array(traces.into_iter().map(|(h, t)| json!({"txHash": h, "result": t})).collect()))
            }
            "debug_traceCall" => {
                let req: TransactionRequest = need(params, 0)?;
                let b = self.resolve_block(param(params, 1)?, latest)?;
                let mut raw_opts = params.get(2).cloned().unwrap_or(Value::Null);
                // muxTracer: tracers that take no config (4byte, noop) are often given `{}`.
                if let Some(cfg) = raw_opts.get_mut("tracerConfig").and_then(Value::as_object_mut) {
                    for (k, v) in cfg.iter_mut() {
                        if matches!(k.as_str(), "4byteTracer" | "noopTracer") && v.as_object().is_some_and(|o| o.is_empty()) {
                            *v = Value::Null;
                        }
                    }
                }
                let opts: alloy_rpc_types_trace::geth::GethDebugTracingCallOptions =
                    if raw_opts.is_null() { Default::default() } else { serde_json::from_value(raw_opts).map_err(|e| RpcError::params(format!("param 2: {e}")))? };
                let header = self.header(&b)?;
                let tx = self.tx_env(req, &header)?;
                let trace = sim.trace_call(&b, tx, opts.state_overrides, opts.block_overrides, opts.tracing_options).await?;
                Ok(serde_json::to_value(trace).unwrap())
            }
            "skim_status" => {
                let z = sim.store.sizes();
                let peers = sim.sched.transport().net.peers();
                let b = sim.sched.transport().net.peer_book();
                let acquisition = json!({
                    "first_snap_peer_s": b.first_snap.map(|t| t.as_secs_f64()),
                    "snap_peers_seen": b.snap_peers_seen,
                    "snap_peers_held_60s": b.snap_peers_held,
                    "sessions": b.sessions.iter().map(|(c, s, n)| json!({"client": c, "snap": s, "sessions": n})).collect::<Vec<_>>(),
                    "closes": b.closes.iter().map(|(r, n)| json!({"reason": r, "count": n})).collect::<Vec<_>>(),
                });
                Ok(json!({
                    "head": sim.store.head().map(|h| h.number),
                    "store": {
                        "chain": z.chain, "recent_bals": z.recent_bals, "keys": z.keys, "versions": z.versions,
                        "subscribed_keys": z.watched_keys, "on_demand_keys": z.temp_keys,
                        "code": z.code, "code_bytes": z.code_bytes, "admissions_refused": z.admissions_refused,
                        "on_demand_evicted": z.on_demand_evicted, "recent_bal_bytes": z.recent_bal_bytes,
                        "resolved_from_bal": z.resolved_from_bal,
                        "bal_promotions": z.bal_promotions,
                        "recent_index": {"blocks": z.recent_bals, "writes": z.recent_writes, "reads": z.recent_reads, "touched": z.recent_touched, "code_bytes": z.recent_code_bytes},
                    },
                    "light_client": self.light.as_ref().map(|l| l.status()),
                    "network": {
                        "name": skim_cl::network::current().name,
                        "glamsterdam_at": skim_cl::network::current().gloas_time(),
                        "forks": crate::forks::get().map(|f| f.json(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()))),
                    },
                    "el_peers": peers.len(),
                    "snap_peers": peers.iter().filter(|(_, p)| p.snap1 || p.snap2).count(),
                    "peer_acquisition": acquisition,
                    "served": self.served_json(),
                    // Requests sent to peers since start, by kind (continuations of partial
                    // responses count again).
                    "peer_requests": skim_el::requests::counts().into_iter().map(|(k, n)| (k.to_string(), json!(n))).collect::<serde_json::Map<_, _>>(),
                    "history": self.chain.as_ref().map(|c| c.history_json()),
                    "memory": {
                        "parts": self.memory_parts(),
                        "heap_live_bytes": crate::heap::live(),
                        "note": "parts: entries, estimated payload bytes and capacity per retained structure (payload only). heap_live_bytes: everything skim has allocated now, counted at the allocator; RSS minus it is allocator overhead (free pages kept, fragmentation) plus code and stacks",
                    },
                    "snap_fetch": {
                        "stats": sim.sched.stats(),
                        "peers": sim.sched.peer_stats().into_iter().map(|(id, p)| {
                            let client = peers.iter().find(|(x, _)| *x == id).map(|(_, i)| i.client.clone());
                            json!({"client": client, "ok": p.ok, "errors": p.errors, "ewma_ms": p.ewma_ms, "inflight": p.inflight, "limit": p.limit})
                        }).collect::<Vec<_>>(),
                    },
                }))
            }
            "skim_dashboard" => Ok(self.dashboard_json()),
            "skim_streamNext" => {
                let chain = self.chain()?;
                let req: crate::stream::Request = need(params, 0)?;
                let finalized = self.light.as_ref().and_then(|l| l.status().finalized_block).map(|b| json!({"number": b.number, "hash": b.hash}));
                // The stream's tip is `latest`: with `--require-verified`, the verified head.
                // (Review: it was always the optimistic head.)
                let (store, light, verified) = (sim.store.clone(), self.light.clone(), self.require_verified);
                // With `--require-verified` the tip keeps its authenticated hash: the stream checks
                // the range it captures against it.
                let tip = move || {
                    if verified {
                        let v = light.as_ref().and_then(|l| l.status().verified_block).and_then(|v| crate::light::resolve(&store, &v))?;
                        Some(crate::stream::Tip { number: v.number, hash: Some(v.hash) })
                    } else {
                        store.head().map(|h| crate::stream::Tip { number: h.number, hash: None })
                    }
                };
                Ok(crate::stream::next(chain, tip, self.events.subscribe(), finalized, req).await?)
            }
            // The block's raw EIP-7928 access list, exactly the bytes its header commits to with
            // `blockAccessListHash`. A client checks keccak256(raw) against the header itself
            // instead of trusting the decoded `stateChanges` of skim_streamNext (which are this
            // list, filtered by address).
            "skim_getBlockAccessList" => {
                let chain = self.chain()?;
                let hash: B256 = need(params, 0)?;
                let b = self.resolve_block(Some(BlockId::from(hash)), latest)?;
                let h = self.header(&b)?;
                let Some(committed) = h.block_access_list_hash else {
                    return Err(RpcError::unavailable(format!("block {} has no access list (before Amsterdam)", b.number)));
                };
                if chain.cached_bal(hash).is_none() {
                    chain.block_bal(hash, &h).await?;
                }
                let raw = chain.cached_bal(hash).ok_or_else(|| RpcError::unavailable(format!("block {} access list not held", b.number)))?;
                Ok(json!({"blockHash": hash, "number": hex_u64(b.number), "blockAccessListHash": committed, "raw": raw}))
            }
            "skim_archiveCoverage" => {
                let a: Address = need(params, 0)?;
                Ok(Value::Array(
                    sim.store
                        .archive_coverage(&a)
                        .into_iter()
                        .map(|(k, from, write)| json!({"key": format!("{k:?}"), "from": from, "starts_at_write": write}))
                        .collect(),
                ))
            }
            "skim_subscriptionStatus" => Ok(serde_json::to_value(self.subs.as_ref().map(|s| s.status()).unwrap_or_default()).unwrap()),
            m if OUT_OF_SCOPE.contains(&m) => Err(RpcError::unsupported(m)),
            m => Err(RpcError::new(-32601, format!("method {m} not found"))),
        }
    }

    /// A JSON-RPC batch: independent requests, answered concurrently (up to
    /// `BATCH_CONCURRENCY` at a time) against the same `latest` block, in request order. A batch
    /// of cold reads then overlaps its network waits instead of queueing them.
    /// The reorg epoch to capture together with `latest` (before it).
    fn reorg_epoch(&self) -> u64 {
        self.sim.store.reorg_epoch()
    }

    async fn batch<'a>(&'a self, reqs: Vec<&'a Value>, latest: &'a Option<BlockRef>, epoch: u64, caller: Caller) -> Vec<Value> {
        use futures::StreamExt;
        let calls: Vec<_> = reqs.into_iter().map(|r| self.one(r, latest, epoch, caller.clone())).collect();
        futures::stream::iter(calls).buffered(BATCH_CONCURRENCY).collect().await
    }

    async fn one(&self, req: &Value, latest: &Option<BlockRef>, epoch: u64, caller: Caller) -> Value {
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let Some(method) = req.get("method").and_then(Value::as_str) else {
            return json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32600, "message": "invalid request"}});
        };
        if let Caller::Page(origin) = &caller
            && method.starts_with("skim_")
        {
            return json!({"jsonrpc": "2.0", "id": id, "error": page_refused(method, &origin)});
        }
        let params = req.get("params").cloned().unwrap_or(json!([]));
        let t = std::time::Instant::now();
        let res = guarded(method, epoch, self.dispatch(method, &params, latest), || self.reorg_epoch()).await;
        let ms = t.elapsed().as_secs_f64() * 1e3;
        tracing::debug!(method, ms, ok = res.is_ok(), "rpc");
        if method != "skim_dashboard" {
            // Method names come from the request: only methods skim implements get their own
            // series (in the dashboard too, whose per-method table would otherwise grow with
            // every new name).
            let m = method_label(method, &res);
            self.dash.record_method(&m, ms, res.is_ok());
            metrics::histogram!("skim_rpc_request_duration_seconds", "method" => m.clone()).record(ms / 1e3);
            let outcome = match &res {
                Ok(_) => "ok",
                Err(e) if e.code == 3 => "reverted",
                Err(e) if e.code == -32002 => "unavailable",
                Err(_) => "error",
            };
            metrics::counter!("skim_rpc_requests_total", "method" => m, "outcome" => outcome).increment(1);
        }
        match res {
            Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
            Err(e) => {
                let mut err = json!({"code": e.code, "message": e.message});
                if let Some(d) = e.data {
                    err["data"] = d;
                }
                json!({"jsonrpc": "2.0", "id": id, "error": err})
            }
        }
    }
}

/// Methods whose answer doesn't depend on which fork is canonical, or that had a side effect
/// already (a broadcast transaction must not be reported as failed).
const REORG_SAFE: &[&str] = &[
    "eth_chainId", "net_version", "net_listening", "web3_clientVersion", "eth_accounts", "eth_requestAccounts", "eth_syncing",
    "eth_sendRawTransaction", "skim_status", "skim_dashboard", "skim_subscriptionStatus", "skim_debugSetSnapHedge",
    // Streams carry their own cursor and rewind on reorgs.
    "skim_streamNext",
];

/// Runs a request and applies the reorg guard with the epoch sampled after it finished. (Review:
/// the epoch was an argument evaluated before the await, so a reorg during the request was
/// never seen.)
async fn guarded(method: &str, epoch: u64, req: impl std::future::Future<Output = R>, now: impl Fn() -> u64) -> R {
    let res = req.await;
    reorg_guard(method, epoch, now(), res)
}

/// A request that started before a reorg (`epoch` captured with its `latest`) and finished after
/// it may have read the old fork in one place and the new one in another: refused as
/// `chain_changed` (retry), never answered.
fn reorg_guard(method: &str, epoch: u64, now: u64, res: R) -> R {
    if epoch == now || REORG_SAFE.contains(&method) {
        return res;
    }
    Err(RpcError::unavailable_because("the chain reorganized during the request; retry", "chain_changed", true))
}

/// The label a request is counted under: its method if skim implements it (dispatch didn't answer
/// "method not found"), else "other". Shared with dispatch by construction, so the set of labels
/// is bounded by the methods that exist, whatever names clients send.
fn method_label(method: &str, res: &R) -> String {
    match res {
        Err(e) if e.code == -32601 => "other".to_string(),
        _ => method.to_string(),
    }
}

impl Rpc {
    /// Refreshes the gauges and totals that mirror live state, right before a scrape.
    pub fn refresh_metrics(&self, top_addresses: usize) {
        use metrics::{counter, gauge};
        let sim = &self.sim;
        gauge!("skim_heap_live_bytes").set(crate::heap::live() as f64);
        for p in self.memory_parts() {
            gauge!("skim_memory_bytes", "part" => p.part).set(p.bytes as f64);
            gauge!("skim_memory_entries", "part" => p.part).set(p.entries as f64);
            gauge!("skim_memory_capacity", "part" => p.part).set(p.capacity as f64);
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        if let Some(h) = sim.store.head() {
            gauge!("skim_head_block").set(h.number as f64);
            gauge!("skim_head_age_seconds").set(now - h.timestamp as f64);
        }
        let peers = sim.sched.transport().net.peers();
        gauge!("skim_peers", "layer" => "el").set(peers.len() as f64);
        for (kind, n) in skim_el::requests::counts() {
            counter!("skim_peer_requests_total", "kind" => kind).absolute(n);
        }
        gauge!("skim_snap_peers").set(peers.iter().filter(|(_, p)| p.snap1 || p.snap2).count() as f64);
        for c in crate::dashboard::by_kind(peers.iter().map(|(_, p)| p.client.as_str())) {
            gauge!("skim_peer_clients", "layer" => "el", "client" => c["client"].as_str().unwrap_or("").to_string()).set(c["count"].as_f64().unwrap_or(0.0));
        }
        let fetch = sim.sched.peer_stats();
        for (id, p) in &peers {
            let Some(f) = fetch.get(id) else { continue };
            let labels = [("peer", format!("{:.10}", id.to_string())), ("client", crate::dashboard::client_kind(&p.client))];
            if f.ok > 0 {
                gauge!("skim_snap_peer_latency_ms", &labels).set(f.ewma_ms);
            }
            gauge!("skim_snap_peer_inflight", &labels).set(f.inflight as f64);
            counter!("skim_snap_peer_answers_total", &labels).absolute(f.ok);
            counter!("skim_snap_peer_errors_total", &labels).absolute(f.errors);
        }
        let book = sim.sched.transport().net.peer_book();
        for (client, snap, n) in &book.sessions {
            counter!("skim_el_sessions_total", "client" => client.clone(), "snap" => snap.to_string()).absolute(*n);
        }
        for (reason, n) in &book.closes {
            counter!("skim_el_session_closes_total", "reason" => reason.clone()).absolute(*n);
        }
        if let Some(t) = book.first_snap {
            gauge!("skim_el_first_snap_peer_seconds").set(t.as_secs_f64());
        }
        gauge!("skim_el_snap_peers_seen").set(book.snap_peers_seen as f64);
        gauge!("skim_el_snap_peers_held").set(book.snap_peers_held as f64);
        let st = sim.sched.stats();
        for (kind, v) in [
            ("jobs", st.jobs), ("requests", st.requests), ("ok", st.ok), ("errors", st.errors), ("failed", st.failed),
            ("hedges", st.hedges), ("root_unavailable", st.root_unavailable), ("disconnects", st.disconnects), ("dedup_hits", st.dedup_hits),
        ] {
            counter!("skim_snap_fetch_total", "kind" => kind).absolute(v);
        }
        if let Some(s) = &self.cl_stats {
            let s = s.lock().unwrap();
            gauge!("skim_peers", "layer" => "cl").set(s.peer_list.len() as f64);
            for c in crate::dashboard::by_kind(s.peer_list.iter().map(|p| p.agent.as_deref().unwrap_or(""))) {
                gauge!("skim_peer_clients", "layer" => "cl", "client" => c["client"].as_str().unwrap_or("").to_string()).set(c["count"].as_f64().unwrap_or(0.0));
            }
            for (kind, v) in [("envelopes_by_range", s.envelope_range_reqs), ("envelopes_by_root", s.envelope_root_reqs)] {
                counter!("skim_served_requests_total", "layer" => "cl", "kind" => kind).absolute(v);
            }
            counter!("skim_served_items_total", "layer" => "cl", "kind" => "envelopes").absolute(s.envelopes_served);
        }
        let c = sim.sched.transport().net.serve_counts();
        for (kind, reqs, items) in [
            ("headers", c.header_reqs, c.headers), ("bodies", c.body_reqs, c.bodies), ("receipts", c.receipt_reqs, c.receipts),
            ("bals", c.bal_reqs, c.bals), ("snap", c.snap_reqs, c.codes),
        ] {
            counter!("skim_served_requests_total", "layer" => "el", "kind" => kind).absolute(reqs);
            counter!("skim_served_items_total", "layer" => "el", "kind" => kind).absolute(items);
        }
        let z = sim.store.sizes();
        gauge!("skim_store_keys", "kind" => "subscribed").set(z.watched_keys as f64);
        gauge!("skim_store_keys", "kind" => "on_demand").set(z.temp_keys as f64);
        gauge!("skim_store_keys", "kind" => "versions").set(z.versions as f64);
        gauge!("skim_store_bytes", "kind" => "code").set(z.code_bytes as f64);
        gauge!("skim_store_bytes", "kind" => "recent_bals").set(z.recent_bal_bytes as f64);
        counter!("skim_store_admissions_refused_total").absolute(z.admissions_refused);
        counter!("skim_store_on_demand_evicted_total").absolute(z.on_demand_evicted);
        counter!("skim_store_resolved_from_bal_total").absolute(z.resolved_from_bal);
        counter!("skim_store_archive_base_writes_total").absolute(z.archive_base_writes);
        if let Some(subs) = &self.subs {
            for s in subs.status() {
                gauge!("skim_subscription_ready", "name" => s.name.clone()).set(s.ready as u8 as f64);
                gauge!("skim_subscription_missing_keys", "name" => s.name.clone()).set(s.missing_keys as f64);
            }
        }
        if top_addresses > 0 {
            let labels = self.subs.as_ref().map(|s| s.labels()).unwrap_or_default();
            for a in self.dash.addresses_json(&sim.store, &labels).into_iter().take(top_addresses) {
                // Only `address` labels the counters, so a change of subscription
                // never splits an address into two series.
                let addr = a["address"].as_str().unwrap_or("").to_string();
                counter!("skim_address_calls_total", "address" => addr.clone()).absolute(a["calls"].as_u64().unwrap_or(0));
                counter!("skim_address_fetched_keys_total", "address" => addr.clone()).absolute(a["fetched"].as_u64().unwrap_or(0));
                let subscription = match a["status"].as_str() {
                    Some("subscribed") => 2.0,
                    Some("partly subscribed") => 1.0,
                    _ => 0.0,
                };
                gauge!("skim_address_subscription", "address" => addr.clone()).set(subscription);
                gauge!("skim_address_info", "address" => addr.clone(), "subscription" => a["label"].as_str().unwrap_or("").to_string()).set(1.0);
                let slots = a["slots"].as_u64().unwrap_or(0);
                let unsubscribed = a["unsubscribed_slots"].as_u64().unwrap_or(0);
                gauge!("skim_address_slots", "address" => addr.clone(), "subscribed" => "true").set((slots - unsubscribed) as f64);
                gauge!("skim_address_slots", "address" => addr, "subscribed" => "false").set(unsubscribed as f64);
            }
        }
    }

    /// Retained data by structure (entries, estimated payload bytes, capacity): block caches,
    /// state, the call cache, peers and network bookkeeping. Explains part of RSS (payload, not
    /// allocator overhead or code pages); what it doesn't cover needs a heap profile.
    pub fn memory_parts(&self) -> Vec<crate::chaindata::MemPart> {
        use crate::chaindata::MemPart;
        let mut out = self.chain.as_ref().map(|c| c.memory_parts()).unwrap_or_default();
        out.push(self.call_cache.lock().part());
        let z = self.sim.store.sizes();
        out.push(MemPart { part: "state.recent_bal_writes", entries: z.recent_bals, bytes: z.recent_bal_bytes, capacity: z.recent_bals });
        out.push(MemPart { part: "state.versions", entries: z.versions, bytes: z.versions * 40 + z.keys * 96, capacity: z.keys });
        out.push(MemPart { part: "state.code", entries: z.code, bytes: z.code_bytes, capacity: z.code });
        let (addrs, methods) = self.dash.sizes();
        out.push(MemPart { part: "dashboard.addresses", entries: addrs, bytes: addrs * 160, capacity: addrs });
        out.push(MemPart { part: "dashboard.methods", entries: methods, bytes: methods * 1100, capacity: methods });
        let net = &self.sim.sched.transport().net;
        let peers = net.peers().len();
        out.push(MemPart { part: "el.peers", entries: peers, bytes: 0, capacity: peers });
        out.push(MemPart { part: "el.peer_book", entries: net.peer_book().records, bytes: net.peer_book().records * 96, capacity: skim_el::book::MAX_RECORDS });
        let (q, ql) = self.sim.sched.queue_lens();
        out.push(MemPart { part: "snap.queued_jobs", entries: q + ql, bytes: (q + ql) * 200, capacity: q + ql });
        if let Some(s) = &self.cl_stats {
            let s = s.lock().unwrap();
            out.push(MemPart { part: "cl.envelopes_served", entries: s.envelope_cache.0, bytes: s.envelope_cache.1, capacity: skim_cl::serve::SERVE_SLOTS });
            out.push(MemPart { part: "cl.peers", entries: s.connected, bytes: 0, capacity: s.connected });
            out.push(MemPart { part: "cl.seen_nodes", entries: s.seen_nodes, bytes: s.seen_nodes * 48, capacity: s.seen_nodes });
            out.push(MemPart { part: "cl.dial_records", entries: s.dial_records, bytes: s.dial_records * 56, capacity: s.dial_records });
        }
        out
    }

    /// `skim_dashboard`: peers, latency, RPC and address usage in one document.
    fn dashboard_json(&self) -> Value {
        use crate::dashboard::{by_kind, client_kind, round2};
        let sim = &self.sim;
        let head = sim.store.head();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let peers = sim.sched.transport().net.peers();
        let fetch = sim.sched.peer_stats();
        let mut el: Vec<Value> = peers
            .iter()
            .map(|(id, p)| {
                let f = fetch.get(id);
                json!({
                    "id": format!("{:.10}", id.to_string()),
                    "client": client_kind(&p.client),
                    "version": p.client.split('/').nth(1).unwrap_or(""),
                    "eth": format!("eth/{}", p.eth_version),
                    "snap": if p.snap2 { "snap/2" } else if p.snap1 { "snap/1" } else { "none" },
                    "age_min": round2(p.connected_at.elapsed().as_secs_f64() / 60.0),
                    "snap_ok": f.map_or(0, |f| f.ok),
                    "snap_errors": f.map_or(0, |f| f.errors),
                    "ewma_ms": f.map_or(Value::Null, |f| json!(round2(f.ewma_ms))),
                    "min_ms": f.map_or(Value::Null, |f| json!(round2(f.min_ms))),
                    "inflight": f.map_or(0, |f| f.inflight),
                })
            })
            .collect();
        el.sort_by(|a, b| (a["snap"] == "none").cmp(&(b["snap"] == "none")).then(b["snap_ok"].as_u64().cmp(&a["snap_ok"].as_u64())));
        let (cl, cl_mesh) = match &self.cl_stats {
            Some(s) => {
                let s = s.lock().unwrap();
                let mut v: Vec<Value> = s
                    .peer_list
                    .iter()
                    .map(|p| {
                        let agent = p.agent.clone().unwrap_or_default();
                        json!({
                            "id": format!("{:.12}", p.id),
                            "client": client_kind(&agent),
                            "version": agent.split('/').nth(1).unwrap_or(""),
                            "direction": if p.outbound { "outbound" } else { "inbound" },
                            "age_min": round2(p.age_s / 60.0),
                            "gossip_first": p.gossip_first,
                            "head_slot": p.head_slot,
                        })
                    })
                    .collect();
                v.sort_by(|a, b| b["gossip_first"].as_u64().cmp(&a["gossip_first"].as_u64()));
                (v, json!(s.mesh))
            }
            None => (vec![], json!({})),
        };
        let blocks = self.dash.blocks();
        let mut arrivals: Vec<f64> = blocks.iter().map(|b| b.arrival_ms).filter(|x| x.is_finite()).collect();
        arrivals.sort_by(f64::total_cmp);
        let q = |p: f64| arrivals.get(((arrivals.len() as f64 - 1.0) * p).round() as usize).copied().map(round2);
        let labels = self.subs.as_ref().map(|s| s.labels()).unwrap_or_default();
        let addresses = self.dash.addresses_json(&sim.store, &labels);
        let unsubscribed = addresses.iter().filter(|a| a["status"] != "subscribed").count();
        let z = sim.store.sizes();
        let el_kinds: Vec<&str> = peers.iter().map(|(_, p)| p.client.as_str()).collect();
        json!({
            "updated": now,
            "summary": {
                "head": head.map(|h| h.number),
                "head_age_s": head.map(|h| round2(now - h.timestamp as f64)),
                "uptime_min": self.dash.uptime_s() / 60,
                "el_peers": peers.len(),
                "snap_peers": peers.iter().filter(|(_, p)| p.snap1 || p.snap2).count(),
                "cl_peers": cl.len(),
                "subscribed_keys": z.watched_keys,
                "on_demand_keys": z.temp_keys,
                "addresses_used": addresses.len(),
                "addresses_not_subscribed": unsubscribed,
                "arrival_p50_ms": q(0.5),
                "arrival_p90_ms": q(0.9),
            },
            "el": { "peers": el, "clients": by_kind(el_kinds.into_iter()) },
            "cl": { "peers": cl.clone(), "clients": by_kind(cl.iter().map(|p| p["client"].as_str().unwrap_or(""))), "mesh": cl_mesh },
            "blocks": {
                "numbers": blocks.iter().map(|b| b.number).collect::<Vec<_>>(),
                "arrival_ms": blocks.iter().map(|b| if b.arrival_ms.is_finite() { json!(round2(b.arrival_ms)) } else { Value::Null }).collect::<Vec<_>>(),
                "changed": blocks.iter().map(|b| b.changed).collect::<Vec<_>>(),
            },
            "rpc": self.dash.methods_json(),
            "addresses": addresses,
            "suggestion": self.dash.suggestion(&sim.store),
            "served": self.served_json(),
        })
    }

    /// Requests skim answered for other peers.
    fn served_json(&self) -> Value {
        let c = self.sim.sched.transport().net.serve_counts();
        json!({
            "el_enode": self.sim.sched.transport().net.enode(),
            "cl_peer_id": self.cl_stats.as_ref().map(|s| s.lock().unwrap().peer_id.clone()),
            "el": {
                "headers": {"requests": c.header_reqs, "items": c.headers},
                "bodies": {"requests": c.body_reqs, "items": c.bodies},
                "receipts": {"requests": c.receipt_reqs, "items": c.receipts},
                "bals": {"requests": c.bal_reqs, "items": c.bals},
                "snap": {"requests": c.snap_reqs, "bytecodes": c.codes},
                "other_requests": c.other_reqs,
                "served_from": self.chain.as_ref().and_then(|c| c.served_from()),
            },
            "cl": self.cl_stats.as_ref().map(|s| {
                let s = s.lock().unwrap();
                json!({
                    "envelopes_by_range": {"requests": s.envelope_range_reqs},
                    "envelopes_by_root": {"requests": s.envelope_root_reqs},
                    "envelopes": s.envelopes_served,
                    "served_from_slot": s.envelopes_from_slot,
                })
            }),
        })
    }

    /// The block `latest` means for this request (the head, or the verified head).
    fn latest(&self) -> Option<BlockRef> {
        if self.require_verified {
            self.light.as_ref().and_then(|l| l.status().verified_block).and_then(|v| crate::light::resolve(&self.sim.store, &v))
        } else {
            self.sim.store.head()
        }
    }
}

async fn handle(State(rpc): State<Arc<Rpc>>, axum::Extension(policy): axum::Extension<Arc<OriginPolicy>>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Json<Value> {
    let caller = policy.caller(&headers);
    // The epoch before `latest`: a reorg between the two shows up as a changed epoch.
    let epoch = rpc.reorg_epoch();
    let latest = rpc.latest();
    match body {
        Value::Array(reqs) if reqs.len() > MAX_BATCH => Json(batch_too_large()),
        Value::Array(reqs) => {
            let mut out = Vec::with_capacity(reqs.len());
            out.extend(rpc.batch(reqs.iter().collect(), &latest, epoch, caller.clone()).await);
            Json(Value::Array(out))
        }
        req => Json(rpc.one(&req, &latest, epoch, caller).await),
    }
}

/// Who is calling, for what a browser page may see. `skim_*` methods expose what this node is
/// used for (subscription names and addresses, the addresses calls read, peer ids): a page
/// gets them only from an origin `--cors` names exactly, never through `*`. Everything else
/// follows `--cors` as before. A request without an `Origin` header (curl, a server, viem in
/// Node) is not a page. (Review R6: with the default `--cors *`, any web page the user opened
/// could read `skim_status` and `skim_dashboard` from a local node.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// No `Origin`, or an origin `--cors` names exactly.
    Trusted,
    /// A browser page from another origin; `skim_*` methods are refused.
    Page(Arc<str>),
}

fn page_refused(method: &str, origin: &str) -> Value {
    json!({"code": -32004, "message": format!("{method} is not available to web pages ({origin}): skim_* methods reveal what this node watches; allow this origin with --cors {origin}")})
}

/// `--cors` as a policy: the origin browser pages may call from (`*` for any), or none.
#[derive(Debug, Clone, Default)]
pub struct OriginPolicy {
    pub cors: Option<String>,
}

impl OriginPolicy {
    pub fn caller(&self, headers: &axum::http::HeaderMap) -> Caller {
        match headers.get(axum::http::header::ORIGIN).and_then(|o| o.to_str().ok()) {
            None => Caller::Trusted,
            Some(o) if self.cors.as_deref() == Some(o) => Caller::Trusted,
            Some(o) => Caller::Page(Arc::from(o)),
        }
    }

    /// Whether a page from `origin` may open a WebSocket: browsers don't apply CORS to
    /// WebSocket upgrades, so the server checks the `Origin` itself, against `--cors`.
    pub fn websocket_allowed(&self, headers: &axum::http::HeaderMap) -> bool {
        match headers.get(axum::http::header::ORIGIN).and_then(|o| o.to_str().ok()) {
            None => true,
            Some(o) => matches!(self.cors.as_deref(), Some("*")) || self.cors.as_deref() == Some(o),
        }
    }
}

/// Requests per batch (HTTP or WebSocket), as geth's default. (Review: unlimited; only the 2 MB
/// body limit applied, ~30k requests.)
const MAX_BATCH: usize = 1000;
/// Subscriptions per WebSocket connection. (Review: unlimited; every block ran a log scan for
/// each `logs` subscription.)
const MAX_WS_SUBSCRIPTIONS: usize = 128;

fn batch_too_large() -> Value {
    json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32600, "message": format!("batch too large: at most {MAX_BATCH} requests")}})
}

/// `cors`: the `Access-Control-Allow-Origin` to send (browser dapps calling skim directly), or
/// `None` to send none.
pub async fn serve(rpc: Arc<Rpc>, listener: tokio::net::TcpListener, cors: Option<String>) -> anyhow::Result<()> {
    use axum::http::{HeaderValue, header};
    // HTTP POST and WebSocket on the same address; OPTIONS answers CORS preflights.
    let policy = Arc::new(OriginPolicy { cors: cors.clone() });
    let mut app = Router::new()
        .route("/", post(handle).get(ws_upgrade).options(|| async { axum::http::StatusCode::NO_CONTENT }))
        .with_state(rpc)
        .layer(axum::Extension(policy));
    if let Some(origin) = cors {
        let origin = HeaderValue::from_str(&origin)?;
        app = app.layer(axum::middleware::map_response(move |mut r: axum::response::Response| {
            let origin = origin.clone();
            async move {
                let h = r.headers_mut();
                h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
                h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("POST, GET, OPTIONS"));
                h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("content-type"));
                h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("86400"));
                r
            }
        }));
    }
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "JSON-RPC listening (HTTP and WebSocket)");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn ws_upgrade(
    State(rpc): State<Arc<Rpc>>,
    axum::Extension(policy): axum::Extension<Arc<OriginPolicy>>,
    headers: axum::http::HeaderMap,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !policy.websocket_allowed(&headers) {
        return (axum::http::StatusCode::FORBIDDEN, "origin not allowed by --cors").into_response();
    }
    let caller = policy.caller(&headers);
    ws.on_upgrade(move |socket| ws_session(rpc, socket, caller))
}

/// One WebSocket connection: ordinary JSON-RPC requests (single or batch), plus
/// `eth_subscribe` / `eth_unsubscribe` for:
/// - `newHeads`: each block whose state is ready
/// - `logs` (with a filter): that block's matching logs
/// - `skim_subscriptions`: readiness and the latest `on_block` results after each block
async fn ws_session(rpc: Arc<Rpc>, socket: axum::extract::ws::WebSocket, caller: Caller) {
    use axum::extract::ws::Message;
    use futures::{SinkExt, StreamExt};
    let (mut tx, mut rx) = socket.split();
    let mut events = rpc.events.subscribe();
    let mut subs: std::collections::HashMap<String, Sub> = Default::default();
    let mut next_id = 1u64;
    loop {
        tokio::select! {
            msg = rx.next() => {
                let Some(Ok(msg)) = msg else { break };
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                let Ok(body) = serde_json::from_str::<Value>(&text) else {
                    let _ = tx.send(Message::Text(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}}).to_string().into())).await;
                    continue;
                };
                let epoch = rpc.reorg_epoch();
                let latest = rpc.latest();
                let mut one = |req: &Value| -> Option<Value> {
                    let method = req.get("method").and_then(Value::as_str)?;
                    let id = req.get("id").cloned().unwrap_or(Value::Null);
                    let params = req.get("params").cloned().unwrap_or(json!([]));
                    match method {
                        "eth_subscribe" => {
                            let kind = params.get(0).and_then(Value::as_str).unwrap_or("");
                            if subs.len() >= MAX_WS_SUBSCRIPTIONS {
                                return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32005, "message": format!("too many subscriptions on this connection (at most {MAX_WS_SUBSCRIPTIONS})")}}));
                            }
                            let sub = match kind {
                                "newHeads" => Sub::NewHeads { track: crate::ws_feed::HeadTrack::seeded(latest.map(|b| (b.number, b.hash))) },
                                "skim_subscriptions" => match &caller {
                                    Caller::Page(origin) => return Some(json!({"jsonrpc": "2.0", "id": id, "error": page_refused("skim_subscriptions", &origin)})),
                                    Caller::Trusted => Sub::Skim,
                                },
                                "logs" => match serde_json::from_value::<alloy_rpc_types_eth::Filter>(params.get(1).cloned().unwrap_or(json!({}))) {
                                    // Logs from blocks after the one current now (or replacing it).
                                    Ok(f) => Sub::Logs {
                                        filter: Box::new(f),
                                        track: crate::ws_feed::HeadTrack::seeded(latest.map(|b| (b.number, b.hash))),
                                        delivered: Default::default(),
                                        failures: 0,
                                    },
                                    Err(e) => return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": format!("logs filter: {e}")}})),
                                },
                                other => return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": format!("unsupported subscription {other:?} (newHeads, logs, skim_subscriptions)")}})),
                            };
                            let sid = format!("{:#x}", next_id);
                            next_id += 1;
                            subs.insert(sid.clone(), sub);
                            Some(json!({"jsonrpc": "2.0", "id": id, "result": sid}))
                        }
                        "eth_unsubscribe" => {
                            let sid = params.get(0).and_then(Value::as_str).unwrap_or("");
                            Some(json!({"jsonrpc": "2.0", "id": id, "result": subs.remove(sid).is_some()}))
                        }
                        _ => None, // handled below (async)
                    }
                };
                let reply = match &body {
                    Value::Array(reqs) if reqs.len() > MAX_BATCH => batch_too_large(),
                    Value::Array(reqs) => {
                        // Subscription changes first (in order, they touch this session), then the
                        // other requests concurrently.
                        let mut out: Vec<Option<Value>> = reqs.iter().map(&mut one).collect();
                        let rest: Vec<&Value> = reqs.iter().zip(&out).filter(|(_, o)| o.is_none()).map(|(r, _)| r).collect();
                        let mut answered = rpc.batch(rest, &latest, epoch, caller.clone()).await.into_iter();
                        for o in out.iter_mut().filter(|o| o.is_none()) {
                            *o = answered.next();
                        }
                        Value::Array(out.into_iter().map(|o| o.unwrap_or(Value::Null)).collect())
                    }
                    req => match one(req) {
                        Some(v) => v,
                        None => rpc.one(req, &latest, epoch, caller.clone()).await,
                    },
                };
                if tx.send(Message::Text(reply.to_string().into())).await.is_err() {
                    break;
                }
            }
            ev = events.recv() => {
                let ev = match ev {
                    Ok(ev) => ev,
                    // Missed heads are caught up on the next one (`HeadTrack` reports every block
                    // since the last it reported); a missed subscriptions push is superseded.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                };
                let sent = match ev {
                    WsEvent::NewHead(b) => {
                        // With `--require-verified`, pushes follow the verified head too, like every
                        // read. (Review: they followed optimistic heads.)
                        let head = if rpc.require_verified { rpc.latest() } else { Some(b) };
                        match head {
                            Some(head) => push_heads(&rpc, &mut subs, (head.number, head.hash), &mut tx).await,
                            None => true,
                        }
                    }
                    WsEvent::Subscriptions(n) => match rpc.subs.as_ref() {
                        Some(s) => {
                            let result = json!({"block": format!("{n:#x}"), "subscriptions": s.status()});
                            let mut ok = true;
                            for (sid, _) in subs.iter().filter(|(_, s)| matches!(s, Sub::Skim)) {
                                ok &= send_push(&mut tx, sid, result.clone()).await;
                            }
                            ok
                        }
                        None => true,
                    },
                };
                if !sent {
                    return;
                }
            }
        }
    }
}

type WsSink = futures::stream::SplitSink<axum::extract::ws::WebSocket, axum::extract::ws::Message>;

async fn send_push(tx: &mut WsSink, sid: &str, result: Value) -> bool {
    use futures::SinkExt;
    let msg = json!({"jsonrpc": "2.0", "method": "eth_subscription", "params": {"subscription": sid, "result": result}});
    tx.send(axum::extract::ws::Message::Text(msg.to_string().into())).await.is_ok()
}

/// Blocks in a row a `logs` subscription may fail to deliver (receipts not fetched) before it
/// ends with an error, rather than stall or skip silently.
const MAX_LOG_DELIVERY_FAILURES: u32 = 8;

/// Moves every head-following subscription to `head`: `newHeads` gets each added block's header;
/// `logs` gets the logs it reported for removed blocks again with `removed: true` (newest
/// first), then the logs of each added block. A subscription's position advances per block
/// delivered. A `logs` subscription that can't be told everything (a gap too long to report, or
/// receipts that keep failing) ends with an error notification instead of skipping silently.
/// False once the connection is gone.
async fn push_heads(rpc: &Rpc, subs: &mut std::collections::HashMap<String, Sub>, head: (u64, B256), tx: &mut WsSink) -> bool {
    let canon = |n: u64| rpc.sim.store.block(n).map(|x| x.hash);
    let mut ended = Vec::new();
    for (sid, sub) in subs.iter_mut() {
        match sub {
            Sub::NewHeads { track } => {
                // Heads more than `KEEP` behind are skipped, as geth skips them for a slow reader.
                let step = track.step(head, canon);
                if let Some((_, to)) = step.skipped
                    && let Some(h) = canon(to)
                {
                    track.skip_to(to, h);
                }
                for &(n, hash) in &step.added {
                    let Some(b) = rpc.sim.store.block(n).filter(|b| b.hash == hash) else { break };
                    let Some(h) = rpc.sim.header_arc(&b) else { break };
                    let Ok(v) = serde_json::to_value(alloy_rpc_types_eth::Header::new((*h).clone())) else { break };
                    if !send_push(tx, sid, v).await {
                        return false;
                    }
                    track.mark(n, hash);
                }
            }
            Sub::Logs { filter, track, delivered, failures } => {
                let Some(c) = &rpc.chain else { continue };
                let step = track.step(head, canon);
                for &(n, hash) in &step.removed {
                    let Some((h, logs)) = delivered.remove(&n) else { continue };
                    if h != hash {
                        continue;
                    }
                    for mut log in logs.into_iter().rev() {
                        log["removed"] = json!(true);
                        if !send_push(tx, sid, log).await {
                            return false;
                        }
                    }
                }
                if let Some((from, to)) = step.skipped {
                    ended.push((sid.clone(), format!("logs of blocks {from}..={to} can't be delivered (more than {} blocks behind); subscribe again", crate::ws_feed::KEEP)));
                    continue;
                }
                for &(n, hash) in &step.added {
                    match c.logs(filter, n, n, Some(hash)).await {
                        Ok(Value::Array(logs)) => {
                            // Logs: one notification per log, as clients expect.
                            for log in &logs {
                                if !send_push(tx, sid, log.clone()).await {
                                    return false;
                                }
                            }
                            delivered.insert(n, (hash, logs));
                            track.mark(n, hash);
                            *failures = 0;
                        }
                        // Not delivered: the position stays, and the next head tries it again.
                        // (Review: the block was skipped for good.)
                        other => {
                            *failures += 1;
                            #[cfg(test)]
                            c.run_test_hook("ws_delivery_failed");
                            if *failures >= MAX_LOG_DELIVERY_FAILURES {
                                let why = match other {
                                    Err(e) => RpcError::from(e).message,
                                    Ok(_) => "unexpected result".into(),
                                };
                                ended.push((sid.clone(), format!("logs of block {n} couldn't be fetched ({why}); subscribe again")));
                            }
                            break;
                        }
                    }
                }
                while delivered.len() as u64 > crate::ws_feed::KEEP {
                    delivered.pop_first();
                }
            }
            Sub::Skim => {}
        }
    }
    for (sid, why) in ended {
        subs.remove(&sid);
        let msg = json!({"jsonrpc": "2.0", "method": "eth_subscription", "params": {"subscription": sid, "error": {"code": -32002, "message": why}}});
        use futures::SinkExt;
        if tx.send(axum::extract::ws::Message::Text(msg.to_string().into())).await.is_err() {
            return false;
        }
    }
    true
}

enum Sub {
    NewHeads { track: crate::ws_feed::HeadTrack },
    /// `track`: the blocks delivered, from the head when it subscribed; `delivered`: the logs
    /// pushed per block, to withdraw if the block leaves the chain; `failures`: deliveries failed
    /// in a row.
    Logs { filter: Box<alloy_rpc_types_eth::Filter>, track: crate::ws_feed::HeadTrack, delivered: std::collections::BTreeMap<u64, (B256, Vec<Value>)>, failures: u32 },
    Skim,
}

#[cfg(test)]
mod tests {
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
}
