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
    /// Called for every header applied (gossip or EL replay), before its BAL is applied.
    pub on_header: Box<dyn Fn(&Header) + Send + Sync>,
    /// How far back to look for a common ancestor.
    pub lookback: u64,
    /// Consensus peers: the missing blocks' payload envelopes (with their BALs) when no EL peer
    /// serves them. A missed gossip payload must not stall the follower until an eth/71 peer
    /// shows up (measured: 5 minutes with only crawlers connected on the EL side).
    pub cl: Option<skim_cl::ClNode>,
}

impl Chain {
    fn apply(&self, h: &Header, writes: Arc<BlockWrites>) -> Result<StateReady, skim_state::StateError> {
        (self.on_header)(h);
        self.store.apply_block(block_ref(h), writes)
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
            match self.reconcile_via_cl(cl, b, head.number).await {
                Ok(r) => return Ok(r),
                Err(e) => tracing::debug!(number = b.number, "gap fill via consensus peers: {e}"),
            }
        }
        Err(Reconcile::Defer("no EL peer served a linked header chain + BALs yet".into()))
    }

    /// Fills `b`'s missing ancestors from consensus peers: walks payload envelopes back from
    /// `b`'s slot until the collected blocks link (by hash) from a block the store holds to `b`'s
    /// parent, then rolls back to that block and applies them in order.
    async fn reconcile_via_cl(&self, cl: &skim_cl::ClNode, b: &BlockRef, head: u64) -> anyhow::Result<(u64, u64)> {
        let slot = b.timestamp.saturating_sub(skim_cl::node::SEPOLIA_GENESIS_TIME) / 12;
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
