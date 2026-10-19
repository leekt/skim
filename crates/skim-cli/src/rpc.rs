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
//! included. `safe`, `finalized` and `earliest` are not tracked yet and answer `-32002`. Within
//! one batch, `latest` is pinned to one block.
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
use skim_state::BlockRef;

use crate::chaindata::{ChainData, DataError};

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
    fn unavailable(m: impl Into<String>) -> Self {
        Self::new(-32002, m)
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
            other => Self::unavailable(other.to_string()),
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
            DataError::Unavailable(m) => Self::unavailable(m),
            DataError::Invalid(m) => Self::new(-32000, m),
            DataError::RangeTooLarge(n) => Self::new(-32005, format!("block range too large: at most {n} blocks per eth_getLogs")),
            DataError::Unsupported(m) => Self::new(-32004, m),
        }
    }
}

impl Rpc {
    fn resolve_block(&self, id: Option<BlockId>, latest: &Option<BlockRef>) -> Result<BlockRef, RpcError> {
        let store = &self.sim.store;
        let latest = || latest.ok_or_else(|| RpcError::unavailable("not following the chain yet"));
        match id.unwrap_or(BlockId::Number(BlockNumberOrTag::Latest)) {
            BlockId::Number(BlockNumberOrTag::Latest) => latest(),
            BlockId::Number(BlockNumberOrTag::Number(n)) => store
                .block(n)
                .or_else(|| store.archived_block(n).map(|(b, _)| b))
                .ok_or_else(|| RpcError::unavailable(format!("block {n} is outside the retained window and not archived"))),
            // Pending state = latest state (skim keeps no mempool); see eth_getTransactionCount.
            BlockId::Number(BlockNumberOrTag::Pending) => latest(),
            BlockId::Number(t @ (BlockNumberOrTag::Safe | BlockNumberOrTag::Finalized)) => {
                // `safe`: signed by a sync-committee supermajority; `finalized`: finalized
                // beacon block's execution block. Both from the light client.
                let st = self.light.as_ref().map(|l| l.status()).unwrap_or_default();
                let v = if t == BlockNumberOrTag::Safe { st.verified_block } else { st.finalized_block };
                let v = v.ok_or_else(|| RpcError::unavailable(format!("no {t} block verified yet")))?;
                // The verified hash, at its height: never whichever block holds that height now.
                crate::light::resolve(store, &v).ok_or_else(|| {
                    RpcError::unavailable(format!("{t} block {} ({}) is not on the selected chain or outside the retained window", v.number, v.hash))
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
                    .ok_or_else(|| RpcError::unavailable(format!("block {} is not in the window or the archive", h.block_hash)))
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
        let block = store.block(n).filter(|b| b.hash == bh).ok_or_else(|| RpcError::unavailable(format!("block {n} is outside the retained window")))?;
        let parent = store.block(n - 1).ok_or_else(|| RpcError::unavailable(format!("parent of block {n} is outside the retained window")))?;
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
        if let Some(overrides) = param::<alloy_rpc_types_eth::state::StateOverride>(params, 2)? {
            // State overrides: the call runs on an overlay; the store is never modified.
            let to: Reads = req.to.and_then(|k| k.to().copied()).into_iter().collect();
            let blocks = [skim_sim::multi::SimBlock { block: None, state: Some(overrides), calls: vec![self.tx_env(req, &header)?.into()] }];
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
        let gas = req.gas.unwrap_or(header.gas_limit).min(header.gas_limit.max(30_000_000));
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
                let key = (b.hash, alloy_primitives::keccak256(format!("{}|{}", params.get(0).unwrap_or(&Value::Null), params.get(2).unwrap_or(&Value::Null))));
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
                let b = self.resolve_block(param(params, 1)?, latest)?;
                let header = self.header(&b)?;
                let cap = req.gas.unwrap_or(header.gas_limit);
                let mut tx = self.tx_env(req, &header)?;
                tx.gas_limit = cap;
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
                    _ => {
                        let (a, b) = ChainData::range(&filter, head)?;
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
                Err(RpcError::unavailable(format!("no peer served a proof for block {} (peers keep only recent state roots)", b.number)))
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
                let (n, bh, _, i) = chain.locate_tx(&hash).await?.ok_or_else(|| RpcError::unavailable("transaction not found in recent blocks"))?;
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
                        "note": "entries, estimated payload bytes and capacity per retained structure; payload only (no allocator overhead or code pages), so the sum explains part of RSS",
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
                let store = sim.store.clone();
                Ok(crate::stream::next(chain, move || store.head().map(|h| h.number), self.events.subscribe(), finalized, req).await?)
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
    async fn batch<'a>(&'a self, reqs: Vec<&'a Value>, latest: &'a Option<BlockRef>) -> Vec<Value> {
        use futures::StreamExt;
        let calls: Vec<_> = reqs.into_iter().map(|r| self.one(r, latest)).collect();
        futures::stream::iter(calls).buffered(BATCH_CONCURRENCY).collect().await
    }

    async fn one(&self, req: &Value, latest: &Option<BlockRef>) -> Value {
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let Some(method) = req.get("method").and_then(Value::as_str) else {
            return json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32600, "message": "invalid request"}});
        };
        let params = req.get("params").cloned().unwrap_or(json!([]));
        let t = std::time::Instant::now();
        let res = self.dispatch(method, &params, latest).await;
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
        if let Some(subs) = &self.subs {
            for s in subs.status() {
                gauge!("skim_subscription_ready", "name" => s.name.clone()).set(s.ready as u8 as f64);
                gauge!("skim_subscription_missing_keys", "name" => s.name.clone()).set(s.missing_keys as f64);
            }
        }
        if top_addresses > 0 {
            let labels = self.subs.as_ref().map(|s| s.labels()).unwrap_or_default();
            for a in self.dash.addresses_json(&sim.store, &labels).into_iter().take(top_addresses) {
                // Only `address` labels the counters, so a change of pinning or subscription
                // never splits an address into two series.
                let addr = a["address"].as_str().unwrap_or("").to_string();
                counter!("skim_address_calls_total", "address" => addr.clone()).absolute(a["calls"].as_u64().unwrap_or(0));
                counter!("skim_address_fetched_keys_total", "address" => addr.clone()).absolute(a["fetched"].as_u64().unwrap_or(0));
                let pinning = match a["status"].as_str() {
                    Some("pinned") => 2.0,
                    Some("partly pinned") => 1.0,
                    _ => 0.0,
                };
                gauge!("skim_address_pinning", "address" => addr.clone()).set(pinning);
                gauge!("skim_address_info", "address" => addr.clone(), "subscription" => a["label"].as_str().unwrap_or("").to_string()).set(1.0);
                let slots = a["slots"].as_u64().unwrap_or(0);
                let unpinned = a["unpinned_slots"].as_u64().unwrap_or(0);
                gauge!("skim_address_slots", "address" => addr.clone(), "pinned" => "true").set((slots - unpinned) as f64);
                gauge!("skim_address_slots", "address" => addr, "pinned" => "false").set(unpinned as f64);
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
        let unpinned = addresses.iter().filter(|a| a["status"] != "pinned").count();
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
                "addresses_not_pinned": unpinned,
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

async fn handle(State(rpc): State<Arc<Rpc>>, Json(body): Json<Value>) -> Json<Value> {
    let latest = rpc.latest();
    match body {
        Value::Array(reqs) => {
            let mut out = Vec::with_capacity(reqs.len());
            out.extend(rpc.batch(reqs.iter().collect(), &latest).await);
            Json(Value::Array(out))
        }
        req => Json(rpc.one(&req, &latest).await),
    }
}

pub async fn serve(rpc: Arc<Rpc>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    // HTTP POST and WebSocket on the same address.
    let app = Router::new()
        .route("/", post(handle).get(ws_upgrade))
        .with_state(rpc);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "JSON-RPC listening (HTTP and WebSocket)");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn ws_upgrade(State(rpc): State<Arc<Rpc>>, ws: axum::extract::ws::WebSocketUpgrade) -> axum::response::Response {
    ws.on_upgrade(move |socket| ws_session(rpc, socket))
}

/// One WebSocket connection: ordinary JSON-RPC requests (single or batch), plus
/// `eth_subscribe` / `eth_unsubscribe` for:
/// - `newHeads`: each block whose state is ready
/// - `logs` (with a filter): that block's matching logs
/// - `skim_subscriptions`: readiness and the latest `on_block` results after each block
async fn ws_session(rpc: Arc<Rpc>, socket: axum::extract::ws::WebSocket) {
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
                let latest = rpc.latest();
                let mut one = |req: &Value| -> Option<Value> {
                    let method = req.get("method").and_then(Value::as_str)?;
                    let id = req.get("id").cloned().unwrap_or(Value::Null);
                    let params = req.get("params").cloned().unwrap_or(json!([]));
                    match method {
                        "eth_subscribe" => {
                            let kind = params.get(0).and_then(Value::as_str).unwrap_or("");
                            let sub = match kind {
                                "newHeads" => Sub::NewHeads,
                                "skim_subscriptions" => Sub::Skim,
                                "logs" => match serde_json::from_value::<alloy_rpc_types_eth::Filter>(params.get(1).cloned().unwrap_or(json!({}))) {
                                    Ok(f) => Sub::Logs(Box::new(f)),
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
                    Value::Array(reqs) => {
                        // Subscription changes first (in order, they touch this session), then the
                        // other requests concurrently.
                        let mut out: Vec<Option<Value>> = reqs.iter().map(&mut one).collect();
                        let rest: Vec<&Value> = reqs.iter().zip(&out).filter(|(_, o)| o.is_none()).map(|(r, _)| r).collect();
                        let mut answered = rpc.batch(rest, &latest).await.into_iter();
                        for o in out.iter_mut().filter(|o| o.is_none()) {
                            *o = answered.next();
                        }
                        Value::Array(out.into_iter().map(|o| o.unwrap_or(Value::Null)).collect())
                    }
                    req => match one(req) {
                        Some(v) => v,
                        None => rpc.one(req, &latest).await,
                    },
                };
                if tx.send(Message::Text(reply.to_string().into())).await.is_err() {
                    break;
                }
            }
            ev = events.recv() => {
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                };
                for (sid, sub) in &subs {
                    // Logs: one notification per log, as clients expect.
                    if let (Sub::Logs(f), WsEvent::NewHead(b), Some(c)) = (sub, &ev, &rpc.chain) {
                        if let Ok(Value::Array(logs)) = c.logs(f, b.number, b.number, Some(b.hash)).await {
                            for log in logs {
                                let msg = json!({"jsonrpc": "2.0", "method": "eth_subscription", "params": {"subscription": sid, "result": log}});
                                if tx.send(Message::Text(msg.to_string().into())).await.is_err() {
                                    return;
                                }
                            }
                        }
                        continue;
                    }
                    let result = match (sub, &ev) {
                        (Sub::NewHeads, WsEvent::NewHead(b)) => match rpc.sim.header_arc(b) {
                            Some(h) => serde_json::to_value(alloy_rpc_types_eth::Header::new((*h).clone())).ok(),
                            None => None,
                        },
                        (Sub::Skim, WsEvent::Subscriptions(n)) => rpc
                            .subs
                            .as_ref()
                            .map(|s| json!({"block": format!("{n:#x}"), "subscriptions": s.status()})),
                        _ => None,
                    };
                    if let Some(result) = result {
                        let msg = json!({"jsonrpc": "2.0", "method": "eth_subscription", "params": {"subscription": sid, "result": result}});
                        if tx.send(Message::Text(msg.to_string().into())).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

enum Sub {
    NewHeads,
    Logs(Box<alloy_rpc_types_eth::Filter>),
    Skim,
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
