//! Places verified blocks on the selected chain (PROJECT.md §6.2), shared by `follow` and `run`.
//!
//! - **Child of the head:** apply.
//! - **One-block fork:** a block whose parent is the head's parent switches branches.
//! - **Anything else** (a gap, a deeper fork, or a parent we don't know): reconcile with the EL
//!   chain. Fetch recent headers, find the newest block both chains agree on, roll back only to
//!   there, and replay the EL blocks (eth/71 BALs) up to the new block's parent. The store is
//!   wiped only if no common ancestor exists in the window.

use std::sync::Arc;

use alloy_consensus::Header;
use alloy_primitives::B256;
use reth_eth_wire_types::BlockHashOrNumber;
use skim_bal::BlockWrites;
use skim_el::ElNet;
use skim_state::{BlockRef, StateReady, Store};

pub fn block_ref(h: &Header) -> BlockRef {
    BlockRef { number: h.number, hash: h.hash_slow(), parent_hash: h.parent_hash, state_root: h.state_root, timestamp: h.timestamp }
}

#[derive(Debug)]
pub enum Placed {
    Applied { ready: StateReady, reorged_to: Option<u64>, filled: u64 },
    Duplicate,
    /// Couldn't connect the block yet (EL peers behind or unreachable); try again later.
    Deferred(String),
    /// No common ancestor in the window: the store was reset and this block anchors anew.
    Reanchored(#[allow(dead_code)] StateReady),
}

pub struct Chain {
    pub store: Arc<Store>,
    pub net: ElNet,
    /// Called for every header about to be applied (gossip or EL replay), before its BAL is:
    /// what executing the block's state needs (header caches). Not a commitment: the apply can
    /// still fail.
    pub on_header: Box<dyn Fn(&Header) + Send + Sync>,
    /// Called once the store has applied the block: it's canonical now (chain-data index,
    /// transaction inclusion). Never for a block the store refused.
    pub on_applied: Box<dyn Fn(&Header) + Send + Sync>,
    /// How far back to look for a common ancestor.
    pub lookback: u64,
    /// Consensus peers: the missing blocks' payload envelopes (with their BALs) when no EL peer
    /// serves them. A missed gossip payload must not stall the follower until an eth/71 peer
    /// shows up (measured: 5 minutes with only crawlers connected on the EL side).
    pub cl: Option<Arc<dyn crate::persist::EnvelopeSource>>,
}

impl Chain {
    fn apply(&self, h: &Header, writes: Arc<BlockWrites>) -> Result<StateReady, skim_state::StateError> {
        (self.on_header)(h);
        let r = self.store.apply_block(block_ref(h), writes);
        if r.is_ok() {
            (self.on_applied)(h);
        }
        r
    }

    pub async fn place(&self, h: Header, writes: Arc<BlockWrites>) -> Placed {
        let b = block_ref(&h);
        let Some(head) = self.store.head() else {
            return self.apply(&h, writes).map_or_else(|e| Placed::Deferred(e.to_string()), Placed::Reanchored);
        };
        if self.store.block(b.number).is_some_and(|x| x.hash == b.hash) {
            return Placed::Duplicate;
        }
        if b.parent_hash == head.hash {
            return match self.apply(&h, writes) {
                Ok(ready) => Placed::Applied { ready, reorged_to: None, filled: 0 },
                Err(e) => Placed::Deferred(e.to_string()),
            };
        }
        if b.number <= head.number
            && b.number > 0
            && self.store.block(b.number - 1).is_some_and(|p| p.hash == b.parent_hash)
        {
            self.store.rollback_to(b.number - 1);
            return match self.apply(&h, writes) {
                Ok(ready) => Placed::Applied { ready, reorged_to: Some(b.number - 1), filled: 0 },
                Err(e) => Placed::Deferred(e.to_string()),
            };
        }
        match self.reconcile(&b).await {
            Ok((ancestor, filled)) => match self.apply(&h, writes) {
                Ok(ready) => Placed::Applied { ready, reorged_to: (ancestor < head.number).then_some(ancestor), filled },
                Err(e) => Placed::Deferred(e.to_string()),
            },
            Err(Reconcile::NoAncestor) => {
                tracing::warn!(number = b.number, "no common ancestor in window; re-anchoring");
                self.store.rollback_to(0);
                // rollback_to(0) keeps block 0 entries only; start fresh at this block.
                match self.apply(&h, writes) {
                    Ok(r) => Placed::Reanchored(r),
                    Err(e) => Placed::Deferred(e.to_string()),
                }
            }
            Err(Reconcile::Defer(why)) => Placed::Deferred(why),
        }
    }

    /// Makes the store's head the EL-canonical parent of `b`. Returns the common ancestor and
    /// how many EL blocks were replayed.
    async fn reconcile(&self, b: &BlockRef) -> Result<(u64, u64), Reconcile> {
        let head = self.store.head().ok_or(Reconcile::NoAncestor)?;
        let from = head.number.min(b.number.saturating_sub(1)).saturating_sub(self.lookback);
        let count = b.number.saturating_sub(from);
        if count == 0 {
            return Err(Reconcile::Defer("nothing to reconcile".into()));
        }
        let peers: Vec<_> = self.net.peers().into_iter().filter(|(_, p)| p.eth_version >= 71).map(|(id, _)| id).collect();
        for peer in peers {
            let Ok(hs) = self.net.headers(peer, BlockHashOrNumber::Number(from), count).await.0 else { continue };
            // Must be a linked chain ending at b's parent.
            let linked = hs.windows(2).all(|w| w[1].parent_hash == w[0].hash_slow());
            if !linked || hs.last().map(|h| h.hash_slow()) != Some(b.parent_hash) {
                continue;
            }
            let ancestor = hs.iter().rev().find(|h| self.store.block(h.number).is_some_and(|x| x.hash == h.hash_slow()));
            let Some(ancestor) = ancestor.map(|h| h.number) else { return Err(Reconcile::NoAncestor) };
            let replay: Vec<&Header> = hs.iter().filter(|h| h.number > ancestor).collect();
            let hashes: Vec<B256> = replay.iter().map(|h| h.hash_slow()).collect();
            let bals = if hashes.is_empty() {
                Vec::new()
            } else {
                match self.net.bals(peer, hashes).await.0 {
                    Ok(b) => b.0,
                    Err(_) => continue,
                }
            };
            if bals.len() != replay.len() || bals.iter().any(|x| x.is_none()) {
                continue;
            }
            // Decode and verify everything before touching the store.
            let mut decoded = Vec::with_capacity(replay.len());
            for (h, raw) in replay.iter().zip(&bals) {
                match skim_bal::decode_verified(raw.as_ref().unwrap(), h.block_access_list_hash.unwrap_or_default()) {
                    Ok(bal) => decoded.push(Arc::new(BlockWrites::from_bal(&bal))),
                    Err(_) => break,
                }
            }
            if decoded.len() != replay.len() {
                continue;
            }
            self.store.rollback_to(ancestor);
            for (h, w) in replay.iter().zip(decoded) {
                if let Err(e) = self.apply(h, w) {
                    return Err(Reconcile::Defer(format!("replay failed at {}: {e}", h.number)));
                }
            }
            return Ok((ancestor, replay.len() as u64));
        }
        // No EL peer could: the same blocks from consensus peers (payload envelopes carry the
        // header fields and the BAL, each envelope checked against its block hash).
        if let Some(cl) = &self.cl {
            match self.reconcile_via_cl(cl.as_ref(), b, head.number).await {
                Ok(r) => return Ok(r),
                Err(e) => tracing::debug!(number = b.number, "gap fill via consensus peers: {e}"),
            }
        }
        Err(Reconcile::Defer("no EL peer served a linked header chain + BALs yet".into()))
    }

    /// Fills `b`'s missing ancestors from consensus peers: walks payload envelopes back from
    /// `b`'s slot until the collected blocks link (by hash) from a block the store holds to `b`'s
    /// parent, then rolls back to that block and applies them in order.
    async fn reconcile_via_cl(&self, cl: &dyn crate::persist::EnvelopeSource, b: &BlockRef, head: u64) -> anyhow::Result<(u64, u64)> {
        let slot = skim_cl::network::current().slot_at(b.timestamp);
        let mut got: std::collections::BTreeMap<u64, (Header, BlockWrites)> = Default::default();
        let mut found: Option<u64> = None;
        let mut trace = crate::persist::CatchupTrace::default();
        let expected = b.number.saturating_sub(head).max(1);
        crate::persist::walk_envelopes(cl, slot.saturating_sub(1), self.lookback * 2 + 8, expected, &mut trace, |h, w| {
            if h.number < b.number {
                got.insert(h.number, (h, w));
            }
            // Linked from a stored block up to b's parent?
            let mut want = b.parent_hash;
            for n in (0..b.number).rev() {
                let Some((hd, _)) = got.get(&n) else { return false };
                if hd.hash_slow() != want {
                    return false;
                }
                if self.store.block(n - 1).is_some_and(|p| p.hash == hd.parent_hash) {
                    found = Some(n - 1);
                    return true;
                }
                want = hd.parent_hash;
            }
            false
        })
        .await?;
        let ancestor = found.ok_or_else(|| anyhow::anyhow!("envelopes didn't link to a stored block"))?;
        self.store.rollback_to(ancestor);
        let mut n_applied = 0;
        for (_, (h, w)) in got.range(ancestor + 1..b.number) {
            self.apply(h, Arc::new(w.clone())).map_err(|e| anyhow::anyhow!("replay failed at {}: {e}", h.number))?;
            n_applied += 1;
        }
        tracing::info!(from = ancestor + 1, to = b.number - 1, "gap filled from consensus peers");
        Ok((ancestor, n_applied))
    }
}

enum Reconcile {
    NoAncestor,
    Defer(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use std::collections::BTreeMap;
    use std::time::Duration;

    const GENESIS: u64 = skim_cl::node::SEPOLIA_GENESIS_TIME;
    /// Block `n` sits in slot `SLOT0 + n`.
    const SLOT0: u64 = 1_000_000;

    /// A `SignedExecutionPayloadEnvelope` (SSZ, Gloas layout) for block `number` on top of
    /// `parent`, with an empty BAL; its `block_hash` is what `verify_payload` rebuilds. Returns
    /// the rebuilt header and the bytes.
    fn envelope(parent: B256, number: u64, salt: u8) -> (Header, Vec<u8>) {
        let slot = SLOT0 + number;
        let timestamp = GENESIS + slot * 12;
        let bal: &[u8] = &[0xc0]; // RLP of an empty list: an empty, valid BAL
        let mut payload = vec![0u8; 540];
        payload[0..32].copy_from_slice(parent.as_slice());
        payload[32..52].copy_from_slice(&[salt; 20]); // fee recipient: tells forks apart
        payload[404..412].copy_from_slice(&number.to_le_bytes());
        payload[412..420].copy_from_slice(&30_000_000u64.to_le_bytes());
        payload[428..436].copy_from_slice(&timestamp.to_le_bytes());
        payload[440..472].copy_from_slice(&U256::from(7u64).to_le_bytes::<32>());
        payload[532..540].copy_from_slice(&slot.to_le_bytes());
        // Variable fields: extra_data, transactions, withdrawals (all empty), then the BAL.
        for off in [436, 504, 508, 528] {
            payload[off..off + 4].copy_from_slice(&540u32.to_le_bytes());
        }
        payload.extend_from_slice(bal);
        let requests = [12u32.to_le_bytes(), 12u32.to_le_bytes(), 12u32.to_le_bytes()].concat(); // three empty lists
        let mut msg = vec![0u8; 80];
        msg[0..4].copy_from_slice(&80u32.to_le_bytes());
        msg[4..8].copy_from_slice(&((80 + payload.len()) as u32).to_le_bytes());
        msg.extend_from_slice(&payload);
        msg.extend_from_slice(&requests);
        let mut signed = 100u32.to_le_bytes().to_vec();
        signed.extend_from_slice(&[0u8; 96]);
        signed.extend_from_slice(&msg);
        // block_hash = the hash of the header rebuilt from these fields.
        let header = {
            let env = skim_cl::ssz::Envelope::parse(&signed).expect("valid layout");
            skim_cl::verify::rebuild_header(&env)
        };
        let at = 100 + 80 + 472;
        signed[at..at + 32].copy_from_slice(header.hash_slow().as_slice());
        let env = skim_cl::ssz::Envelope::parse(&signed).unwrap();
        assert!(skim_cl::verify::verify_payload(&env).ok);
        (header, signed)
    }

    /// `count` linked blocks from `from`, on top of `parent`.
    fn fork(parent: B256, from: u64, count: u64, salt: u8) -> Vec<(Header, Vec<u8>)> {
        let mut out = Vec::new();
        let mut p = parent;
        for n in from..from + count {
            let (h, b) = envelope(p, n, salt);
            p = h.hash_slow();
            out.push((h, b));
        }
        out
    }

    /// Consensus peers that serve these envelopes by slot (nothing else, no network).
    struct Envelopes(BTreeMap<u64, Vec<u8>>);

    impl crate::persist::EnvelopeSource for Envelopes {
        fn envelopes_by_range_from(
            &self,
            start_slot: u64,
            count: u64,
            _timeout: Duration,
        ) -> futures::future::BoxFuture<'_, (Option<String>, anyhow::Result<Vec<Vec<u8>>>)> {
            let out = self.0.range(start_slot..start_slot + count).map(|(_, b)| b.clone()).collect();
            Box::pin(async move { (Some("test".into()), Ok(out)) })
        }
    }

    async fn chain_at(head: &Header, envelopes: &[(Header, Vec<u8>)], with_cl: bool) -> Chain {
        let store = Store::new(64);
        store.apply_block(block_ref(head), Default::default()).unwrap();
        // A real node restricted to trusted peers with none trusted: never any EL peer.
        let net = ElNet::start_sepolia(0, vec![], true).await.expect("local node");
        let source = Envelopes(envelopes.iter().map(|(h, b)| (h.slot_number.unwrap(), b.clone())).collect());
        Chain {
            store,
            net,
            on_header: Box::new(|_| {}),
            on_applied: Box::new(|_| {}),
            lookback: 4,
            cl: with_cl.then(|| Arc::new(source) as Arc<dyn crate::persist::EnvelopeSource>),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_gap_with_no_el_peers_is_filled_from_consensus_peers() {
        // A100 stored; A104 arrives (gossip missed A101..A103); no EL peer; consensus peers
        // serve A101..A103.
        let a = fork(B256::repeat_byte(0x99), 100, 5, 0xa);
        let chain = chain_at(&a[0].0, &a[1..4], true).await;
        match chain.place(a[4].0.clone(), Default::default()).await {
            Placed::Applied { filled, reorged_to, .. } => assert_eq!((filled, reorged_to), (3, None)),
            other => panic!("expected the gap filled, got {other:?}"),
        }
        assert_eq!(chain.store.head().map(|h| h.hash), Some(a[4].0.hash_slow()));
        for (h, _) in &a[1..4] {
            assert_eq!(chain.store.block(h.number).map(|b| b.hash), Some(h.hash_slow()), "block {}", h.number);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn envelopes_from_another_fork_are_not_applied() {
        // A100 stored; A104 arrives; consensus peers only have B101..B103, a fork that doesn't
        // lead to A104. Nothing is applied: the block waits.
        let a = fork(B256::repeat_byte(0x99), 100, 5, 0xa);
        let b = fork(a[0].0.hash_slow(), 101, 3, 0xb);
        let chain = chain_at(&a[0].0, &b, true).await;
        let r = chain.place(a[4].0.clone(), Default::default()).await;
        assert!(matches!(r, Placed::Deferred(_)), "{r:?}");
        assert_eq!(chain.store.head().map(|h| h.number), Some(100));
        assert!(chain.store.block(101).is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_block_the_store_refuses_is_never_reported_as_applied() {
        // Review: the chain-data callback (canonical index, transaction inclusion) ran before
        // the store applied the block, so a refused block was canonical there but not here.
        let a = fork(B256::repeat_byte(0x99), 100, 2, 0xa);
        let mut chain = chain_at(&a[0].0, &[], false).await;
        let applied = Arc::new(std::sync::Mutex::new(Vec::new()));
        let rec = applied.clone();
        chain.on_applied = Box::new(move |h| rec.lock().unwrap().push(h.number));
        // A child of the head by hash but at the wrong height: the store refuses it.
        let mut bad = a[1].0.clone();
        bad.number = 102;
        assert!(matches!(chain.place(bad, Default::default()).await, Placed::Deferred(_)));
        assert!(applied.lock().unwrap().is_empty(), "not reported: the store's head is still 100");
        assert_eq!(chain.store.head().map(|h| h.number), Some(100));
        // The real child: applied, then reported.
        assert!(matches!(chain.place(a[1].0.clone(), Default::default()).await, Placed::Applied { .. }));
        assert_eq!(*applied.lock().unwrap(), vec![101]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn without_consensus_peers_the_gap_waits() {
        // The same gap with no envelope source: deferred (what the fallback replaces).
        let a = fork(B256::repeat_byte(0x99), 100, 5, 0xa);
        let chain = chain_at(&a[0].0, &a[1..4], false).await;
        assert!(matches!(chain.place(a[4].0.clone(), Default::default()).await, Placed::Deferred(_)));
    }
}
