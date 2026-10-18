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
//! - `apply {block, logs, stateChanges?}`: a block with logs matching the module's filters, or
//!   (with `bal`) changes to its accounts from the block's access list, each with its
//!   `block_access_index` (blocks with neither are skipped; `cursor` says how far the scan
//!   went).
//! Each response also reports `finalized` (verified by the light client), so a module can hold
//! irreversible exports until then.
//!
//! At the head, a request with `waitMs` returns as soon as skim applies the next block (it's
//! woken by the same new-head notification WebSocket subscribers get); data is still pulled.

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
pub struct BalFilter {
    pub addresses: Vec<alloy_primitives::Address>,
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
    #[serde(default)]
    pub filters: Vec<Filter>,
    /// State changes to these accounts, from each block's access list, change by change with its
    /// `block_access_index` (a separate hook from logs; either or both may be given).
    pub bal: Option<BalFilter>,
    /// Blocks per response (at most `--max-logs-range`).
    pub max_blocks: Option<u64>,
    /// At the head: how long to wait for a new block before answering with no events.
    pub wait_ms: Option<u64>,
}

pub async fn next(
    chain: &ChainData,
    head: impl Fn() -> Option<u64>,
    mut new_heads: tokio::sync::broadcast::Receiver<crate::rpc::WsEvent>,
    finalized: Option<Value>,
    req: Request,
) -> Result<Value, DataError> {
    let bal_addresses = req.bal.as_ref().map(|b| b.addresses.clone()).unwrap_or_default();
    if req.filters.is_empty() && bal_addresses.is_empty() {
        return Err(DataError::Invalid("give log filters, bal addresses, or both".into()));
    }
    let mut events = Vec::new();
    let Some(mut tip) = head() else { return Err(DataError::Unavailable("not following the chain yet".into())) };

    // Where the module is (`pos`) and the hash it applied there (`anchor`), checked against the
    // chain: a cursor that isn't canonical any more rewinds to the newest canonical `recent` block.
    let start_tip = tip;
    let locate = |events: &mut Vec<Value>| -> Result<(u64, Option<B256>), DataError> {
        Ok(match req.cursor {
            None => (req.from_block.unwrap_or(start_tip).saturating_sub(1), None),
            Some(c) if chain.canonical_hash(c.number) == Some(c.hash) => (c.number, Some(c.hash)),
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
                (a.number, Some(a.hash))
            }
        })
    };
    let (mut pos, mut anchor) = locate(&mut events)?;

    // Before the fork (or, head-only, below the in-memory window) skim has no headers at all:
    // a gap, then continue after it. Blocks within that range whose receipts peers have expired
    // are handled per block by the scan (stored matches are still served).
    let history = chain.history_json();
    let logs_from = history["fork_floor"].as_u64().or_else(|| (history["mode"] == "off").then(|| history["memory_headers_from"].as_u64()).flatten()).unwrap_or(0);
    if pos + 1 < logs_from {
        events.push(json!({"type": "gap", "from": pos + 1, "to": logs_from - 1, "reason": "no headers: before the fork, or outside the in-memory window with --history off"}));
        pos = logs_from - 1;
        anchor = chain.canonical_hash(pos);
    }

    // At the head: wait for the next block (woken by the new-head notification, not polling).
    if pos >= tip {
        let until = Instant::now() + Duration::from_millis(req.wait_ms.unwrap_or(0).min(30_000));
        while pos >= tip && Instant::now() < until {
            let left = until.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, new_heads.recv()).await {
                Ok(Ok(crate::rpc::WsEvent::NewHead(_))) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Ok(_)) => continue,
                Ok(Err(_)) | Err(_) => break,
            }
            tip = head().unwrap_or(tip);
        }
        // The chain may have reorganized while waiting, possibly at the same height: re-check the
        // module's position before answering, and rewind if it isn't canonical any more.
        if anchor.is_some_and(|h| chain.canonical_hash(pos) != Some(h)) {
            if events.is_empty() && req.cursor.is_some_and(|c| c.number == pos) {
                (pos, anchor) = locate(&mut events)?;
            } else {
                return Err(DataError::Unavailable("the chain reorganized during the request; retry".into()));
            }
        }
    }
    // The new position: the last block scanned. With nothing to scan, the module's own position
    // (never another block's hash at that height).
    let cursor = if pos < tip {
        let to = tip.min(pos + req.max_blocks.unwrap_or(chain.max_log_range).clamp(1, chain.max_log_range));
        let scan = if req.filters.is_empty() { None } else { Some(chain.scan(&req.filters, pos + 1, to, None, true).await?) };
        let bal = if bal_addresses.is_empty() { Vec::new() } else { chain.bal_changes(&bal_addresses, pos + 1, to).await? };
        // The range must continue the module's position, by hash.
        if let (Some(a), Some(sc)) = (anchor, &scan)
            && sc.first_parent != a
        {
            return Err(DataError::Unavailable("the chain reorganized during the request; retry".into()));
        }
        // One `apply` per block with logs, state changes, or both, in block order; candidate
        // blocks whose receipts expired on every peer (and aren't stored) as gaps between them.
        let mut by_block: std::collections::BTreeMap<u64, Value> = std::collections::BTreeMap::new();
        if let Some(scan) = &scan {
            for b in &scan.blocks {
                by_block.insert(b.number, json!({
                    "type": "apply",
                    "block": {"number": b.number, "hash": b.hash, "parentHash": b.parent_hash, "timestamp": b.timestamp},
                    "logs": b.logs,
                }));
            }
            let mut run: Option<(u64, u64)> = None;
            let flush = |run: &mut Option<(u64, u64)>, by_block: &mut std::collections::BTreeMap<u64, Value>| {
                if let Some((a, b)) = run.take() {
                    by_block.insert(a, json!({"type": "gap", "from": a, "to": b, "reason": "receipts expired on every connected peer and not stored locally"}));
                }
            };
            for &n in &scan.expired {
                match run {
                    Some((a, b)) if by_block.range(b + 1..n).next().is_none() => run = Some((a, n)),
                    _ => {
                        flush(&mut run, &mut by_block);
                        run = Some((n, n));
                    }
                }
            }
            flush(&mut run, &mut by_block);
        }
        for (n, bh, h, changes) in bal {
            let e = by_block.entry(n).or_insert_with(|| json!({
                "type": "apply",
                "block": {"number": n, "hash": bh, "parentHash": h.parent_hash, "timestamp": h.timestamp},
                "logs": [],
            }));
            e["stateChanges"] = Value::Array(changes);
        }
        events.extend(by_block.into_values());
        let last = match &scan {
            Some(s) => s.last,
            None => (to, chain.canonical_hash(to).ok_or_else(|| DataError::Unavailable(format!("no header for block {to}")))?),
        };
        pos = last.0;
        Some(json!({"number": last.0, "hash": last.1}))
    } else {
        anchor.map(|h| json!({"number": pos, "hash": h})).or_else(|| chain.canonical_hash(pos).map(|h| json!({"number": pos, "hash": h})))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chaindata::ChainData;
    use std::sync::Arc;
    use alloy_consensus::Header;
    use alloy_primitives::{Address, Bloom, Bytes, Log};
    use skim_el::Receipt;

    const EMITTER: Address = Address::repeat_byte(0xe5);

    /// Block `n` on fork `tag`: one transaction whose receipt has one log from EMITTER, carrying
    /// the fork tag in its data (so a test can tell which fork a delivered log came from).
    fn block(n: u64, parent: B256, tag: u8) -> (Header, Vec<Receipt>, B256) {
        let log = Log::new_unchecked(EMITTER, vec![B256::repeat_byte(0x70)], Bytes::from(vec![tag, n as u8]));
        let mut bloom = Bloom::default();
        bloom.accrue_log(&log);
        let h = Header {
            number: n,
            parent_hash: parent,
            logs_bloom: bloom,
            receipts_root: B256::repeat_byte(1), // not empty: receipts come from the seeded cache
            timestamp: n * 12,
            extra_data: vec![tag].into(),
            ..Default::default()
        };
        let receipts = vec![Receipt { success: true, cumulative_gas_used: 21_000, logs: vec![log], ..Default::default() }];
        (h, receipts, B256::repeat_byte(tag) ^ B256::from(alloy_primitives::U256::from(n)))
    }

    /// Notes blocks `from..=to` of fork `tag`, starting from `parent`; returns their hashes.
    fn extend(cd: &ChainData, from: u64, to: u64, mut parent: B256, tag: u8) -> Vec<B256> {
        let mut hashes = Vec::new();
        for n in from..=to {
            let (h, receipts, tx) = block(n, parent, tag);
            let hash = h.hash_slow();
            cd.note_header(&h);
            cd.note_receipts(n, hash, receipts, vec![tx]);
            hashes.push(hash);
            parent = hash;
        }
        hashes
    }

    fn req(cursor: Option<BlockId>, recent: Vec<BlockId>, from_block: Option<u64>) -> Request {
        Request { cursor, recent, from_block, filters: vec![Filter::new().address(EMITTER)], bal: None, max_blocks: Some(64), wait_ms: Some(0) }
    }

    fn id(number: u64, hash: B256) -> BlockId {
        BlockId { number, hash }
    }

    /// Per applied block: number, hash, and the fork tag of each delivered log.
    fn applied(r: &Value) -> Vec<(u64, B256, Vec<u8>)> {
        r["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "apply")
            .map(|e| {
                let n = e["block"]["number"].as_u64().unwrap();
                let h: B256 = e["block"]["hash"].as_str().unwrap().parse().unwrap();
                let tags = e["logs"].as_array().unwrap().iter().map(|l| u8::from_str_radix(&l["data"].as_str().unwrap()[2..4], 16).unwrap());
                (n, h, tags.collect())
            })
            .collect()
    }

    fn head_ref(n: u64, hash: B256) -> skim_state::BlockRef {
        skim_state::BlockRef { number: n, hash, parent_hash: B256::ZERO, state_root: B256::ZERO, timestamp: n * 12 }
    }

    #[tokio::test]
    async fn a_reorg_at_the_same_height_during_a_head_wait_rewinds_instead_of_moving_the_cursor() {
        // Review: cursor A105 at the head, waiting; A105 is replaced by B105 (same height).
        // The answer must rewind (to A104, still canonical) and deliver B105, never just hand back
        // B105 as the cursor with nothing undone.
        let cd = ChainData::offline(2048);
        let a = extend(&cd, 100, 105, B256::repeat_byte(0x99), 0xa);
        let (tx, rx) = tokio::sync::broadcast::channel(4);
        let tip = Arc::new(std::sync::atomic::AtomicU64::new(105));
        let (cd2, tip2) = (cd.clone(), tip.clone());
        let mut r = req(Some(id(105, a[5])), vec![id(104, a[4]), id(103, a[3])], None);
        r.wait_ms = Some(3000);
        let task = tokio::spawn(async move { next(&cd2, move || Some(tip2.load(std::sync::atomic::Ordering::SeqCst)), rx, None, r).await });
        tokio::time::sleep(Duration::from_millis(150)).await;
        let b = extend(&cd, 105, 105, a[4], 0xb);
        tx.send(crate::rpc::WsEvent::NewHead(head_ref(105, b[0]))).unwrap();
        let r = task.await.unwrap().unwrap();
        let ev = r["events"].as_array().unwrap();
        assert_eq!(ev[0]["type"], "rewind", "{r}");
        assert_eq!(ev[0]["to"]["number"], 104);
        assert!(applied(&r).iter().all(|(_, h, t)| *h == b[0] && t == &vec![0xb]), "{r}");
        assert_eq!(r["cursor"]["hash"].as_str().unwrap().parse::<B256>().unwrap(), b[0]);
    }

    #[tokio::test]
    async fn an_empty_answer_keeps_the_modules_own_cursor() {
        // Nothing new within the wait: the cursor comes back as the module sent it.
        let cd = ChainData::offline(2048);
        let a = extend(&cd, 100, 105, B256::repeat_byte(0x99), 0xa);
        let mut r = req(Some(id(105, a[5])), vec![], None);
        r.wait_ms = Some(50);
        let r = next(&cd, || Some(105), tokio::sync::broadcast::channel(1).1, None, r).await.unwrap();
        assert!(r["events"].as_array().unwrap().is_empty());
        assert_eq!(r["cursor"]["hash"].as_str().unwrap().parse::<B256>().unwrap(), a[5]);
    }

    #[tokio::test]
    async fn stored_matches_are_served_after_peers_expire_them() {
        // Review: block 120's matches are on disk; every peer's earliest becomes 200. A stream
        // still gets 120 (no network, no gap). Without the store, the same block is a gap.
        let dir = tempfile::tempdir().unwrap();
        let cd = ChainData::offline(2048);
        cd.set_history(crate::history::HeaderStore::open(dir.path().join("h.redb")).unwrap());
        extend(&cd, 118, 122, B256::repeat_byte(0x99), 0xa);
        let first = next(&cd, || Some(122), tokio::sync::broadcast::channel(1).1, None, req(None, vec![], Some(118))).await.unwrap();
        assert_eq!(applied(&first).len(), 5);
        *cd.test_peers_from.lock() = Some(200);
        let again = next(&cd, || Some(122), tokio::sync::broadcast::channel(1).1, None, req(None, vec![], Some(118))).await.unwrap();
        assert_eq!(applied(&again).len(), 5, "{again}");
        assert!(again["events"].as_array().unwrap().iter().all(|e| e["type"] != "gap"), "{again}");

        let bare = ChainData::offline(2048);
        extend(&bare, 118, 122, B256::repeat_byte(0x99), 0xa);
        *bare.test_peers_from.lock() = Some(200);
        let r = next(&bare, || Some(122), tokio::sync::broadcast::channel(1).1, None, req(None, vec![], Some(118))).await.unwrap();
        assert!(applied(&r).is_empty());
        let gaps: Vec<_> = r["events"].as_array().unwrap().iter().filter(|e| e["type"] == "gap").collect();
        assert_eq!((gaps.len(), gaps[0]["from"].as_u64(), gaps[0]["to"].as_u64()), (1, Some(118), Some(122)), "{r}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_scan_cancelled_while_waiting_releases_its_own_reservations() {
        // Review: another scan holds X; scan B wants X and Y, reserves Y, waits for X, and is
        // cancelled. Y's reservation must not outlive it.
        let dir = tempfile::tempdir().unwrap();
        let cd = ChainData::offline(2048);
        cd.set_history(crate::history::HeaderStore::open(dir.path().join("h.redb")).unwrap());
        let h = extend(&cd, 100, 101, B256::repeat_byte(0x99), 0xa);
        let filters = vec![Filter::new().address(EMITTER)];
        let fp = crate::chaindata::filters_fingerprint(&filters);
        let key = |b: B256| alloy_primitives::keccak256([fp.as_slice(), b.as_slice()].concat());
        cd.inflight.lock().insert(key(h[0]), Arc::new(tokio::sync::Semaphore::new(0))); // "another scan" holds X
        let cd2 = cd.clone();
        let task = tokio::spawn(async move { cd2.scan(&filters, 100, 101, None, true).await.map(|_| ()) });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(cd.inflight.lock().contains_key(&key(h[1])), "B reserved Y while waiting for X");
        task.abort();
        let _ = task.await;
        assert!(!cd.inflight.lock().contains_key(&key(h[1])), "Y's reservation outlived the cancelled scan");
        assert!(cd.inflight.lock().contains_key(&key(h[0])), "X still belongs to the other scan");
    }

    #[tokio::test]
    async fn a_reorg_rewinds_the_module_and_never_delivers_the_old_fork_again() {
        // Fork A: 100..=110. The module applied up to A105.
        let cd = ChainData::offline(2048);
        let a = extend(&cd, 100, 110, B256::repeat_byte(0x99), 0xa);
        let r = next(&cd, || Some(110), tokio::sync::broadcast::channel(1).1, None, req(None, vec![], Some(100))).await.unwrap();
        assert!(applied(&r).iter().all(|(_, _, t)| t == &vec![0xa]), "before the reorg: fork A");

        // Reorg at 104: B104 (child of A103) .. B112 replace A104..A110.
        let b = extend(&cd, 104, 112, a[3], 0xb);
        let recent = vec![id(105, a[5]), id(104, a[4]), id(103, a[3]), id(100, a[0])];
        let r = next(&cd, || Some(112), tokio::sync::broadcast::channel(1).1, None, req(Some(id(105, a[5])), recent, None)).await.unwrap();
        let events = r["events"].as_array().unwrap();
        assert_eq!(events[0]["type"], "rewind", "{r}");
        assert_eq!(events[0]["to"]["number"], 103, "to the newest recent block still on the chain");
        let blocks = applied(&r);
        assert_eq!(blocks.iter().map(|b| b.0).collect::<Vec<_>>(), (104..=112).collect::<Vec<_>>());
        for (n, h, tags) in &blocks {
            assert_eq!(tags, &vec![0xb], "block {n}: only fork B's logs after the rewind");
            assert_eq!(*h, b[(*n - 104) as usize]);
        }
        assert_eq!(r["cursor"]["hash"], json!(b[8]));
    }

    #[tokio::test]
    async fn a_cursor_with_no_canonical_recent_block_is_refused_not_guessed() {
        let cd = ChainData::offline(2048);
        let a = extend(&cd, 100, 110, B256::repeat_byte(0x99), 0xa);
        extend(&cd, 104, 112, a[3], 0xb);
        // Everything the module reports is on the abandoned fork.
        let r = next(&cd, || Some(112), tokio::sync::broadcast::channel(1).1, None, req(Some(id(106, a[6])), vec![id(105, a[5]), id(104, a[4])], None)).await;
        let Err(DataError::Unavailable(m)) = r else { panic!("expected a refusal, got {r:?}") };
        assert!(m.contains("not on the chain"), "{m}");
    }

    #[tokio::test]
    async fn blocks_skim_cant_serve_are_an_explicit_gap() {
        // Head-only (no history store): logs cover the in-memory window, 100..=110.
        let cd = ChainData::offline(2048);
        extend(&cd, 100, 110, B256::repeat_byte(0x99), 0xa);
        let r = next(&cd, || Some(110), tokio::sync::broadcast::channel(1).1, None, req(None, vec![], Some(50))).await.unwrap();
        let e = &r["events"][0];
        assert_eq!((e["type"].as_str(), e["from"].as_u64(), e["to"].as_u64()), (Some("gap"), Some(50), Some(99)), "{r}");
        assert_eq!(applied(&r).first().map(|b| b.0), Some(100));
    }

    #[tokio::test]
    async fn get_logs_by_block_hash_never_returns_the_replacement_block() {
        // eth_getLogs({blockHash: A105}): the RPC resolves the hash to height 105, then a reorg
        // replaces A105 with B105 before the scan.
        let cd = ChainData::offline(2048);
        let a = extend(&cd, 100, 110, B256::repeat_byte(0x99), 0xa);
        let n = cd.number_of(&a[5]).unwrap();
        let f = Filter::new().address(EMITTER);
        assert_eq!(cd.logs(&f, n, n, Some(a[5])).await.unwrap().as_array().unwrap().len(), 1);
        extend(&cd, 104, 112, a[3], 0xb);
        let after = cd.logs(&f, n, n, Some(a[5])).await;
        assert!(matches!(after, Err(DataError::Unavailable(ref m)) if m.contains("no longer on the chain")), "{after:?}");
    }
}
