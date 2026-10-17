//! Block streams (`skim_streamNext`): an app module follows the chain through skim instead of
//! re-implementing catch-up, retries and reorg handling over `eth_getLogs`.
//!
//! Pull-based and stateless in skim. The module keeps its position (`cursor`, the last block
//! it applied) in its own store, in the same transaction as the index changes it made, and
//! passes it back with each request:
//! - restarting is resuming (a crash after applying but before saving the cursor only causes a
//!   re-delivery, never a gap);
//! - skim holds no per-module queue, so a slow module can't stall skim or grow memory; it just
//!   falls behind, and is told explicitly (`gap`) if the data it needs is no longer available.
//!
//! Events, in order:
//! - `rewind {to}`: the module's cursor is no longer on the chain; undo everything above `to`
//!   (the newest block of the module's `recent` list that still is).
//! - `gap {from, to}`: blocks skim can't serve logs for any more (before the fork, or expired
//!   on every connected peer); the module decides how to mark them.
//! - `apply {block, logs}`: a block with logs matching the module's filters (blocks without
//!   matches are skipped; the response's `cursor` says how far the scan went).
//! Each response also reports `finalized` (verified by the light client), so a module can hold
//! irreversible exports until then.

use std::time::{Duration, Instant};

use alloy_primitives::B256;
use alloy_rpc_types_eth::Filter;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::chaindata::{ChainData, DataError};

#[derive(Deserialize, Clone, Copy, Debug)]
pub struct BlockId {
    pub number: u64,
    pub hash: B256,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    /// The last block the module applied (`None`: a new module, starting at `from_block`).
    pub cursor: Option<BlockId>,
    /// Blocks the module applied recently, newest first: where to rewind to after a reorg.
    #[serde(default)]
    pub recent: Vec<BlockId>,
    /// For a new module: the first block it wants (default: the head).
    pub from_block: Option<u64>,
    /// Logs matching any of these (address and topics; block fields are ignored).
    pub filters: Vec<Filter>,
    /// Blocks per response (at most `--max-logs-range`).
    pub max_blocks: Option<u64>,
    /// At the head: how long to wait for a new block before answering with no events.
    pub wait_ms: Option<u64>,
}

pub async fn next(chain: &ChainData, head: impl Fn() -> Option<u64>, finalized: Option<Value>, req: Request) -> Result<Value, DataError> {
    if req.filters.is_empty() {
        return Err(DataError::Invalid("filters must not be empty".into()));
    }
    let mut events = Vec::new();
    let Some(mut tip) = head() else { return Err(DataError::Unavailable("not following the chain yet".into())) };

    // Where the module is, and whether that's still on the chain.
    let mut pos: u64 = match req.cursor {
        None => req.from_block.unwrap_or(tip).saturating_sub(1),
        Some(c) if chain.canonical_hash(c.number) == Some(c.hash) => c.number,
        Some(c) => {
            let ancestor = req.recent.iter().filter(|b| b.number < c.number).find(|b| chain.canonical_hash(b.number) == Some(b.hash));
            let Some(a) = ancestor else {
                return Err(DataError::Unavailable(format!(
                    "cursor {} ({}) is not on the chain and none of the {} recent blocks is: send more of them, or reset the module",
                    c.number,
                    c.hash,
                    req.recent.len()
                )));
            };
            events.push(json!({"type": "rewind", "to": {"number": a.number, "hash": a.hash}}));
            a.number
        }
    };

    // Blocks skim can't serve logs for: say so, then continue after them.
    let history = chain.history_json();
    let logs_from = history["logs_from"].as_u64().unwrap_or(0).max(history["fork_floor"].as_u64().unwrap_or(0));
    if pos + 1 < logs_from {
        events.push(json!({"type": "gap", "from": pos + 1, "to": logs_from - 1, "reason": "logs not available (before the fork, or expired on every connected peer)"}));
        pos = logs_from - 1;
    }

    // At the head: wait (briefly) for the next block.
    if pos >= tip {
        let until = Instant::now() + Duration::from_millis(req.wait_ms.unwrap_or(0).min(30_000));
        while pos >= tip && Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(200)).await;
            tip = head().unwrap_or(tip);
        }
    }
    // The new position: the last block scanned, else where the rewind/gap left the module.
    let cursor = if pos < tip {
        let to = tip.min(pos + req.max_blocks.unwrap_or(chain.max_log_range).clamp(1, chain.max_log_range));
        let scan = chain.scan(&req.filters, pos + 1, to, None).await?;
        for b in scan.blocks {
            events.push(json!({
                "type": "apply",
                "block": {"number": b.number, "hash": b.hash, "parentHash": b.parent_hash, "timestamp": b.timestamp},
                "logs": b.logs,
            }));
        }
        pos = scan.last.0;
        Some(json!({"number": scan.last.0, "hash": scan.last.1}))
    } else {
        chain.canonical_hash(pos).map(|h| json!({"number": pos, "hash": h}))
    };
    Ok(json!({
        "events": events,
        // `cursor` is null only where skim has no hash for the position (e.g. the end of a gap
        // before the fork); then the next request starts at `nextFrom`.
        "cursor": cursor,
        "nextFrom": pos + 1,
        "head": tip,
        "finalized": finalized,
        "logsFrom": logs_from,
    }))
}
