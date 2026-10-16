//! Per-key MVCC state store driven by EIP-7928 block access lists (PROJECT.md §5, §6).
//!
//! - **Versions:** each tracked key keeps `(block number, value)` versions along the
//!   *selected* chain. A snapshot of block `n` reads the newest version at or below `n`.
//! - **Unknown is not zero:** a key with no version at or below `n` reads as
//!   [`StateValue::Unknown`].
//! - **BAL index:** recent blocks' normalized writes are kept, so a value fetched at block `P`
//!   (a SNAP base) is committed together with every later write to that key ([`Store::insert_base`]).
//!   A base never reaches backwards: it is valid for `P` and its descendants only.
//! - **Reorgs:** [`Store::rollback_to`] truncates versions and chain entries above the common
//!   ancestor. There is no separate undo journal.
//! - **Single writer:** all mutations go through `&self` methods under one lock. Readers take
//!   immutable [`Snapshot`]s. A snapshot whose block left the selected chain reports
//!   [`StateError::Reorged`] instead of reading another branch's data.

pub mod archive;
pub mod persist;

use std::{collections::BTreeMap, sync::Arc};

// The hot maps (every SLOAD of a simulation looks a key up) use alloy's fast hasher.
use alloy_primitives::map::{HashMap, HashSet};

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use parking_lot::RwLock;
pub use skim_bal::{BlockWrites, StateKey};
use tokio::sync::broadcast;

/// A block on the selected execution chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRef {
    pub number: u64,
    pub hash: B256,
    pub parent_hash: B256,
    pub state_root: B256,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateValue {
    Known(U256),
    Unknown,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum StateError {
    #[error("block {0} is not the child of the head {1}")]
    NotChild(B256, B256),
    #[error("block {number} ({hash}) is no longer on the selected chain")]
    Reorged { number: u64, hash: B256 },
    #[error("block {0} is outside the retained window")]
    OutOfWindow(u64),
    #[error("{0} slots would exceed the subscribed-key limit")]
    LimitReached(usize),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreSizes {
    pub chain: usize,
    pub recent_bals: usize,
    pub keys: usize,
    pub versions: usize,
    pub watched_keys: usize,
    pub temp_keys: usize,
    pub watched_addresses: usize,
    pub code: usize,
    pub code_bytes: usize,
    /// Subscriptions refused because the subscribed-key limit was reached (kept on demand).
    pub admissions_refused: u64,
    /// On-demand keys evicted early because `max_on_demand` was reached.
    pub on_demand_evicted: u64,
    /// Slots held for fully downloaded contracts (`storage: all`).
    pub complete_slots: usize,
    /// Estimated bytes held by the recent-BAL index (all writes of the last `window` blocks,
    /// needed to replay fetched values to the head). Scales with chain activity, not with
    /// your keys; `--window` bounds it.
    pub recent_bal_bytes: usize,
}

/// How long a fetched value is kept current.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    /// Subscribed: kept current from every BAL until unsubscribed.
    Subscribed,
    /// On-demand: kept current for this many blocks past the head, then evicted with its
    /// versions (it reads as Unknown again and is refetched when needed).
    Ttl(u64),
}

/// Emitted after each applied block.
#[derive(Debug, Clone)]
pub struct StateReady {
    pub block: BlockRef,
    /// Tracked keys this block wrote.
    pub changed_keys: Arc<Vec<StateKey>>,
    /// Untracked addresses the block touched (prefetch candidates, §7.3).
    pub touched: usize,
}

/// `BLOCKHASH` covers the 256 blocks before the executing block.
pub const BLOCKHASH_DEPTH: u64 = 257;

#[derive(Default)]
struct Inner {
    chain: BTreeMap<u64, BlockRef>,
    versions: HashMap<StateKey, Vec<(u64, U256)>>,
    watched_addresses: HashSet<Address>,
    watched_keys: HashSet<StateKey>,
    /// On-demand keys and the block after which they expire (tracked until then).
    temp: HashMap<StateKey, u64>,
    /// Keys whose every version goes to the on-disk archive.
    archived: HashSet<StateKey>,
    pending_headers: HashMap<B256, Vec<u8>>,
    recent: BTreeMap<u64, Arc<BlockWrites>>,
    code: HashMap<B256, Bytes>,
    /// Canonical hashes for `BLOCKHASH`: the last 256 below the head must stay answerable even
    /// when the state window is shorter. Filled from applied blocks, their parent hashes, and
    /// hash-linked ancestor headers ([`Store::insert_ancestor_hashes`]).
    hashes: BTreeMap<u64, B256>,
    /// Contracts whose whole storage is known (`storage: all`): every slot at `block`, keyed by
    /// keccak(slot) as the trie stores it. Later writes are ordinary versions on top.
    complete: HashMap<Address, Complete>,
}

#[derive(Clone)]
struct Complete {
    block: u64,
    slots: Arc<HashMap<B256, U256>>,
}

impl Inner {
    fn head(&self) -> Option<&BlockRef> {
        self.chain.values().next_back()
    }

    fn tracked(&self, k: &StateKey) -> bool {
        let a = match k {
            StateKey::Balance(a) | StateKey::Nonce(a) | StateKey::CodeHash(a) | StateKey::Storage(a, _) => a,
        };
        self.watched_addresses.contains(a) || self.watched_keys.contains(k) || self.temp.contains_key(k)
    }

    fn tracked_by_address(&self, k: &StateKey) -> bool {
        let a = match k {
            StateKey::Balance(a) | StateKey::Nonce(a) | StateKey::CodeHash(a) | StateKey::Storage(a, _) => a,
        };
        self.watched_addresses.contains(a)
    }

    fn put(&mut self, key: StateKey, number: u64, value: U256) {
        let v = self.versions.entry(key).or_default();
        match v.binary_search_by_key(&number, |(n, _)| *n) {
            Ok(i) => v[i].1 = value,
            Err(i) => v.insert(i, (number, value)),
        }
    }
}

pub struct Store {
    inner: RwLock<Inner>,
    window: u64,
    events: broadcast::Sender<StateReady>,
    /// Selective archive (M4): every version of archived keys, on disk.
    archive: Option<Arc<archive::Archive>>,
    archive_errors: std::sync::atomic::AtomicU64,
    /// Hard limit on subscribed keys, enforced where keys are admitted (not just counted).
    max_subscribed: std::sync::atomic::AtomicUsize,
    admissions_refused: std::sync::atomic::AtomicU64,
    /// Hard limit on on-demand (TTL) keys; the soonest-expiring are evicted first.
    max_on_demand: std::sync::atomic::AtomicUsize,
    on_demand_evicted: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    pub(crate) test_hook: parking_lot::Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

/// Default cap on on-demand keys (each holds a few versions within the window).
pub const DEFAULT_MAX_ON_DEMAND: usize = 200_000;
/// Rough bytes per entry of a block's write list (key + value + vector overhead).
const BAL_ENTRY_BYTES: usize = 96;

/// What an over-limit subscription becomes: an on-demand key with this TTL.
const REFUSED_TTL: u64 = 4;

impl Store {
    /// `window`: blocks of history (chain entries, BAL index, versions) kept for reads, lazy
    /// fills and reorgs.
    pub fn new(window: u64) -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::default(),
            window,
            events: broadcast::channel(1024).0,
            archive: None,
            archive_errors: Default::default(),
            max_subscribed: std::sync::atomic::AtomicUsize::new(usize::MAX),
            admissions_refused: Default::default(),
            max_on_demand: std::sync::atomic::AtomicUsize::new(DEFAULT_MAX_ON_DEMAND),
            on_demand_evicted: Default::default(),
            #[cfg(test)]
            test_hook: Default::default(),
        })
    }

    /// A store that also archives every version of archived keys (reloaded from `archive`).
    pub fn with_archive(window: u64, archive: archive::Archive) -> Result<Arc<Self>, archive::ArchiveError> {
        let keys = archive.archived_keys()?;
        let keys: HashSet<StateKey> = keys.into_iter().collect();
        let inner = Inner { archived: keys.clone(), watched_keys: keys, ..Default::default() };
        Ok(Arc::new(Self {
            inner: RwLock::new(inner),
            window,
            events: broadcast::channel(1024).0,
            archive: Some(Arc::new(archive)),
            archive_errors: Default::default(),
            max_subscribed: std::sync::atomic::AtomicUsize::new(usize::MAX),
            admissions_refused: Default::default(),
            max_on_demand: std::sync::atomic::AtomicUsize::new(DEFAULT_MAX_ON_DEMAND),
            on_demand_evicted: Default::default(),
            #[cfg(test)]
            test_hook: Default::default(),
        }))
    }

    pub fn archive(&self) -> Option<&Arc<archive::Archive>> {
        self.archive.as_ref()
    }

    pub fn archive_errors(&self) -> u64 {
        self.archive_errors.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn archive_try<T>(&self, r: Result<T, archive::ArchiveError>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(_) => {
                self.archive_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                None
            }
        }
    }

    /// Header RLP for `hash`, written to the archive when its block is applied.
    pub fn note_header(&self, hash: B256, rlp: Vec<u8>) {
        if self.archive.is_some() {
            self.inner.write().pending_headers.insert(hash, rlp);
        }
    }

    /// Caps on-demand (TTL) keys. When full, the soonest-expiring keys are evicted (they are
    /// refetched, verified, if a call needs them again).
    pub fn set_max_on_demand(&self, n: usize) {
        self.max_on_demand.store(n, std::sync::atomic::Ordering::Relaxed);
    }

    /// Caps subscribed keys (watched keys, including archived ones). Keys already subscribed
    /// stay; new subscriptions beyond the cap are refused and kept on demand instead.
    pub fn set_max_subscribed(&self, n: usize) {
        self.max_subscribed.store(n, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether `k` may become subscribed (already is, or there's room).
    fn admit(&self, st: &Inner, k: &StateKey) -> bool {
        let complete: usize = st.complete.values().map(|c| c.slots.len()).sum();
        let ok = st.watched_keys.contains(k)
            || st.watched_keys.len() + complete < self.max_subscribed.load(std::sync::atomic::Ordering::Relaxed);
        if !ok {
            self.admissions_refused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        ok
    }

    /// Archives `k` (a subscription that keeps every version on disk). Returns false if the
    /// subscribed-key limit refused it.
    pub fn archive_key_checked(&self, k: StateKey) -> bool {
        self.archive_key_inner(k, true)
    }

    /// Subscribes `k` and archives every version of it from now on. Versions already held in
    /// memory (the key was tracked) are archived too, so coverage starts at the earliest one.
    pub fn archive_key(&self, k: StateKey) {
        self.archive_key_inner(k, false);
    }

    /// Admission (if `check`), subscription and the archive write happen under one write lock:
    /// no other subscription can take the last slot in between, and no rollback can interleave
    /// with the archive write (archive and memory always change together).
    fn archive_key_inner(&self, k: StateKey, check: bool) -> bool {
        let Some(ar) = &self.archive else {
            return if check { self.watch_key(k) } else { self.watch_key_unchecked(k) };
        };
        let mut st = self.inner.write();
        if check && !self.admit(&st, &k) {
            return false;
        }
        st.temp.remove(&k);
        st.watched_keys.insert(k);
        if !st.archived.insert(k) {
            return true;
        }
        let mem = st.versions.get(&k).cloned().unwrap_or_default();
        let r = match mem.split_first() {
            Some(((b0, v0), rest)) => ar.write_base(&k, *b0, *v0, rest),
            None => ar.add_keys(&[k]),
        };
        self.archive_try(r);
        drop(st);
        true
    }

    fn watch_key_unchecked(&self, k: StateKey) -> bool {
        let mut st = self.inner.write();
        st.temp.remove(&k);
        st.watched_keys.insert(k);
        true
    }

    pub fn is_archived(&self, k: &StateKey) -> bool {
        self.inner.read().archived.contains(k)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StateReady> {
        self.events.subscribe()
    }

    /// Every BAL write to this address is recorded from now on.
    pub fn watch_address(&self, a: Address) {
        self.inner.write().watched_addresses.insert(a);
    }

    /// Subscribes `k`. Returns false (and changes nothing) if the subscribed-key limit refuses it.
    pub fn watch_key(&self, k: StateKey) -> bool {
        let mut st = self.inner.write();
        if !self.admit(&st, &k) {
            return false;
        }
        st.temp.remove(&k);
        st.watched_keys.insert(k);
        true
    }

    /// Stops keeping `k` current (unless covered by a watched address) and drops its versions.
    pub fn unwatch_key(&self, k: &StateKey) {
        let mut st = self.inner.write();
        st.watched_keys.remove(k);
        st.temp.remove(k);
        if !st.tracked_by_address(k) {
            st.versions.remove(k);
        }
    }

    /// Extends on-demand keys that were just used (`cache_ttl_blocks` from the head).
    pub fn touch_ttl(&self, keys: &[StateKey], n: u64) {
        // Most calls in a block find their keys already extended to head + n: check under the
        // read lock and only take the write lock when something actually needs extending.
        {
            let st = self.inner.read();
            let Some(head) = st.head().map(|h| h.number) else { return };
            if !keys.iter().any(|k| st.temp.get(k).is_some_and(|e| *e < head + n)) {
                return;
            }
        }
        let mut st = self.inner.write();
        let Some(head) = st.head().map(|h| h.number) else { return };
        for k in keys {
            if let Some(e) = st.temp.get_mut(k) {
                *e = (*e).max(head + n);
            }
        }
    }

    pub fn is_subscribed(&self, k: &StateKey) -> bool {
        let st = self.inner.read();
        st.watched_keys.contains(k) || st.tracked_by_address(k)
    }

    pub fn head(&self) -> Option<BlockRef> {
        self.inner.read().head().copied()
    }

    pub fn block(&self, number: u64) -> Option<BlockRef> {
        self.inner.read().chain.get(&number).copied()
    }

    /// Applies a block whose BAL commitment has already been verified. The first block anchors
    /// the chain; later ones must extend the head.
    pub fn apply_block(&self, block: BlockRef, writes: Arc<BlockWrites>) -> Result<StateReady, StateError> {
        let ready = {
            let mut st = self.inner.write();
            if let Some(h) = st.head() {
                if block.parent_hash != h.hash || block.number != h.number + 1 {
                    return Err(StateError::NotChild(block.hash, h.hash));
                }
            }
            let mut changed = Vec::new();
            for &(k, v) in &writes.writes {
                if st.tracked(&k) {
                    st.put(k, block.number, v);
                    changed.push(k);
                }
            }
            // Keep code only for tracked accounts: every contract deployed on the chain shows up
            // in some BAL, and caching all of it grows without bound.
            let tracked_code: HashSet<B256> = writes
                .writes
                .iter()
                .filter(|(k, _)| matches!(k, StateKey::CodeHash(_)) && st.tracked(k))
                .map(|(_, v)| B256::from(*v))
                .collect();
            for (h, c) in &writes.code {
                if tracked_code.contains(h) {
                    st.code.insert(*h, c.clone());
                }
            }
            let touched = writes.touched.iter().filter(|a| !st.watched_addresses.contains(*a)).count();
            if let Some(ar) = &self.archive {
                let aw: Vec<(StateKey, U256)> = writes.writes.iter().filter(|(k, _)| st.archived.contains(k)).copied().collect();
                let rlp = st.pending_headers.remove(&block.hash).unwrap_or_default();
                st.pending_headers.retain(|_, _| false);
                let r = ar.write_block(&archive::ArchiveBlock {
                    number: block.number,
                    hash: block.hash,
                    header_rlp: &rlp,
                    writes: &aw,
                });
                self.archive_try(r);
                for (h, c) in &writes.code {
                    if aw.iter().any(|(k, v)| matches!(k, StateKey::CodeHash(_)) && B256::from(*v) == *h) {
                        let r = ar.put_code(*h, c);
                        self.archive_try(r);
                    }
                }
            }
            st.chain.insert(block.number, block);
            st.hashes.insert(block.number, block.hash);
            if let Some(p) = block.number.checked_sub(1) {
                st.hashes.insert(p, block.parent_hash);
            }
            st.recent.insert(block.number, writes);
            self.gc(&mut st);
            StateReady { block, changed_keys: Arc::new(changed), touched }
        };
        let _ = self.events.send(ready.clone());
        Ok(ready)
    }

    /// Drops chain entries, BAL index and versions above `number` (reorg to that ancestor).
    pub fn rollback_to(&self, number: u64) {
        // Archive and memory roll back under the same lock as every other mutation of either.
        let mut st = self.inner.write();
        if let Some(ar) = &self.archive {
            let r = ar.truncate_above(number);
            self.archive_try(r);
        }
        st.chain.retain(|n, _| *n <= number);
        st.hashes.retain(|n, _| *n <= number);
        // A download taken at a block that's now orphaned is no longer this chain's state.
        st.complete.retain(|_, c| c.block <= number);
        st.recent.retain(|n, _| *n <= number);
        for v in st.versions.values_mut() {
            v.retain(|(n, _)| *n <= number);
        }
    }

    /// Commits a value fetched at `at` (e.g. via SNAP at `at.state_root`), then replays later
    /// indexed writes to `key`, so a stale fetch never hides a newer BAL write. Tracks the key.
    pub fn insert_base(&self, at: &BlockRef, key: StateKey, value: U256) -> Result<(), StateError> {
        self.insert_base_with(at, key, value, Retention::Subscribed)
    }

    /// [`Self::insert_base`] with an explicit retention. A TTL never downgrades a key that is
    /// already subscribed.
    pub fn insert_base_with(&self, at: &BlockRef, key: StateKey, value: U256, retention: Retention) -> Result<(), StateError> {
        let mut st = self.inner.write();
        if st.chain.get(&at.number).map(|b| b.hash) != Some(at.hash) {
            return Err(StateError::Reorged { number: at.number, hash: at.hash });
        }
        let retention = match retention {
            Retention::Subscribed if !self.admit(&st, &key) => Retention::Ttl(REFUSED_TTL),
            r => r,
        };
        match retention {
            Retention::Subscribed => {
                st.temp.remove(&key);
                st.watched_keys.insert(key);
            }
            Retention::Ttl(n) => {
                let subscribed = st.watched_keys.contains(&key) || st.tracked_by_address(&key);
                if !subscribed {
                    let head = st.head().map_or(at.number, |h| h.number);
                    let max = self.max_on_demand.load(std::sync::atomic::Ordering::Relaxed);
                    if !st.temp.contains_key(&key) && st.temp.len() >= max {
                        // Evict the soonest-expiring tenth (never the key being inserted).
                        let mut by_exp: Vec<(u64, StateKey)> = st.temp.iter().map(|(k, e)| (*e, *k)).collect();
                        by_exp.sort();
                        let n_evict = (max / 10).max(1).min(by_exp.len());
                        for (_, k) in by_exp.into_iter().take(n_evict) {
                            st.temp.remove(&k);
                            st.versions.remove(&k);
                        }
                        self.on_demand_evicted.fetch_add(n_evict as u64, std::sync::atomic::Ordering::Relaxed);
                    }
                    let e = st.temp.entry(key).or_insert(0);
                    *e = (*e).max(head + n);
                }
            }
        }
        // Keep an existing version at `at` (a BAL write at that block is authoritative).
        let has_at = st.versions.get(&key).is_some_and(|v| v.iter().any(|(n, _)| *n == at.number));
        if !has_at {
            st.put(key, at.number, value);
        }
        let later: Vec<(u64, U256)> = st
            .recent
            .range(at.number + 1..)
            .filter_map(|(n, w)| w.get(&key).map(|v| (*n, v)))
            .collect();
        for (n, v) in &later {
            st.put(key, *n, *v);
        }
        #[cfg(test)]
        if let Some(h) = self.test_hook.lock().as_ref() {
            h(); // tests pause here, still holding the lock, to race a rollback against us
        }
        if st.archived.contains(&key) {
            let base = st.versions.get(&key).and_then(|v| v.iter().find(|(n, _)| *n == at.number).copied());
            // Written while still holding the lock: the chain check above and the archive write
            // are one step, so a rollback can't land between them and leave old-fork data.
            if let (Some(ar), Some((n, v))) = (&self.archive, base) {
                let r = ar.write_base(&key, n, v, &later);
                self.archive_try(r);
            }
        }
        drop(st);
        Ok(())
    }

    /// Sizes of the store's structures (for growth monitoring).
    pub fn sizes(&self) -> StoreSizes {
        let st = self.inner.read();
        StoreSizes {
            chain: st.chain.len(),
            recent_bals: st.recent.len(),
            keys: st.versions.len(),
            versions: st.versions.values().map(Vec::len).sum(),
            watched_keys: st.watched_keys.len(),
            temp_keys: st.temp.len(),
            watched_addresses: st.watched_addresses.len(),
            code: st.code.len(),
            code_bytes: st.code.values().map(|c| c.len()).sum(),
            admissions_refused: self.admissions_refused.load(std::sync::atomic::Ordering::Relaxed),
            on_demand_evicted: self.on_demand_evicted.load(std::sync::atomic::Ordering::Relaxed),
            complete_slots: st.complete.values().map(|c| c.slots.len()).sum(),
            recent_bal_bytes: st.recent.values().map(|w| (w.writes.len() + w.touched.len() + w.storage_reads.len()) * BAL_ENTRY_BYTES).sum(),
        }
    }

    pub fn code(&self, hash: &B256) -> Option<Bytes> {
        if let Some(c) = self.inner.read().code.get(hash).cloned() {
            return Some(c);
        }
        self.archive.as_ref().and_then(|a| a.code(hash).ok().flatten())
    }

    pub fn insert_code(&self, code: Bytes) -> B256 {
        let h = keccak256(&code);
        if let Some(ar) = &self.archive {
            let r = ar.put_code(h, &code);
            self.archive_try(r);
        }
        self.inner.write().code.insert(h, code);
        h
    }

    /// Snapshot of `block` (must be on the selected chain).
    pub fn snapshot(self: &Arc<Self>, block: &BlockRef) -> Result<Snapshot, StateError> {
        let st = self.inner.read();
        match st.chain.get(&block.number) {
            Some(b) if b.hash == block.hash => Ok(Snapshot { store: self.clone(), block: *block, archived: false }),
            Some(_) => Err(StateError::Reorged { number: block.number, hash: block.hash }),
            None => match &self.archive {
                Some(ar) if ar.hash(block.number).ok().flatten() == Some(block.hash) => {
                    Ok(Snapshot { store: self.clone(), block: *block, archived: true })
                }
                _ => Err(StateError::OutOfWindow(block.number)),
            },
        }
    }

    /// A block outside the in-memory window, from the archive (header decoded).
    pub fn archived_block(&self, number: u64) -> Option<(BlockRef, Vec<u8>)> {
        let ar = self.archive.as_ref()?;
        let rlp = ar.header_rlp(number).ok().flatten()?;
        let hash = ar.hash(number).ok().flatten()?;
        let h = <alloy_consensus::Header as alloy_rlp::Decodable>::decode(&mut rlp.as_slice()).ok()?;
        Some((
            BlockRef { number, hash, parent_hash: h.parent_hash, state_root: h.state_root, timestamp: h.timestamp },
            rlp,
        ))
    }

    /// Block hash by number: the in-memory chain, the `BLOCKHASH` ring, then the archive.
    /// `None` means unknown, never zero.
    pub fn block_hash(&self, number: u64) -> Option<B256> {
        {
            let st = self.inner.read();
            if let Some(b) = st.chain.get(&number) {
                return Some(b.hash);
            }
            if let Some(h) = st.hashes.get(&number) {
                return Some(*h);
            }
        }
        self.archive.as_ref().and_then(|a| a.hash(number).ok().flatten())
    }

    /// The lowest block whose hash is known in memory, as `(number, hash)`. Its parent hash
    /// comes with the header, so ancestor walks start from here.
    pub fn oldest_known_hash(&self) -> Option<(u64, B256)> {
        self.inner.read().hashes.first_key_value().map(|(n, h)| (*n, *h))
    }

    /// Adds ancestor hashes that link to the oldest known hash: `chain` is ordered newest
    /// first, and each entry's hash must be the parent hash of the one before it (callers pass
    /// `(number, hash, parent_hash)` from headers they hashed themselves). Stops at the first
    /// break; returns how many were accepted.
    pub fn insert_ancestor_hashes(&self, chain: &[(u64, B256, B256)]) -> usize {
        let mut st = self.inner.write();
        let Some((mut n, mut expect_hash)) = st.hashes.first_key_value().map(|(n, h)| (*n, *h)) else { return 0 };
        let mut added = 0;
        // The entry at `n` is known; its header's parent hash is the next expected hash.
        let Some(mut parent) = chain.iter().find(|(m, h, _)| *m == n && *h == expect_hash).map(|c| c.2).or_else(|| {
            st.chain.get(&n).map(|b| b.parent_hash)
        }) else {
            return 0;
        };
        for &(m, h, p) in chain {
            if m >= n {
                continue;
            }
            if m != n - 1 || h != parent {
                break;
            }
            st.hashes.insert(m, h);
            (n, expect_hash, parent) = (m, h, p);
            added += 1;
        }
        let _ = expect_hash;
        added
    }

    /// Installs a contract's whole storage, downloaded and verified at block `at` (keccak-keyed
    /// slots). The address must already be watched, so writes after `at` are recorded; writes
    /// between `at` and now that are in the BAL index are replayed here. Counts toward the
    /// subscribed-key limit as one key per slot.
    pub fn install_complete_storage(&self, at: &BlockRef, a: Address, slots: Vec<(B256, U256)>) -> Result<(), StateError> {
        let mut st = self.inner.write();
        if st.chain.get(&at.number).map(|b| b.hash) != Some(at.hash) {
            return Err(StateError::Reorged { number: at.number, hash: at.hash });
        }
        let max = self.max_subscribed.load(std::sync::atomic::Ordering::Relaxed);
        let others: usize = st.complete.iter().filter(|(x, _)| **x != a).map(|(_, c)| c.slots.len()).sum();
        if st.watched_keys.len() + others + slots.len() > max {
            self.admissions_refused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Err(StateError::LimitReached(slots.len()));
        }
        st.watched_addresses.insert(a);
        let later: Vec<(u64, StateKey, U256)> = st
            .recent
            .range(at.number + 1..)
            .flat_map(|(n, w)| w.writes.iter().filter(|(k, _)| matches!(k, StateKey::Storage(x, _) if *x == a)).map(move |(k, v)| (*n, *k, *v)))
            .collect();
        for (n, k, v) in later {
            st.put(k, n, v);
        }
        st.complete.insert(a, Complete { block: at.number, slots: Arc::new(slots.into_iter().collect()) });
        Ok(())
    }

    /// The block a contract's whole storage is known from, if it is.
    pub fn complete_storage_block(&self, a: &Address) -> Option<u64> {
        self.inner.read().complete.get(a).map(|c| c.block)
    }

    /// Number of a block hash on the in-memory chain.
    pub fn number_of_hash(&self, hash: &B256) -> Option<u64> {
        self.inner.read().chain.iter().rev().find(|(_, b)| b.hash == *hash).map(|(n, _)| *n)
    }

    /// Number of an archived block hash.
    pub fn archived_number(&self, hash: &B256) -> Option<u64> {
        self.archive.as_ref().and_then(|a| a.number_of(hash).ok().flatten())
    }

    /// Tracked keys that are known at `block`.
    /// Subscribed keys known at `block`, with the bytecode their code hashes point to: what
    /// a restart snapshot keeps.
    pub fn subscribed_at(&self, block: &BlockRef) -> (Vec<(StateKey, U256)>, Vec<(B256, Bytes)>) {
        let st = self.inner.read();
        let values: Vec<(StateKey, U256)> = st
            .watched_keys
            .iter()
            .filter_map(|k| {
                let v = st.versions.get(k)?;
                let i = v.partition_point(|(n, _)| *n <= block.number);
                (i > 0).then(|| (*k, v[i - 1].1))
            })
            .collect();
        let code = values
            .iter()
            .filter(|(k, _)| matches!(k, StateKey::CodeHash(_)))
            .filter_map(|(_, h)| {
                let h = B256::from(*h);
                st.code.get(&h).map(|c| (h, c.clone()))
            })
            .collect();
        (values, code)
    }

    pub fn known_keys(&self, block: &BlockRef) -> Vec<(StateKey, U256)> {
        let st = self.inner.read();
        st.versions
            .iter()
            .filter_map(|(k, v)| {
                let i = v.partition_point(|(n, _)| *n <= block.number);
                (i > 0).then(|| (*k, v[i - 1].1))
            })
            .collect()
    }

    fn gc(&self, st: &mut Inner) {
        let Some(head) = st.head().map(|h| h.number) else { return };
        // Expire on-demand keys: stop tracking and forget their versions.
        let expired: Vec<StateKey> = st.temp.iter().filter(|(_, e)| **e < head).map(|(k, _)| *k).collect();
        for k in expired {
            st.temp.remove(&k);
            st.versions.remove(&k);
        }
        // Bytecode lives only as long as some tracked account's code hash points at it.
        let referenced: HashSet<B256> = st
            .versions
            .iter()
            .filter(|(k, _)| matches!(k, StateKey::CodeHash(_)))
            .flat_map(|(_, v)| v.iter().map(|(_, h)| B256::from(*h)))
            .collect();
        st.code.retain(|h, _| referenced.contains(h));
        // BLOCKHASH reaches back 256 blocks from the next block: keep head-256..=head.
        if let Some(c) = head.checked_sub(BLOCKHASH_DEPTH) {
            st.hashes.retain(|n, _| *n >= c);
        }
        let Some(cut) = head.checked_sub(self.window) else { return };
        st.chain.retain(|n, _| *n > cut);
        st.recent.retain(|n, _| *n > cut);
        for v in st.versions.values_mut() {
            // Collapse everything at or below the cut into one base version.
            let i = v.partition_point(|(n, _)| *n <= cut);
            if i > 1 {
                v.drain(..i - 1);
            }
        }
    }
}

/// An immutable view of one block's post-state.
#[derive(Clone)]
pub struct Snapshot {
    store: Arc<Store>,
    block: BlockRef,
    /// Served from the on-disk archive (outside the in-memory window).
    archived: bool,
}

impl Snapshot {
    pub fn block(&self) -> &BlockRef {
        &self.block
    }

    pub fn code(&self, hash: &B256) -> Option<Bytes> {
        self.store.code(hash)
    }

    pub fn is_archived(&self) -> bool {
        self.archived
    }

    pub fn get(&self, key: &StateKey) -> Result<StateValue, StateError> {
        if self.archived {
            let ar = self.store.archive.as_ref().expect("archived snapshot without archive");
            return match ar.get_at(key, self.block.number, self.block.hash) {
                Ok(archive::ArchiveRead::Value(v)) => Ok(StateValue::Known(v)),
                Ok(archive::ArchiveRead::OtherFork) => Err(StateError::Reorged { number: self.block.number, hash: self.block.hash }),
                _ => Ok(StateValue::Unknown),
            };
        }
        let st = self.store.inner.read();
        if st.chain.get(&self.block.number).map(|b| b.hash) != Some(self.block.hash) {
            return Err(StateError::Reorged { number: self.block.number, hash: self.block.hash });
        }
        if let Some(v) = st.versions.get(key) {
            let i = v.partition_point(|(n, _)| *n <= self.block.number);
            if i > 0 {
                return Ok(StateValue::Known(v[i - 1].1));
            }
        }
        // A slot of a fully downloaded contract that no write since the download touched: its
        // value is in the verified trie, or it is a proven zero.
        if let StateKey::Storage(a, slot) = key
            && let Some(c) = st.complete.get(a)
            && c.block <= self.block.number
        {
            let h = keccak256(slot.to_be_bytes::<32>());
            return Ok(StateValue::Known(c.slots.get(&h).copied().unwrap_or_default()));
        }
        Ok(StateValue::Unknown)
    }
}

#[cfg(test)]
mod tests;
