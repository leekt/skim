//! Restart persistence for `skim run` (see `skim_state::persist`).
//!
//! - **Node key:** generated once and reused, so peers recognise a restarted node.
//! - **Peers:** good EL peers are saved every minute and re-dialed on start.
//! - **State snapshot:** subscribed keys' values at a block, saved every 4 blocks and on
//!   shutdown. On start it is verified and replayed to the head before use:
//!   headers from the snapshot block to the head must link by hash (both ends included), each
//!   block's BAL must match its header, and the writes are replayed in order. If anything
//!   doesn't check out, the snapshot is discarded and state is fetched as without it.

use std::{sync::Arc, time::Instant};

use alloy_primitives::B256;
use skim_el::{ElNet, Peer};
use skim_state::{BlockRef, BlockWrites, Retention, Store, persist::{Persist, Saved}};

/// Longest gap a snapshot is replayed across (each block costs a header and a BAL download).
const MAX_REPLAY: u64 = 8192;

pub fn node_key(p: &Persist) -> anyhow::Result<[u8; 32]> {
    if let Some(k) = p.get_meta("node_key")?
        && let Ok(k) = <[u8; 32]>::try_from(k.as_slice())
    {
        return Ok(k);
    }
    let mut k = [0u8; 32];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut k)?;
    p.set_meta("node_key", &k)?;
    Ok(k)
}

const MAX_SAVED_PEERS: usize = 64;

pub fn load_peers(p: &Persist) -> Vec<(Peer, std::net::SocketAddr)> {
    let Ok(Some(raw)) = p.get_meta("peers") else { return Vec::new() };
    String::from_utf8_lossy(&raw)
        .lines()
        .filter_map(|l| {
            let (id, addr) = l.split_once('@')?;
            Some((id.parse().ok()?, addr.parse().ok()?))
        })
        .collect()
}

pub fn spawn_peer_saver(p: Arc<Persist>, net: ElNet) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let peers = net.good_peers();
            if peers.is_empty() {
                continue;
            }
            // This run's peers, best first (held SNAP sessions lead), then previously saved ones
            // not judged this run, up to MAX_SAVED_PEERS. Peers this run found useless (crawlers,
            // only ever dropped us) are left out, even if an older list had them.
            let cooled: std::collections::HashSet<_> = net.cooled_peers().into_iter().collect();
            let mut seen = std::collections::HashSet::new();
            let text: String = peers
                .into_iter()
                .chain(load_peers(&p).into_iter().filter(|(id, _)| !cooled.contains(id)))
                .filter(|(id, _)| seen.insert(*id))
                .take(MAX_SAVED_PEERS)
                .map(|(id, a)| format!("{id}@{a}\n"))
                .collect();
            if let Err(e) = p.set_meta("peers", text.as_bytes()) {
                tracing::warn!("saving peers: {e}");
            }
        }
    });
}

/// Saves the subscribed state at `head` (off the async runtime).
pub fn save_now(p: &Arc<Persist>, store: &Store, head: &BlockRef) {
    let (values, code) = store.subscribed_at(head);
    if values.is_empty() {
        return;
    }
    let (p, number, hash) = (p.clone(), head.number, head.hash);
    let t = std::thread::spawn(move || p.save(number, hash, &values, &code));
    // Shutdown needs the write finished; per-block saves are small and quick too.
    match t.join() {
        Ok(Err(e)) => tracing::warn!("saving restart snapshot: {e}"),
        Err(_) => tracing::warn!("saving restart snapshot: writer panicked"),
        Ok(Ok(())) => {}
    }
}

/// Verifies the snapshot against the chain up to `head` and installs it. Never partially:
/// either every check passes and all keys are inserted, or nothing is.
pub async fn restore(store: Arc<Store>, net: ElNet, cl: skim_cl::ClNode, saved: Saved, head: BlockRef) {
    let t = Instant::now();
    // Consensus peers connect within seconds of start; EL peers can take minutes. Try the
    // envelopes (payload + BAL, verified like gossip) first, then fall back to EL.
    if saved.number < head.number && head.number - saved.number <= MAX_REPLAY {
        match fetch_via_cl(&cl, &saved, &head).await {
            Ok(blocks) => match verify_and_replay(&saved, &blocks, &head).and_then(|(v, c)| install(&store, &head, &v, &c)) {
                Ok(n) => {
                    tracing::info!(keys = n, from = saved.number, to = head.number, ms = t.elapsed().as_millis() as u64,
                        source = "consensus peers", "restart snapshot verified and replayed to the head");
                    return;
                }
                Err(e) => tracing::warn!("restart via consensus peers failed ({e}); trying EL peers"),
            },
            Err(e) => tracing::info!("restart via consensus peers unavailable ({e}); trying EL peers"),
        }
    }
    match restore_inner(&store, &net, &saved, &head).await {
        Ok(n) => tracing::info!(keys = n, from = saved.number, to = head.number, ms = t.elapsed().as_millis() as u64,
            source = "EL peers", "restart snapshot verified and replayed to the head"),
        Err(e) => tracing::warn!(from = saved.number, "restart snapshot discarded ({e}); fetching state as usual"),
    }
}

async fn restore_inner(store: &Arc<Store>, net: &ElNet, saved: &Saved, head: &BlockRef) -> anyhow::Result<usize> {
    use reth_eth_wire_types::BlockHashOrNumber;
    if saved.number >= head.number {
        // The snapshot is at or after the first block we applied: check it's on this chain.
        let b = store.block(saved.number).ok_or_else(|| anyhow::anyhow!("block {} not in the window", saved.number))?;
        anyhow::ensure!(b.hash == saved.hash, "snapshot block {} is not on the current chain", saved.number);
        return install(store, &b, &saved.values, &saved.code);
    }
    let gap = head.number - saved.number;
    anyhow::ensure!(gap <= MAX_REPLAY, "snapshot is {gap} blocks old (max {MAX_REPLAY})");
    // EL peers come up after CL gossip; give them time.
    let deadline = Instant::now() + std::time::Duration::from_secs(300);
    let mut fetched: Vec<(alloy_consensus::Header, BlockWrites)> = Vec::with_capacity(gap as usize);
    let mut prev = saved.hash;
    let mut next = saved.number + 1;
    while next <= head.number {
        anyhow::ensure!(Instant::now() < deadline, "no EL peer served headers + BALs from {next}");
        let peers: Vec<Peer> = net.peers().into_iter().filter(|(_, p)| p.eth_version >= 71).map(|(id, _)| id).collect();
        let mut progressed = false;
        for peer in peers {
            let count = (head.number - next + 1).min(64);
            let Ok(hs) = net.headers(peer, BlockHashOrNumber::Number(next), count).await.0 else { continue };
            if hs.is_empty() || hs[0].number != next {
                continue;
            }
            let hashes: Vec<B256> = hs.iter().map(|h| h.hash_slow()).collect();
            // Chain link: each header's parent is the previous hash (the snapshot block first).
            if hs.iter().zip(std::iter::once(prev).chain(hashes.iter().copied())).any(|(h, p)| h.parent_hash != p) {
                continue;
            }
            let Ok(bals) = net.bals(peer, hashes.clone()).await.0 else { continue };
            let mut ok = 0;
            for (h, raw) in hs.iter().zip(bals.0) {
                let Some(raw) = raw else { break };
                let Ok(bal) = skim_bal::decode_verified(&raw, h.block_access_list_hash.unwrap_or_default()) else { break };
                fetched.push((h.clone(), BlockWrites::from_bal(&bal)));
                ok += 1;
            }
            if ok == 0 {
                continue;
            }
            prev = hashes[ok - 1];
            next += ok as u64;
            progressed = true;
            break;
        }
        if !progressed {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }
    let (values, code) = verify_and_replay(saved, &fetched, head)?;
    install(store, head, &values, &code)
}

/// Blocks `saved.number + 1 ..= head.number` from consensus peers' payload envelopes, walking
/// back from the head's slot. Each envelope is checked like gossip (the rebuilt header must
/// hash to the payload's block hash, which commits to the BAL); linkage is checked after.
/// One `ExecutionPayloadEnvelopesByRange` request of a catch-up walk.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RangeRequest {
    pub start_slot: u64,
    pub end_slot: u64,
    pub peer: Option<String>,
    pub ms: u64,
    pub envelopes: usize,
    pub bytes: usize,
    pub error: Option<String>,
}

/// Where a catch-up walk's time went (logged after every attempt, success or not).
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct CatchupTrace {
    pub slots_walked: u64,
    pub blocks_found: usize,
    pub total_ms: u64,
    pub outcome: String,
    pub requests: Vec<RangeRequest>,
}

impl CatchupTrace {
    /// A compact summary: request count, latency distribution, empty answers, errors by kind,
    /// and per-peer success/stall counts.
    pub fn summary(&self) -> serde_json::Value {
        let mut ms: Vec<u64> = self.requests.iter().map(|r| r.ms).collect();
        ms.sort_unstable();
        let q = |p: f64| ms.get(((ms.len() as f64 - 1.0) * p).round() as usize).copied().unwrap_or(0);
        let mut errors: std::collections::BTreeMap<String, usize> = Default::default();
        let mut peers: std::collections::BTreeMap<String, (usize, usize)> = Default::default();
        for r in &self.requests {
            if let Some(e) = &r.error {
                let kind = if e.contains("timed out") { "timeout" } else if e.contains("no CL peer") { "no_candidate" } else { "peer_error" };
                *errors.entry(kind.into()).or_default() += 1;
            }
            let p = peers.entry(r.peer.clone().unwrap_or_else(|| "-".into())).or_default();
            if r.error.is_none() && r.envelopes > 0 { p.0 += 1 } else { p.1 += 1 }
        }
        let waiting: u64 = self.requests.iter().filter(|r| r.error.is_some()).map(|r| r.ms).sum();
        serde_json::json!({
            "outcome": self.outcome,
            "total_ms": self.total_ms,
            "slots_walked": self.slots_walked,
            "blocks_found": self.blocks_found,
            "requests": self.requests.len(),
            "request_ms": {"p50": q(0.5), "p90": q(0.9), "max": q(1.0)},
            "ms_in_failed_requests": waiting,
            "empty_answers": self.requests.iter().filter(|r| r.error.is_none() && r.envelopes == 0).count(),
            "envelopes": self.requests.iter().map(|r| r.envelopes).sum::<usize>(),
            "bytes": self.requests.iter().map(|r| r.bytes).sum::<usize>(),
            "errors": errors,
            "peers_ok_failed": peers,
        })
    }
}

/// Catch-up budget: the whole walk, and each request.
const CATCHUP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);
const CATCHUP_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Range requests in flight at once (each goes to a different peer, round robin). A 32-slot
/// answer is several MB of payloads and takes ~2-6 s, so one at a time can't cover a gap of a
/// few hundred blocks within the budget (measured with `skim probe-catchup`).
const CATCHUP_PARALLEL: usize = 8;

/// Walks back from `head_slot` in 32-slot `ByRange` windows, newest first, up to
/// [`CATCHUP_PARALLEL`] at a time, until `keep` says the envelopes collected so far suffice,
/// recording every request in `trace`. Each envelope is verified (payload hash) before `keep`
/// sees it.
pub async fn walk_envelopes(
    cl: &skim_cl::ClNode,
    head_slot: u64,
    max_slots: u64,
    expected_blocks: u64,
    trace: &mut CatchupTrace,
    mut keep: impl FnMut(alloy_consensus::Header, BlockWrites) -> bool,
) -> anyhow::Result<()> {
    use futures::stream::{FuturesUnordered, StreamExt};
    const CHUNK: u64 = 32;
    let t0 = Instant::now();
    let deadline = t0 + CATCHUP_DEADLINE;
    let lowest_slot = head_slot.saturating_sub(max_slots);
    // One window, with up to 4 attempts; returns its requests and the envelopes (if any).
    let fetch = |start: u64, end: u64| async move {
        let mut reqs = Vec::new();
        for _ in 0..4 {
            let left = deadline.saturating_duration_since(Instant::now()).min(CATCHUP_REQUEST_TIMEOUT);
            if left.is_zero() {
                break;
            }
            let t = Instant::now();
            let (peer, res) = cl.envelopes_by_range_from(start, end - start + 1, left).await;
            let mut req = RangeRequest { start_slot: start, end_slot: end, peer: peer.map(|p| p.to_string()), ms: t.elapsed().as_millis() as u64, envelopes: 0, bytes: 0, error: None };
            match res {
                Ok(c) => {
                    req.envelopes = c.len();
                    req.bytes = c.iter().map(Vec::len).sum();
                    tracing::debug!(?req, "catch-up request");
                    reqs.push(req);
                    if !c.is_empty() {
                        return (start, reqs, Some(c));
                    }
                }
                Err(e) => {
                    req.error = Some(e.to_string());
                    tracing::debug!(?req, "catch-up request");
                    reqs.push(req);
                }
            }
        }
        (start, reqs, None)
    };
    let mut next_end = Some(head_slot);
    let mut inflight = FuturesUnordered::new();
    let mut lowest_started = head_slot + 1;
    // Start only as many windows as the gap can need (a 3-block gap: 2, not 8 x 32 slots of
    // payloads); widen to CATCHUP_PARALLEL once a window has come back without finishing.
    let mut parallel = (expected_blocks.div_ceil(CHUNK) as usize + 1).min(CATCHUP_PARALLEL);
    let r: anyhow::Result<()> = async {
        loop {
            while inflight.len() < parallel
                && let Some(end) = next_end
            {
                let start = end.saturating_sub(CHUNK - 1).max(lowest_slot);
                lowest_started = start;
                inflight.push(fetch(start, end));
                next_end = (start > lowest_slot).then(|| start - 1);
            }
            let Some((start, reqs, chunks)) = inflight.next().await else {
                anyhow::bail!("walked back {} slots without finishing", head_slot - lowest_slot);
            };
            trace.requests.extend(reqs);
            let chunks = chunks.ok_or_else(|| {
                if Instant::now() >= deadline {
                    anyhow::anyhow!("walk budget ({} s) exhausted with slots {start}.. unanswered", CATCHUP_DEADLINE.as_secs())
                } else {
                    anyhow::anyhow!("no peer served envelopes for slots {start}..={}", start + CHUNK - 1)
                }
            })?;
            trace.slots_walked = head_slot - lowest_started + 1;
            let mut finished = false;
            for ssz in chunks {
                let env = skim_cl::ssz::Envelope::parse(&ssz)?;
                let v = skim_cl::verify::verify_payload(&env);
                anyhow::ensure!(v.ok, "envelope for block {} does not hash to its block_hash", env.payload.block_number);
                let bal = skim_bal::decode(env.payload.block_access_list)?;
                trace.blocks_found += 1;
                finished |= keep(v.header, BlockWrites::from_bal(&bal));
            }
            if finished {
                return Ok(());
            }
            parallel = CATCHUP_PARALLEL;
        }
    }
    .await;
    trace.total_ms = t0.elapsed().as_millis() as u64;
    trace.outcome = match &r {
        Ok(()) => "complete".into(),
        Err(e) => e.to_string(),
    };
    r
}

async fn fetch_via_cl(cl: &skim_cl::ClNode, saved: &Saved, head: &BlockRef) -> anyhow::Result<Vec<(alloy_consensus::Header, BlockWrites)>> {
    let head_slot = head.timestamp.saturating_sub(skim_cl::node::SEPOLIA_GENESIS_TIME) / 12;
    let lowest_needed = saved.number + 1;
    let mut got: std::collections::BTreeMap<u64, (alloy_consensus::Header, BlockWrites)> = Default::default();
    let mut trace = CatchupTrace::default();
    // Slots >= blocks (some slots are empty), so walk back until the lowest needed block shows up.
    let r = walk_envelopes(cl, head_slot, MAX_REPLAY * 2, head.number - saved.number, &mut trace, |h, w| {
        let n = h.number;
        if n >= lowest_needed && n <= head.number {
            got.insert(n, (h, w));
        }
        // Windows complete out of order: done only when every block of the gap is here.
        got.len() as u64 == head.number - lowest_needed + 1
    })
    .await;
    tracing::info!(gap = head.number - saved.number, summary = %trace.summary(), "restart catch-up via consensus peers");
    r?;
    Ok(got.into_values().collect())
}

type Replayed = (Vec<(skim_bal::StateKey, alloy_primitives::U256)>, Vec<(B256, alloy_primitives::Bytes)>);

/// The whole check, independent of where the blocks came from: the headers start right after
/// the snapshot block, each links to the previous by hash (the first to the snapshot's hash, so
/// a snapshot on an orphaned fork fails here), and the last one is the current head. Only then
/// are the writes replayed. BALs were already checked against their headers when decoded.
pub fn verify_and_replay(saved: &Saved, blocks: &[(alloy_consensus::Header, BlockWrites)], head: &BlockRef) -> anyhow::Result<Replayed> {
    let mut prev = saved.hash;
    let mut number = saved.number;
    for (h, _) in blocks {
        anyhow::ensure!(h.number == number + 1, "block {} follows {number}", h.number);
        anyhow::ensure!(h.parent_hash == prev, "block {} does not link to the snapshot chain", h.number);
        prev = h.hash_slow();
        number = h.number;
    }
    anyhow::ensure!(number == head.number && prev == head.hash, "replayed chain ends at {number}/{prev}, not the head {}/{}", head.number, head.hash);
    let writes: Vec<BlockWrites> = blocks.iter().map(|(_, w)| w.clone()).collect();
    Ok(skim_state::persist::replay(saved, &writes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_primitives::{Address, U256};
    use skim_bal::StateKey;

    fn chain(from_parent: B256, from: u64, n: u64, salt: u64) -> Vec<Header> {
        let mut out = Vec::new();
        let mut parent = from_parent;
        for i in 0..n {
            let h = Header { number: from + i, parent_hash: parent, gas_limit: salt, ..Default::default() };
            parent = h.hash_slow();
            out.push(h);
        }
        out
    }

    fn slot() -> StateKey {
        StateKey::Storage(Address::repeat_byte(1), U256::from(1))
    }

    fn head_of(h: &Header) -> BlockRef {
        BlockRef { number: h.number, hash: h.hash_slow(), parent_hash: h.parent_hash, state_root: B256::ZERO, timestamp: 0 }
    }

    #[test]
    fn replays_a_linked_chain() {
        let snap_hash = B256::repeat_byte(5);
        let hs = chain(snap_hash, 11, 3, 1);
        let w = |v: u64| BlockWrites { writes: vec![(slot(), U256::from(v))], ..Default::default() };
        let blocks: Vec<_> = hs.iter().cloned().zip([w(2), BlockWrites::default(), w(3)]).collect();
        let saved = Saved { number: 10, hash: snap_hash, values: vec![(slot(), U256::from(1))], code: vec![] };
        let (v, _) = verify_and_replay(&saved, &blocks, &head_of(&hs[2])).unwrap();
        assert_eq!(v, vec![(slot(), U256::from(3))]);
    }

    #[test]
    fn a_snapshot_from_an_orphaned_fork_is_discarded() {
        // The snapshot was taken at block 10 on a branch that was reorged out while skim was
        // down: the canonical 11 doesn't have it as parent.
        let canonical_10 = B256::repeat_byte(5);
        let orphaned_10 = B256::repeat_byte(6);
        let hs = chain(canonical_10, 11, 3, 1);
        let blocks: Vec<_> = hs.iter().cloned().map(|h| (h, BlockWrites::default())).collect();
        let saved = Saved { number: 10, hash: orphaned_10, values: vec![(slot(), U256::from(1))], code: vec![] };
        assert!(verify_and_replay(&saved, &blocks, &head_of(&hs[2])).is_err());
    }

    #[test]
    fn a_chain_that_misses_the_head_or_skips_blocks_is_discarded() {
        let snap_hash = B256::repeat_byte(5);
        let hs = chain(snap_hash, 11, 3, 1);
        let blocks: Vec<_> = hs.iter().cloned().map(|h| (h, BlockWrites::default())).collect();
        let saved = Saved { number: 10, hash: snap_hash, values: vec![], code: vec![] };
        let other_head = chain(snap_hash, 11, 3, 2); // same numbers, different blocks
        assert!(verify_and_replay(&saved, &blocks, &head_of(&other_head[2])).is_err());
        let gapped: Vec<_> = vec![blocks[0].clone(), blocks[2].clone()];
        assert!(verify_and_replay(&saved, &gapped, &head_of(&hs[2])).is_err());
    }

    #[test]
    fn a_restore_fills_only_keys_a_fresh_fetch_has_not_resolved() {
        // The restore and the first subscription passes run concurrently.
        let store = Arc::new(Store::new(64));
        let hs = chain(B256::ZERO, 1, 1, 1);
        let at = head_of(&hs[0]);
        store.apply_block(at, Arc::new(BlockWrites::default())).unwrap();
        let fetched = StateKey::Storage(Address::repeat_byte(1), U256::from(2));
        store.insert_base(&at, fetched, U256::from(20)).unwrap(); // the fresh fetch got there first
        let n = install(&store, &at, &[(fetched, U256::from(99)), (slot(), U256::from(7))], &[]).unwrap();
        assert_eq!(n, 1);
        let snap = store.snapshot(&at).unwrap();
        assert_eq!(snap.get(&fetched).unwrap(), skim_state::StateValue::Known(U256::from(20)));
        assert_eq!(snap.get(&slot()).unwrap(), skim_state::StateValue::Known(U256::from(7)));
    }
}

/// How long restored values stay cached before a subscription must claim them.
const RESTORED_TTL_BLOCKS: u64 = 64;

fn install(store: &Arc<Store>, at: &BlockRef, values: &[(skim_bal::StateKey, alloy_primitives::U256)], code: &[(B256, alloy_primitives::Bytes)]) -> anyhow::Result<usize> {
    for (_, c) in code {
        store.insert_code(c.clone());
    }
    // The restore runs alongside the first subscription passes, which fetch fresh state: a
    // key already known at `at` came from a verified fetch (or a BAL) and is left alone.
    let snap = store.snapshot(at)?;
    let mut n = 0;
    for (k, v) in values {
        if matches!(snap.get(k), Ok(skim_state::StateValue::Known(_))) {
            continue;
        }
        // `at` must still be in the window; insert_base replays any later blocks itself.
        // Provisional: the next subscription pass re-subscribes the keys the current config
        // still needs, and the rest (keys of removed subscriptions, anything subscribed by an
        // older version) expire instead of staying subscribed forever.
        store.insert_base_with(at, *k, *v, Retention::Ttl(RESTORED_TTL_BLOCKS))?;
        n += 1;
    }
    Ok(n)
}
