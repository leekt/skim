//! Consensus verification for `skim run`: which execution blocks a supermajority of the sync
//! committee has signed off on (`safe`), and which are finalized (`finalized`).
//!
//! Trust starts from a checkpoint block root: `--checkpoint`, else the last finalized root
//! this node verified itself (saved in the data dir), else a checkpoint-sync URL. From there,
//! every update must carry a valid sync-committee signature. The next period's committee is
//! learned from a `LightClientUpdate` signed by a held committee (proven against its attested
//! state), and becomes current once finality reaches that period.

use std::sync::Arc;

use alloy_primitives::B256;
use parking_lot::{Mutex, RwLock};
use skim_cl::{ClNode, light};
use skim_state::{Store, persist::Persist};

/// A verified execution block: the hash the light client verified, and where it sits on the
/// chain skim follows. Never one without the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct VerifiedBlock {
    pub number: u64,
    pub hash: B256,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct LightStatus {
    /// Sync committee period skim holds (None until bootstrapped).
    pub period: Option<u64>,
    /// Highest execution block a supermajority of the sync committee vouched for, found on the
    /// selected chain: number and hash, always set together (`safe`).
    pub verified_block: Option<VerifiedBlock>,
    /// The latest attested execution block hash (may not be on skim's chain yet).
    pub verified_hash: Option<B256>,
    /// Execution block of the latest finalized beacon block skim verified, found on the
    /// selected chain (`finalized`).
    pub finalized_block: Option<VerifiedBlock>,
    pub finalized_hash: Option<B256>,
    pub updates_verified: u64,
    pub updates_rejected: u64,
    pub last_error: Option<String>,
    pub trusted_from: Option<String>,
}

pub struct Light {
    committee: Mutex<Option<light::Committees>>,
    fetching_next: std::sync::atomic::AtomicBool,
    status: RwLock<LightStatus>,
    cl: ClNode,
    persist: Option<Arc<Persist>>,
    bootstrapping: std::sync::atomic::AtomicBool,
}

const FINALIZED_ROOT_META: &str = "lc_finalized_root";

impl Light {
    pub fn new(cl: ClNode, persist: Option<Arc<Persist>>) -> Arc<Self> {
        Arc::new(Self {
            committee: Mutex::new(None),
            fetching_next: Default::default(),
            status: Default::default(),
            cl,
            persist,
            bootstrapping: Default::default(),
        })
    }

    pub fn status(&self) -> LightStatus {
        self.status.read().clone()
    }

    /// Picks the trusted root and bootstraps in the background, retrying across peers.
    pub fn start(self: &Arc<Self>, checkpoint: Option<B256>, checkpoint_url: String) {
        let me = self.clone();
        tokio::spawn(async move {
            let saved = me.persist.as_ref().and_then(|p| p.get_meta(FINALIZED_ROOT_META).ok().flatten()).filter(|v| v.len() == 32);
            let (root, from) = match (checkpoint, saved) {
                (Some(r), _) => (r, "--checkpoint".to_string()),
                (None, Some(v)) => (B256::from_slice(&v), "finalized root verified in a previous run".to_string()),
                (None, None) => match fetch_checkpoint(&checkpoint_url).await {
                    Ok(r) => (r, format!("checkpoint sync URL {checkpoint_url}")),
                    Err(e) => {
                        me.status.write().last_error = Some(format!("no checkpoint: {e}"));
                        tracing::warn!("light client: no checkpoint root ({e}); consensus verification is off");
                        return;
                    }
                },
            };
            me.status.write().trusted_from = Some(from.clone());
            tracing::info!(%root, %from, "light client: bootstrapping");
            me.bootstrap(root).await;
        });
    }

    async fn bootstrap(self: &Arc<Self>, root: B256) {
        if self.bootstrapping.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        for attempt in 0..120 {
            match self.cl.light_client_bootstrap(root.0).await.and_then(|b| light::bootstrap(&b, root).map_err(Into::into)) {
                Ok((header, committee)) => {
                    tracing::info!(slot = header.beacon.slot, period = committee.period, "light client: sync committee verified");
                    self.status.write().period = Some(committee.period);
                    *self.committee.lock() = Some(light::Committees::new(committee));
                    self.fetch_next_committee();
                    break;
                }
                Err(e) => {
                    if attempt % 10 == 0 {
                        tracing::debug!("light client bootstrap: {e}");
                    }
                    self.status.write().last_error = Some(format!("bootstrap: {e}"));
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
            }
        }
        self.bootstrapping.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// A gossiped light-client update. Verified against the committee; results are stored as
    /// execution block hashes and resolved to numbers by [`Self::on_block`].
    pub async fn on_gossip(self: &Arc<Self>, topic: &str, data: &[u8], store: &Store) {
        let fv = skim_cl::node::SEPOLIA_GLOAS_VERSION;
        tracing::debug!(topic, len = data.len(), "light client update received");
        let res = {
            let c = self.committee.lock();
            let Some(c) = c.as_ref() else { return };
            match topic {
                "light_client_optimistic_update" => light::optimistic_update(c, data, fv).map(|(a, _)| (a, None)),
                "light_client_finality_update" => light::finality_update(c, data, fv).map(|(a, f)| (a, Some(f))),
                _ => return,
            }
        };
        match res {
            Ok((attested, finalized)) => {
                {
                    let mut st = self.status.write();
                    st.updates_verified += 1;
                    st.verified_hash = Some(attested.execution_block_hash);
                    if let Some(f) = &finalized {
                        st.finalized_hash = Some(f.execution_block_hash);
                    }
                }
                if let Some(f) = finalized {
                    let root = B256::from(f.beacon.root());
                    if let Some(p) = &self.persist {
                        let _ = p.set_meta(FINALIZED_ROOT_META, root.as_slice());
                    }
                    // Finality in the next period: its (already authenticated) committee becomes
                    // current, and the one after it is fetched.
                    let rotated = self.committee.lock().as_mut().and_then(|c| c.advance(f.beacon.slot).then_some(c.current.period));
                    if let Some(p) = rotated {
                        tracing::info!(period = p, "light client: sync committee period advanced");
                        self.status.write().period = Some(p);
                        self.fetch_next_committee();
                    }
                }
                if self.committee.lock().as_ref().is_some_and(|c| c.next.is_none()) {
                    self.fetch_next_committee();
                }
                self.on_block(store);
            }
            Err(e) => {
                let mut st = self.status.write();
                st.updates_rejected += 1;
                st.last_error = Some(e.to_string());
                tracing::debug!(topic, "light client update rejected: {e}");
            }
        }
    }

    /// Catches the committees up to the live period from `LightClientUpdatesByRange`, applying
    /// each update in order (`light::apply_updates`): learns the next committee, applies
    /// historical finality, rotates. A checkpoint several periods old ends at the live period
    /// with its successor known, so live gossip verifies. In the background, one run at a time.
    fn fetch_next_committee(self: &Arc<Self>) {
        if self.fetching_next.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let fv = skim_cl::node::SEPOLIA_GLOAS_VERSION;
            for attempt in 0..120 {
                let live = skim_cl::NodeConfig::sepolia(0).current_slot() / light::SLOTS_PER_PERIOD;
                let Some((period, has_next)) = me.committee.lock().as_ref().map(|c| (c.current.period, c.next.is_some())) else { break };
                if period >= live && has_next {
                    break;
                }
                let count = (live.saturating_sub(period) + 1).min(128);
                match me.cl.light_client_updates(period, count).await {
                    Ok(updates) => {
                        let r = {
                            let mut g = me.committee.lock();
                            let Some(cs) = g.as_mut() else { break };
                            light::apply_updates(cs, &updates, fv)
                        };
                        if r.rotations > 0 || r.applied > 0 {
                            let (p, next) = me.committee.lock().as_ref().map(|c| (c.current.period, c.next.as_ref().map(|n| n.period))).unwrap_or_default();
                            tracing::info!(applied = r.applied, rotations = r.rotations, period = p, ?next, "light client: caught up from updates");
                            me.status.write().period = Some(p);
                        }
                        if let Some(f) = r.finalized
                            && let Some(ps) = &me.persist
                        {
                            let _ = ps.set_meta(FINALIZED_ROOT_META, &f.beacon.root());
                        }
                        if let Some(e) = r.stopped {
                            tracing::debug!("light client updates: stopped at an update that doesn't verify: {e}");
                        }
                        // Progress (a rotation or a newly learned committee): more periods may follow
                        // right away. Anything else waits before asking again.
                        let after = me.committee.lock().as_ref().map(|c| (c.current.period, c.next.is_some()));
                        if after != Some((period, has_next)) {
                            continue;
                        }
                    }
                    Err(e) if attempt % 10 == 0 => tracing::debug!("light client updates: {e}"),
                    Err(_) => {}
                }
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
            me.fetching_next.store(false, std::sync::atomic::Ordering::SeqCst);
        });
    }

    /// Resolves verified hashes to block numbers in the store's chain (blocks may arrive after
    /// the update that vouches for them).
    pub fn on_block(&self, store: &Store) {
        let (v, f, cur_v, cur_f) = {
            let st = self.status.read();
            (st.verified_hash, st.finalized_hash, st.verified_block, st.finalized_block)
        };
        let next_v = advance(store, cur_v, v);
        let next_f = advance(store, cur_f, f);
        let mut st = self.status.write();
        st.verified_block = next_v;
        st.finalized_block = next_f;
    }
}

/// The verified block to report after a new block: the newly attested hash once it is on the
/// chain (if it is not lower), else the current one while it is still on the chain. A verified
/// block whose height now holds a different block (a reorg replaced it) is dropped, never kept
/// as a bare height.
fn advance(store: &Store, current: Option<VerifiedBlock>, attested: Option<B256>) -> Option<VerifiedBlock> {
    let still_on_chain = |b: &VerifiedBlock| store.block(b.number).is_none_or(|x| x.hash == b.hash);
    let current = current.filter(still_on_chain);
    let candidate = attested.and_then(|h| store.number_of_hash(&h).map(|number| VerifiedBlock { number, hash: h }));
    match (current, candidate) {
        (Some(c), Some(n)) if n.number < c.number => Some(c),
        (_, Some(n)) => Some(n),
        (c, None) => c,
    }
}

/// Resolves a verified block to the block skim holds at that height, only if it is the same
/// block (by hash). Otherwise `None`: the caller refuses instead of substituting.
pub fn resolve(store: &Store, b: &VerifiedBlock) -> Option<skim_state::BlockRef> {
    store
        .block(b.number)
        .or_else(|| store.archived_block(b.number).map(|(x, _)| x))
        .filter(|x| x.hash == b.hash)
}

async fn fetch_checkpoint(url: &str) -> anyhow::Result<B256> {
    let v: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/eth/v1/beacon/blocks/finalized/root", url.trim_end_matches('/')))
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await?
        .json()
        .await?;
    let r = v["data"]["root"].as_str().ok_or_else(|| anyhow::anyhow!("no data.root"))?;
    Ok(r.trim_start_matches("0x").parse()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use skim_state::BlockRef;

    fn blk(n: u64, branch: u8) -> BlockRef {
        let h = |n: u64, b: u8| {
            let mut x = [0u8; 32];
            x[..8].copy_from_slice(&n.to_be_bytes());
            x[31] = b;
            B256::from(x)
        };
        BlockRef { number: n, hash: h(n, branch), parent_hash: h(n.saturating_sub(1), if n == 1 { 0 } else { branch }), state_root: B256::ZERO, timestamp: 0 }
    }

    fn chain(store: &Store, from: u64, to: u64, branch: u8) {
        for n in from..=to {
            store.apply_block(blk(n, branch), Default::default()).unwrap();
        }
    }

    #[test]
    fn safe_never_resolves_to_a_replacement_block_at_the_same_height() {
        // Review scenario: the LC verifies A100; the chain switches to B100.
        let store = Store::new(64);
        chain(&store, 1, 100, 0);
        let a100 = blk(100, 0);
        let v = advance(&store, None, Some(a100.hash)).expect("A100 is on the chain");
        assert_eq!(v, VerifiedBlock { number: 100, hash: a100.hash });
        assert_eq!(resolve(&store, &v), Some(a100));

        store.rollback_to(99);
        let b100 = BlockRef { parent_hash: blk(99, 0).hash, ..blk(100, 1) }; // same parent, different block
        store.apply_block(b100, Default::default()).unwrap();
        assert_eq!(resolve(&store, &v), None, "must refuse, not return B100");
        // The next block drops the stale verified block instead of keeping its height.
        assert_eq!(advance(&store, Some(v), Some(a100.hash)), None);
    }

    #[test]
    fn number_and_hash_move_together() {
        let store = Store::new(64);
        chain(&store, 1, 120, 0);
        let v110 = VerifiedBlock { number: 110, hash: blk(110, 0).hash };
        // An older attestation never pairs the old height with a new hash (or vice versa).
        assert_eq!(advance(&store, Some(v110), Some(blk(105, 0).hash)), Some(v110));
        // A newer one replaces both.
        assert_eq!(advance(&store, Some(v110), Some(blk(118, 0).hash)), Some(VerifiedBlock { number: 118, hash: blk(118, 0).hash }));
        // A hash skim hasn't seen yet keeps the current pair.
        assert_eq!(advance(&store, Some(v110), Some(B256::repeat_byte(0x77))), Some(v110));
    }
}
