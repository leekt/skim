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
//! - `gap {from, to, scope}`: blocks skim can't serve any more (`scope: "all"` before the fork
//!   or the in-memory window: no headers; `"logs"` when receipts expired on every connected
//!   peer); the module decides how to mark them.
//! - `apply {block, logs, stateChanges?, logsAvailable?}`: a block with logs matching the
//!   module's filters, or (with `bal`) changes to its accounts from the block's access list, each
//!   with its `block_access_index` (blocks with neither are skipped; `cursor` says how far the
//!   scan went). Logs and state changes are separate channels: a block in a logs gap still gets
//!   its state changes, marked `logsAvailable: false`.
//! Everything in a response (logs, state changes, the cursor) comes from one hash-linked range
//! of headers that continues the module's cursor.
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
    // Make the headers the check needs available first: after a skim restart, the blocks between
    // what's on disk and the new in-memory window must be walked back before any hash can be
    // compared. A hash skim still can't establish is a retry, never "not on the chain".
    if let Some(c) = req.cursor {
        let lowest = req.recent.iter().map(|b| b.number).chain([c.number]).min().unwrap_or(c.number);
        chain.ensure_headers(lowest).await?;
        if chain.canonical_hash(c.number).is_none() {
            return Err(DataError::Unavailable(format!("block {} isn't known to skim yet; retry", c.number)));
        }
    }
    let (mut pos, mut anchor) = locate(&mut events)?;

    // Before the fork (or, head-only, below the in-memory window) skim has no headers at all:
    // a gap, then continue after it. Blocks within that range whose receipts peers have expired
    // are handled per block by the scan (stored matches are still served).
    let history = chain.history_json();
    let logs_from = history["fork_floor"].as_u64().or_else(|| (history["mode"] == "off").then(|| history["memory_headers_from"].as_u64()).flatten()).unwrap_or(0);
    if pos + 1 < logs_from {
        events.push(json!({"type": "gap", "from": pos + 1, "to": logs_from - 1, "scope": "all", "reason": "no headers: before the fork, or outside the in-memory window with --history off"}));
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
                return Err(DataError::ChainChanged("the chain reorganized during the request; retry".into()));
            }
        }
    }
    // The new position: the last block scanned. With nothing to scan, the module's own position
    // (never another block's hash at that height).
    let cursor = if pos < tip {
        let to = tip.min(pos + req.max_blocks.unwrap_or(chain.max_log_range).clamp(1, chain.max_log_range));
        // One hash-linked range of headers for everything this response reports: logs, state
        // changes and the new cursor all come from it (never re-resolved), and it must continue
        // the module's position by hash. A reorg after this point leaves the response consistent
        // with the chain it was captured from; the next request then rewinds.
        #[cfg(test)]
        chain.run_test_hook("before_capture");
        let headers = chain.linked_headers(pos + 1, to).await?;
        #[cfg(test)]
        chain.run_test_hook("after_capture");
        if let Some(a) = anchor
            && headers.first().is_none_or(|(_, _, h)| h.parent_hash != a)
        {
            return Err(DataError::ChainChanged("the chain reorganized during the request; retry".into()));
        }
        let scan = if req.filters.is_empty() { None } else { Some(chain.scan_headers(&req.filters, &headers, true).await?) };
        #[cfg(test)]
        chain.run_test_hook("after_scan");
        let bal = if bal_addresses.is_empty() { Vec::new() } else { chain.bal_changes(&bal_addresses, &headers).await? };
        let hash_at: std::collections::HashMap<u64, B256> = headers.iter().map(|(n, bh, _)| (*n, *bh)).collect();
        // Per block: an `apply` with logs, state changes, or both. Logs and state changes are
        // separate channels: a block whose receipts expired on every peer (and aren't stored) is
        // in a `gap` scoped to logs, and still gets an `apply` for its state changes, marked
        // `logsAvailable: false`.
        let mut applies: std::collections::BTreeMap<u64, Value> = std::collections::BTreeMap::new();
        let block_json = |n: u64, bh: B256, h: &alloy_consensus::Header| json!({"number": n, "hash": bh, "parentHash": h.parent_hash, "timestamp": h.timestamp});
        let mut expired: Vec<u64> = Vec::new();
        if let Some(scan) = &scan {
            for b in &scan.blocks {
                debug_assert_eq!(hash_at.get(&b.number), Some(&b.hash));
                applies.insert(b.number, json!({
                    "type": "apply",
                    "block": {"number": b.number, "hash": b.hash, "parentHash": b.parent_hash, "timestamp": b.timestamp},
                    "logs": b.logs,
                }));
            }
            expired = scan.expired.clone();
        }
        for (n, bh, h, changes) in bal {
            if hash_at.get(&n) != Some(&bh) {
                return Err(DataError::ChainChanged("the chain reorganized during the request; retry".into()));
            }
            let e = applies.entry(n).or_insert_with(|| json!({"type": "apply", "block": block_json(n, bh, &h), "logs": []}));
            e["stateChanges"] = Value::Array(changes);
            if expired.contains(&n) {
                e["logsAvailable"] = json!(false);
            }
        }
        // Expired blocks as `gap` runs (consecutive numbers), each before the applies it spans.
        let mut gaps: Vec<(u64, u64)> = Vec::new();
        for &n in &expired {
            match gaps.last_mut() {
                Some((_, b)) if *b + 1 == n => *b = n,
                _ => gaps.push((n, n)),
            }
        }
        let mut gaps = gaps.into_iter().peekable();
        for (n, e) in applies {
            while let Some((a, b)) = gaps.next_if(|(a, _)| *a <= n) {
                events.push(json!({"type": "gap", "from": a, "to": b, "scope": "logs", "reason": "receipts expired on every connected peer and not stored locally"}));
            }
            events.push(e);
        }
        for (a, b) in gaps {
            events.push(json!({"type": "gap", "from": a, "to": b, "scope": "logs", "reason": "receipts expired on every connected peer and not stored locally"}));
        }
        let (n, bh, _) = headers.last().expect("pos < to");
        pos = *n;
        Some(json!({"number": n, "hash": bh}))
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
            // A post-fork header (the history store only keeps those).
            base_fee_per_gas: Some(7),
            withdrawals_root: Some(B256::ZERO),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(B256::ZERO),
            requests_hash: Some(B256::ZERO),
            block_access_list_hash: Some(B256::ZERO),
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
        let key = |b: B256| b;
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
    async fn after_a_skim_restart_an_unknown_cursor_is_a_retry_not_a_rewind() {
        // Found in deployment: the previous run stored headers up to A110; skim restarted and its
        // in-memory headers start at A113, so 111-112 are on neither. The module's cursor (A110)
        // can't be compared until they're walked: a retry, never "not on the chain". Once they
        // are there, the module continues without a rewind.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.redb");
        let mut all = Vec::new();
        let mut parent = B256::repeat_byte(0x99);
        for n in 100..=120 {
            let (h, receipts, tx) = block(n, parent, 0xa);
            parent = h.hash_slow();
            all.push((h, receipts, tx));
        }
        {
            let cd = ChainData::offline(2048);
            cd.set_history(crate::history::HeaderStore::open(&path).unwrap());
            for (h, _, _) in &all[..=10] {
                cd.note_header(h); // 100..=110 written as tips
            }
        }
        let cd = ChainData::offline(2048);
        cd.set_history(crate::history::HeaderStore::open(&path).unwrap());
        for (h, receipts, tx) in &all[13..] {
            cd.note_header(h);
            cd.note_receipts(h.number, h.hash_slow(), receipts.clone(), vec![*tx]);
        }
        let hash = |n: u64| all[(n - 100) as usize].0.hash_slow();
        let req_at_110 = || req(Some(id(110, hash(110))), vec![id(109, hash(109))], None);
        let r = next(&cd, || Some(120), tokio::sync::broadcast::channel(1).1, None, req_at_110()).await;
        let e = format!("{:?}", r.expect_err("111-112 can't be walked offline"));
        assert!(!e.contains("not on the chain"), "{e}");
        // Peers serve 111-112 (here: written straight to the store).
        let run: Vec<(B256, &Header)> = all[11..13].iter().map(|(h, _, _)| (h.hash_slow(), h)).collect();
        cd.history_store().unwrap().put_run(&run).unwrap();
        for (h, receipts, tx) in &all[11..13] {
            cd.note_receipts(h.number, h.hash_slow(), receipts.clone(), vec![*tx]);
        }
        let r = next(&cd, || Some(120), tokio::sync::broadcast::channel(1).1, None, req_at_110()).await.unwrap();
        assert!(r["events"].as_array().unwrap().iter().all(|e| e["type"] != "rewind"), "{r}");
        assert_eq!(applied(&r).first().map(|x| x.0), Some(111), "{r}");
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
        assert!(matches!(after, Err(DataError::ChainChanged(ref m)) if m.contains("no longer on the chain")), "{after:?}");
    }

    const WATCHED: Address = Address::repeat_byte(0xba);

    /// A BAL in which WATCHED's balance becomes `value` (by transaction 0).
    fn bal(value: u64) -> skim_bal::BlockAccessList {
        use skim_bal::alloy_eip7928::{BalanceChange, BlockAccessIndex};
        vec![skim_bal::AccountChanges::new(WATCHED).with_balance_change(BalanceChange::new(BlockAccessIndex(1), alloy_primitives::U256::from(value)))]
    }

    fn req_bal(cursor: Option<BlockId>, recent: Vec<BlockId>, logs: bool) -> Request {
        let mut r = req(cursor, recent, None);
        r.bal = Some(BalFilter { addresses: vec![WATCHED] });
        if !logs {
            r.filters.clear();
        }
        r
    }

    /// Per apply: (number, hash, state change values).
    fn changes(r: &Value) -> Vec<(u64, B256, Vec<u64>)> {
        r["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "apply")
            .map(|e| {
                let vals = e["stateChanges"].as_array().map(|c| c.iter().map(|x| u64::from_str_radix(x["value"].as_str().unwrap().trim_start_matches("0x"), 16).unwrap()).collect()).unwrap_or_default();
                (e["block"]["number"].as_u64().unwrap(), e["block"]["hash"].as_str().unwrap().parse().unwrap(), vals)
            })
            .collect()
    }

    #[tokio::test]
    async fn logs_state_changes_and_cursor_come_from_one_chain_despite_a_reorg_mid_request() {
        // Review P1: cursor A100; the logs of A101 are scanned, then A100/A101 are replaced by
        // B100/B101 before the BALs are read. The response must be all A (consistent; the next
        // request rewinds), never A101's block and logs with B101's state changes.
        let cd = ChainData::offline(2048);
        let a = extend(&cd, 99, 101, B256::repeat_byte(0x99), 0xa);
        cd.note_bal(a[2], bal(0xa));
        let cd2 = cd.clone();
        let a99 = a[0];
        cd.test_hooks.lock().insert("after_scan", Box::new(move || {
            let b = extend(&cd2, 100, 101, a99, 0xb);
            cd2.note_bal(b[1], bal(0xb));
        }));
        let r = next(&cd, || Some(101), tokio::sync::broadcast::channel(1).1, None, req_bal(Some(id(100, a[1])), vec![id(99, a[0])], true)).await.unwrap();
        assert_eq!(applied(&r), vec![(101, a[2], vec![0xa])], "{r}");
        assert_eq!(changes(&r), vec![(101, a[2], vec![0xa])], "{r}");
        assert_eq!(r["cursor"]["hash"].as_str().unwrap().parse::<B256>().unwrap(), a[2]);
    }

    #[tokio::test]
    async fn a_bal_only_stream_never_continues_a_cursor_onto_another_fork() {
        // Review P1: with no log filters there was no cursor-parent check. Cursor A100 is
        // accepted, then the chain becomes B100/B101 before the range is read: refuse (retry),
        // never Apply(B101) after A100 without a rewind.
        let cd = ChainData::offline(2048);
        let a = extend(&cd, 99, 101, B256::repeat_byte(0x99), 0xa);
        cd.note_bal(a[2], bal(0xa));
        let cd2 = cd.clone();
        let a99 = a[0];
        cd.test_hooks.lock().insert("before_capture", Box::new(move || {
            let b = extend(&cd2, 100, 101, a99, 0xb);
            cd2.note_bal(b[0], vec![]);
            cd2.note_bal(b[1], bal(0xb));
        }));
        let r = next(&cd, || Some(101), tokio::sync::broadcast::channel(1).1, None, req_bal(Some(id(100, a[1])), vec![id(99, a[0])], false)).await;
        assert!(matches!(r, Err(DataError::ChainChanged(_))), "{:?}", r.map(|v| v.to_string()));
        // Retried: rewinds to 99, then B101's changes.
        let r = next(&cd, || Some(101), tokio::sync::broadcast::channel(1).1, None, req_bal(Some(id(100, a[1])), vec![id(99, a[0])], false)).await.unwrap();
        assert_eq!(r["events"][0]["type"], "rewind", "{r}");
        assert_eq!(changes(&r).iter().map(|(n, _, v)| (*n, v.clone())).collect::<Vec<_>>(), vec![(101, vec![0xb])], "{r}");
    }

    #[tokio::test]
    async fn state_changes_still_arrive_for_a_block_whose_receipts_expired() {
        // Review P2: logs for 101 are unavailable (expired, not stored), its BAL is known. The
        // changes must come as an apply with the block's hash, marked logsAvailable: false, and
        // the gap must be scoped to logs.
        let cd = ChainData::offline(2048);
        let a = extend(&cd, 100, 101, B256::repeat_byte(0x99), 0xa);
        cd.note_bal(a[1], bal(7));
        *cd.test_peers_from.lock() = Some(200);
        let r = next(&cd, || Some(101), tokio::sync::broadcast::channel(1).1, None, req_bal(Some(id(100, a[0])), vec![], true)).await.unwrap();
        let ev = r["events"].as_array().unwrap();
        assert_eq!(ev.len(), 2, "{r}");
        assert_eq!((ev[0]["type"].as_str(), ev[0]["scope"].as_str(), ev[0]["from"].as_u64()), (Some("gap"), Some("logs"), Some(101)), "{r}");
        assert!(ev[0].get("stateChanges").is_none());
        assert_eq!(changes(&r), vec![(101, a[1], vec![7])], "{r}");
        assert_eq!(ev[1]["logsAvailable"], false);
    }

    #[tokio::test]
    async fn eth_get_logs_answers_from_stored_matches_after_peers_expire_them() {
        // Review (earlier finding, partly open): a stream stored block 120's matches; peers then
        // expire it. eth_getLogs only reads (persist = false) but must still use what's stored.
        let dir = tempfile::tempdir().unwrap();
        let cd = ChainData::offline(2048);
        cd.set_history(crate::history::HeaderStore::open(dir.path().join("h.redb")).unwrap());
        extend(&cd, 118, 122, B256::repeat_byte(0x99), 0xa);
        next(&cd, || Some(122), tokio::sync::broadcast::channel(1).1, None, req(None, vec![], Some(118))).await.unwrap();
        *cd.test_peers_from.lock() = Some(200);
        let s = cd.scan(&[Filter::new().address(EMITTER)], 118, 122, None, false).await.map_err(|e| format!("{e:?}")).unwrap();
        assert_eq!(s.blocks.len(), 5);
        // A filter never stored for still refuses rather than answering from partial coverage.
        let other = Filter::new().address(EMITTER).event_signature(B256::repeat_byte(0x70));
        assert!(cd.scan(&[other], 118, 122, None, false).await.is_err());
    }
}
