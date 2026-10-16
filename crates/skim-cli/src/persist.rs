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
            // This run's peers first, then the previously saved ones (peers that were full today
            // may have room next time), up to MAX_SAVED_PEERS.
            let mut seen = std::collections::HashSet::new();
            let text: String = peers
                .into_iter()
                .chain(load_peers(&p))
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

async fn restore_inner(store: &Store, net: &ElNet, saved: &Saved, head: &BlockRef) -> anyhow::Result<usize> {
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
async fn fetch_via_cl(cl: &skim_cl::ClNode, saved: &Saved, head: &BlockRef) -> anyhow::Result<Vec<(alloy_consensus::Header, BlockWrites)>> {
    const CHUNK: u64 = 32;
    let deadline = Instant::now() + std::time::Duration::from_secs(60);
    let head_slot = head.timestamp.saturating_sub(skim_cl::node::SEPOLIA_GENESIS_TIME) / 12;
    let lowest_needed = saved.number + 1;
    let mut got: std::collections::BTreeMap<u64, (alloy_consensus::Header, BlockWrites)> = Default::default();
    let mut end = head_slot;
    // Slots >= blocks (some slots are empty), so walk back until the lowest needed block shows up.
    while got.keys().next().is_none_or(|n| *n > lowest_needed) {
        anyhow::ensure!(Instant::now() < deadline, "timed out at slot {end}");
        anyhow::ensure!(head_slot - end <= MAX_REPLAY * 2, "walked back {} slots without reaching block {lowest_needed}", head_slot - end);
        let start = end.saturating_sub(CHUNK - 1);
        let mut chunks = None;
        for _ in 0..4 {
            match cl.envelopes_by_range(start, end - start + 1).await {
                Ok(c) if !c.is_empty() => {
                    chunks = Some(c);
                    break;
                }
                Ok(_) => {}
                Err(e) => tracing::debug!("envelopes {start}..={end}: {e}"),
            }
        }
        let chunks = chunks.ok_or_else(|| anyhow::anyhow!("no peer served envelopes for slots {start}..={end}"))?;
        for ssz in chunks {
            let env = skim_cl::ssz::Envelope::parse(&ssz)?;
            let v = skim_cl::verify::verify_payload(&env);
            anyhow::ensure!(v.ok, "envelope for block {} does not hash to its block_hash", env.payload.block_number);
            let n = v.header.number;
            if n >= lowest_needed && n <= head.number {
                let bal = skim_bal::decode(env.payload.block_access_list)?;
                got.insert(n, (v.header, BlockWrites::from_bal(&bal)));
            }
        }
        if start == 0 {
            break;
        }
        end = start - 1;
    }
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
}

fn install(store: &Store, at: &BlockRef, values: &[(skim_bal::StateKey, alloy_primitives::U256)], code: &[(B256, alloy_primitives::Bytes)]) -> anyhow::Result<usize> {
    for (_, c) in code {
        store.insert_code(c.clone());
    }
    let mut n = 0;
    for (k, v) in values {
        // `at` must still be in the window; insert_base replays any later blocks itself.
        store.insert_base_with(at, *k, *v, Retention::Subscribed)?;
        n += 1;
    }
    Ok(n)
}
