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
    /// Reads of untracked keys answered from a recent BAL write instead of a fetch.
    pub resolved_from_bal: u64,
    /// Keys given a retention (on-demand, subscribed, archived) from recent BALs, no fetch.
    pub bal_promotions: u64,
    /// Slots held for fully downloaded contracts (`storage: all`).
    pub complete_slots: usize,
    /// Estimated bytes held by the recent-BAL index (all writes of the last `window` blocks,
    /// needed to replay fetched values to the head). Scales with chain activity, not with
    /// your keys; `--window` bounds it.
    pub recent_bal_bytes: usize,
    /// What the recent-BAL index holds, by part (entries, and bytecode bytes).
    pub recent_writes: usize,
    pub recent_reads: usize,
    pub recent_touched: usize,
    pub recent_code_bytes: usize,
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
    /// Whole-storage contracts dropped because their growth hit `max_subscribed_keys`, and why
    /// (reported by the owning subscription).
    dropped_complete: HashMap<Address, String>,
}

#[derive(Clone)]
struct Complete {
    block: u64,
    slots: Arc<HashMap<B256, U256>>,
    /// Slots first written after the download (not in `slots`). They are tracked too, so they
    /// count toward the limits like the downloaded ones.
    extra: HashSet<U256>,
}

impl Complete {
    fn len(&self) -> usize {
        self.slots.len() + self.extra.len()
    }

    /// Records a write; true if it is a slot the contract didn't hold yet.
    fn note_write(&mut self, slot: U256) -> bool {
        !self.extra.contains(&slot) && !self.slots.contains_key(&keccak256(slot.to_be_bytes::<32>())) && self.extra.insert(slot)
    }
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

    fn subscribed_total(&self) -> usize {
        self.watched_keys.len() + self.complete.values().map(Complete::len).sum::<usize>()
    }

    /// Stops whole-storage tracking of `a`: the address is no longer tracked, and its keys'
    /// versions are dropped unless a key is watched individually.
    fn drop_complete(&mut self, a: &Address) {
        self.complete.remove(a);
        self.watched_addresses.remove(a);
        let watched = std::mem::take(&mut self.watched_keys);
        self.versions.retain(|k, _| match k {
            StateKey::Storage(x, _) | StateKey::Balance(x) | StateKey::Nonce(x) | StateKey::CodeHash(x) => x != a || watched.contains(k),
        });
        self.watched_keys = watched;
    }

    /// Owned by a subscription (the key itself, or its whole address). An owned key's versions
    /// are never deleted because of an on-demand TTL, whatever TTL entry it still has.
    fn owned(&self, k: &StateKey) -> bool {
        self.watched_keys.contains(k) || self.tracked_by_address(k)
    }

    /// The canonical hash this store holds for block `n` (state window or hash ring).
    fn known_hash(&self, n: u64) -> Option<B256> {
        self.chain.get(&n).map(|b| b.hash).or_else(|| self.hashes.get(&n).copied())
    }

    /// The lowest block in `from..to` whose canonical hash is known and passes `ok`.
    fn first_known(&self, from: u64, to: u64, ok: &dyn Fn(u64, B256) -> bool) -> Option<(u64, B256)> {
        let mut c: Vec<(u64, B256)> = self.chain.range(from..to).map(|(n, b)| (*n, b.hash)).collect();
        c.extend(self.hashes.range(from..to).map(|(n, h)| (*n, *h)));
        c.sort_unstable_by_key(|x| x.0);
        c.into_iter().find(|(n, h)| ok(*n, *h))
    }

    /// `k`'s versions as archive writes `(block, hash, value)`, each under a hash this store
    /// knows for its block. A version whose block hash is gone (pruned from the hash ring while
    /// the value stayed, e.g. an unchanged key tracked for longer than both windows) is moved
    /// up to the lowest later block with a known hash, where it is still the key's value, so the
    /// archive starts there: never earlier, and never without a base.
    /// `ok` admits a `(block, hash)` (the archive must hold that block).
    fn archivable_versions(&self, k: &StateKey, ok: &dyn Fn(u64, B256) -> bool) -> Vec<(u64, B256, U256)> {
        let Some(v) = self.versions.get(k) else { return vec![] };
        let end = self.head().map_or(0, |h| h.number + 1);
        let mut out = Vec::with_capacity(v.len());
        for (i, (n, x)) in v.iter().enumerate() {
            if let Some(h) = self.known_hash(*n).filter(|h| ok(*n, *h)) {
                out.push((*n, h, *x));
                continue;
            }
            // In effect until the next version (or the head).
            let until = v.get(i + 1).map_or(end, |(m, _)| *m);
            if let Some((m, h)) = self.first_known(*n + 1, until, ok) {
                out.push((m, h, *x));
            }
        }
        out
    }

    /// The writes to `k` in the recent-BAL index, oldest first, if they are usable: every block
    /// from the first of them to the head is indexed (a gap could hide a write).
    fn bal_writes(&self, k: &StateKey) -> Vec<(u64, U256)> {
        let Some(head) = self.head().map(|h| h.number) else { return vec![] };
        let w: Vec<(u64, U256)> = self.recent.iter().filter_map(|(n, b)| b.lookup(k).map(|v| (*n, v))).collect();
        match w.first() {
            Some((first, _)) if self.recent.range(first..=&head).count() as u64 == head - first + 1 => w,
            _ => vec![],
        }
    }

    /// Promotion from the recent-BAL index: when a key gets a retention (on-demand, subscribed,
    /// archived) and holds no versions, the values it only had in recent BALs become its
    /// versions, at the blocks that wrote them. From there BAL writes keep it current like any
    /// fetched key, and archiving persists them.
    fn seed_from_bal(&mut self, k: &StateKey) -> bool {
        if self.versions.get(k).is_some_and(|v| !v.is_empty()) {
            return false;
        }
        let w = self.bal_writes(k);
        for (n, v) in &w {
            self.put(*k, *n, *v);
        }
        !w.is_empty()
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
    /// Reads of untracked keys answered from a recent block's BAL write (no fetch needed).
    resolved_from_bal: std::sync::atomic::AtomicU64,
    /// Keys given a retention whose value came from recent BALs (promotion, no fetch).
    bal_promotions: std::sync::atomic::AtomicU64,
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
            resolved_from_bal: Default::default(),
            bal_promotions: Default::default(),
            #[cfg(test)]
            test_hook: Default::default(),
        })
    }

    /// A store that also archives every version of archived keys (reloaded from `archive`).
    /// Reloaded keys stay archived (each block's writes to them keep being recorded) but hold
    /// no subscribed slot until a subscription asks for them again: a key nothing re-requests
    /// (a removed subscription, a call that stopped reading it) never crowds out the keys the
    /// current config needs, however often skim restarts. Those are released after a while.
    pub fn with_archive(window: u64, archive: archive::Archive) -> Result<Arc<Self>, archive::ArchiveError> {
        let keys = archive.archived_keys()?;
        let keys: HashSet<StateKey> = keys.into_iter().collect();
        let inner = Inner { archived: keys, ..Default::default() };
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
            resolved_from_bal: Default::default(),
            bal_promotions: Default::default(),
            #[cfg(test)]
            test_hook: Default::default(),
        }))
    }

    pub fn archive(&self) -> Option<&Arc<archive::Archive>> {
        self.archive.as_ref()
    }

    /// Archive coverage of an address's archived keys: (key, first covered block, whether that
    /// start is a write).
    pub fn archive_coverage(&self, a: &Address) -> Vec<(StateKey, u64, bool)> {
        let Some(ar) = &self.archive else { return vec![] };
        ar.coverages()
            .unwrap_or_default()
            .into_iter()
            .filter(|(k, _, _)| matches!(k, StateKey::Balance(x) | StateKey::Nonce(x) | StateKey::CodeHash(x) | StateKey::Storage(x, _) if x == a))
            .map(|(k, c, ext)| (k, c, !ext))
            .collect()
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
        let complete: usize = st.complete.values().map(Complete::len).sum();
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
        // Seed before archiving, so the archive starts at the block that wrote the value.
        let seeded = st.seed_from_bal(&k);
        if seeded {
            self.bal_promotions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        if !st.archived.insert(k) {
            // Already archived (e.g. reloaded after a restart) is not "its value is archived": a
            // value known in memory has to become this segment's base, whether this call seeded
            // it or something earlier did (an on-demand read warming the key first). Repeating a
            // base is harmless: it never overwrites a BAL write, and coverage only moves earlier.
            self.archive_known_versions(&st, &k);
            return true;
        }
        let mem = st.archivable_versions(&k, &|n, h| ar.hash(n).ok().flatten() == Some(h));
        if mem.is_empty() && st.versions.get(&k).is_some_and(|v| !v.is_empty()) {
            tracing::warn!(?k, "archive: a known value has no archived block to start from; archived from its next write");
        }
        let r = match mem.split_first() {
            Some(((b0, h0, v0), rest)) => ar.write_base(&k, *b0, *h0, *v0, rest),
            None => ar.add_keys(&[k]),
        };
        self.archive_try(r);
        drop(st);
        true
    }

    /// Writes `k`'s in-memory versions to the archive as its base (hash-checked against the
    /// archived blocks; a version older than the archive starts at its first block instead).
    /// Callers hold the write lock, so archive and memory change together.
    fn archive_known_versions(&self, st: &Inner, k: &StateKey) {
        let Some(ar) = &self.archive else { return };
        let mem = st.archivable_versions(k, &|n, h| ar.hash(n).ok().flatten() == Some(h));
        if let Some(((b0, h0, v0), rest)) = mem.split_first() {
            let r = ar.write_base(k, *b0, *h0, *v0, rest);
            self.archive_try(r);
        }
    }

    fn watch_key_unchecked(&self, k: StateKey) -> bool {
        let mut st = self.inner.write();
        st.temp.remove(&k);
        st.watched_keys.insert(k);
        if st.seed_from_bal(&k) {
            self.bal_promotions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
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
        let mut st = self.inner.write();
        st.watched_addresses.insert(a);
        // The address owns its keys now: on-demand TTL entries for them must not expire (and
        // delete) versions that address-level tracking keeps current.
        st.temp.retain(|k, _| match k {
            StateKey::Balance(x) | StateKey::Nonce(x) | StateKey::CodeHash(x) | StateKey::Storage(x, _) => *x != a,
        });
    }

    /// Subscribes `k`. Returns false (and changes nothing) if the subscribed-key limit refuses it.
    pub fn watch_key(&self, k: StateKey) -> bool {
        let mut st = self.inner.write();
        if !self.admit(&st, &k) {
            return false;
        }
        st.temp.remove(&k);
        st.watched_keys.insert(k);
        if st.seed_from_bal(&k) {
            self.bal_promotions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        true
    }

    /// Archived keys (the ones currently archived, not those whose coverage ended).
    pub fn archived_keys(&self) -> Vec<StateKey> {
        self.inner.read().archived.iter().copied().collect()
    }

    /// Stops archiving `keys` and frees their subscribed slots: their archive history stays
    /// readable up to the current head, reads above that are refused, and archiving one again
    /// later starts its coverage afresh. Returns how many were released.
    pub fn release_archived(&self, keys: &[StateKey]) -> usize {
        // Archive and memory change under one lock, as everywhere else.
        let mut st = self.inner.write();
        let head = st.head().map(|h| h.number);
        let mut n = 0;
        for k in keys {
            if !st.archived.remove(k) {
                continue;
            }
            if let Some(ar) = &self.archive {
                let r = ar.end_coverage(k, head);
                self.archive_try(r);
            }
            st.watched_keys.remove(k);
            st.temp.remove(k);
            if !st.tracked_by_address(k) {
                st.versions.remove(k);
            }
            n += 1;
        }
        n
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

    /// Extends on-demand keys that were just used (`cache_ttl_blocks` from the head). A used key
    /// skim doesn't track yet but read from a recent BAL becomes on-demand state like a fetched
    /// one (its BAL values become versions), so it stays known after the BAL leaves the window.
    pub fn touch_ttl(&self, keys: &[StateKey], n: u64) {
        // Most calls in a block find their keys already extended to head + n: check under the
        // read lock and only take the write lock when something actually needs doing.
        {
            let st = self.inner.read();
            let Some(head) = st.head().map(|h| h.number) else { return };
            let needs = |k: &StateKey| match st.temp.get(k) {
                Some(e) => *e < head + n,
                None => !st.tracked(k) && !st.bal_writes(k).is_empty(),
            };
            if !keys.iter().any(needs) {
                return;
            }
        }
        let mut st = self.inner.write();
        let Some(head) = st.head().map(|h| h.number) else { return };
        for k in keys {
            if let Some(e) = st.temp.get_mut(k) {
                *e = (*e).max(head + n);
            } else if !st.tracked(k) && !st.bal_writes(k).is_empty() {
                self.admit_temp(&mut st, *k, head + n);
                if st.seed_from_bal(k) {
                    self.bal_promotions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // An archived key's value just became known: archive it now, as the BAL
                    // seeding does, rather than relying on a later subscription to notice.
                    if st.archived.contains(k) {
                        self.archive_known_versions(&st, k);
                    }
                }
            }
        }
    }

    /// Adds an on-demand entry expiring at `until`, evicting the soonest-expiring tenth when
    /// `max_on_demand` is reached (never `key` itself, never versions of owned keys).
    fn admit_temp(&self, st: &mut Inner, key: StateKey, until: u64) {
        let max = self.max_on_demand.load(std::sync::atomic::Ordering::Relaxed);
        if !st.temp.contains_key(&key) && st.temp.len() >= max {
            let mut by_exp: Vec<(u64, StateKey)> = st.temp.iter().map(|(k, e)| (*e, *k)).collect();
            by_exp.sort();
            let n_evict = (max / 10).max(1).min(by_exp.len());
            for (_, k) in by_exp.into_iter().take(n_evict) {
                st.temp.remove(&k);
                if !st.owned(&k) {
                    st.versions.remove(&k);
                }
            }
            self.on_demand_evicted.fetch_add(n_evict as u64, std::sync::atomic::Ordering::Relaxed);
        }
        let e = st.temp.entry(key).or_insert(0);
        *e = (*e).max(until);
    }

    /// Addresses with any subscribed key (an account field, a slot, or whole storage): what the
    /// dashboard calls pinned.
    pub fn pinned_addresses(&self) -> HashSet<Address> {
        let st = self.inner.read();
        let mut out = st.watched_addresses.clone();
        out.extend(st.complete.keys().copied());
        out.extend(st.watched_keys.iter().map(|k| match k {
            StateKey::Balance(a) | StateKey::Nonce(a) | StateKey::CodeHash(a) | StateKey::Storage(a, _) => *a,
        }));
        out
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
        // Recent-BAL lookups binary-search each block's writes (BAL-built values are sorted).
        let writes = if writes.is_normalized() {
            writes
        } else {
            let mut w = (*writes).clone();
            w.normalize();
            Arc::new(w)
        };
        let ready = {
            let mut st = self.inner.write();
            if let Some(h) = st.head() {
                if block.parent_hash != h.hash || block.number != h.number + 1 {
                    return Err(StateError::NotChild(block.hash, h.hash));
                }
            }
            let mut changed = Vec::new();
            let max = self.max_subscribed.load(std::sync::atomic::Ordering::Relaxed);
            let mut over_budget: Vec<Address> = Vec::new();
            for &(k, v) in &writes.writes {
                if st.tracked(&k) {
                    if let StateKey::Storage(a, s) = k
                        && st.complete.get_mut(&a).is_some_and(|c| c.note_write(s))
                        && st.subscribed_total() > max
                        && !over_budget.contains(&a)
                    {
                        over_budget.push(a);
                    }
                    st.put(k, block.number, v);
                    changed.push(k);
                }
            }
            // A whole-storage contract whose growth exceeds the subscribed-key budget stops
            // being tracked as a whole (never: keep claiming completeness and skip its writes).
            for a in over_budget {
                st.drop_complete(&a);
                st.dropped_complete.insert(
                    a,
                    format!("whole storage dropped at block {}: growth exceeded limits.max_subscribed_keys = {max}", block.number),
                );
                self.admissions_refused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                // A height the archive already has (e.g. the first block after a restart): the
                // same block is already archived; another block means the archive holds an
                // abandoned fork from here up. Drop it before writing, so this fork's values are
                // never stored under that fork's headers. (A fork deeper than this height leaves
                // an unlinked segment boundary, which the healer reconciles.)
                let overlap = ar.last_block().ok().flatten().is_some_and(|end| end >= block.number);
                let already = overlap && ar.hash(block.number).ok().flatten() == Some(block.hash);
                if overlap && !already {
                    tracing::warn!(block = block.number, "archive: holds another block at this height; dropping it and everything above");
                    let r = ar.truncate_above(block.number.saturating_sub(1));
                    self.archive_try(r);
                }
                let r = if already { Ok(()) } else { ar.write_block(&archive::ArchiveBlock {
                    number: block.number,
                    hash: block.hash,
                    parent_hash: block.parent_hash,
                    header_rlp: &rlp,
                    writes: &aw,
                }) };
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
            // The index only ever needs the writes (BAL-first lookups, promotion, replaying
            // fetched values): keep those, not the block's reads, touched addresses or new code
            // (~44% of a Sepolia block's entries).
            let compact = if writes.storage_reads.is_empty() && writes.touched.is_empty() && writes.code.is_empty() {
                writes
            } else {
                Arc::new(BlockWrites { writes: writes.writes.clone(), ..Default::default() })
            };
            st.recent.insert(block.number, compact);
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

    /// Fills the recent-BAL index with blocks from before the first applied one (just after a
    /// start, the index would otherwise cover only blocks seen since). `blocks` must be
    /// ascending and hash-linked, and the last must be a block whose hash the store already
    /// holds (the parent of the first applied block, or later); BALs are expected verified
    /// against their headers. Tracked keys that hold no value yet are then promoted from the
    /// index, as they would have been had these blocks been applied. Returns the blocks added.
    pub fn seed_recent(&self, blocks: &[(BlockRef, Arc<BlockWrites>)]) -> Result<usize, StateError> {
        for w in blocks.windows(2) {
            if w[1].0.number != w[0].0.number + 1 || w[1].0.parent_hash != w[0].0.hash {
                return Err(StateError::NotChild(w[1].0.hash, w[0].0.hash));
            }
        }
        let Some((last, _)) = blocks.last() else { return Ok(0) };
        let mut st = self.inner.write();
        if st.hashes.get(&last.number) != Some(&last.hash) {
            return Err(StateError::Reorged { number: last.number, hash: last.hash });
        }
        let cut = st.head().map(|h| h.number.saturating_sub(self.window)).unwrap_or(0);
        let mut added = 0;
        for (b, w) in blocks {
            if b.number <= cut || st.recent.contains_key(&b.number) {
                continue;
            }
            let mut w = BlockWrites { writes: w.writes.clone(), ..Default::default() };
            w.normalize();
            st.recent.insert(b.number, Arc::new(w));
            st.hashes.entry(b.number).or_insert(b.hash);
            st.hashes.entry(b.number.saturating_sub(1)).or_insert(b.parent_hash);
            added += 1;
        }
        let waiting: Vec<StateKey> = st.watched_keys.iter().chain(st.temp.keys()).copied().collect();
        for k in waiting {
            if st.seed_from_bal(&k) {
                self.bal_promotions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // An archived key now known: its archive coverage starts here too (at the first
                // block the archive holds, if the value predates it), as when it was archived.
                if st.archived.contains(&k) {
                    self.archive_known_versions(&st, &k);
                }
            }
        }
        Ok(added)
    }

    /// Commits a value fetched at `at` (e.g. via SNAP at `at.state_root`), then replays later
    /// indexed writes to `key`, so a stale fetch never hides a newer BAL write. Tracks the key.
    pub fn insert_base(&self, at: &BlockRef, key: StateKey, value: U256) -> Result<(), StateError> {
        self.insert_base_with(at, key, value, Retention::Subscribed)
    }

    /// [`Self::insert_base`] with an explicit retention. A TTL never downgrades a key that is
    /// already subscribed.
    pub fn insert_base_with(&self, at: &BlockRef, key: StateKey, value: U256, retention: Retention) -> Result<(), StateError> {
        self.insert_bases_with(at, &[(key, value)], retention)
    }

    /// Several verified values at `at` (e.g. an account's balance, nonce and code hash), under one
    /// write lock: the chain check, the replay of later BAL writes and the archive writes happen
    /// together for all of them, as one step (a rollback can't land in between).
    pub fn insert_bases_with(&self, at: &BlockRef, values: &[(StateKey, U256)], retention: Retention) -> Result<(), StateError> {
        let mut st = self.inner.write();
        if st.chain.get(&at.number).map(|b| b.hash) != Some(at.hash) {
            return Err(StateError::Reorged { number: at.number, hash: at.hash });
        }
        for &(key, value) in values {
            self.insert_base_locked(&mut st, at, key, value, retention);
        }
        drop(st);
        Ok(())
    }

    fn insert_base_locked(&self, st: &mut Inner, at: &BlockRef, key: StateKey, value: U256, retention: Retention) {
        let retention = match retention {
            Retention::Subscribed if !self.admit(st, &key) => Retention::Ttl(REFUSED_TTL),
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
                    self.admit_temp(st, key, head + n);
                }
            }
        }
        // Keep an existing version at `at` (a BAL write at that block is authoritative).
        let has_at = st.versions.get(&key).is_some_and(|v| v.iter().any(|(n, _)| *n == at.number));
        if !has_at {
            st.put(key, at.number, value);
        }
        // The recent index keeps each block's writes sorted: binary search.
        let later: Vec<(u64, U256)> = st.recent.range(at.number + 1..).filter_map(|(n, w)| w.lookup(&key).map(|v| (*n, v))).collect();
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
                let later: Vec<(u64, B256, U256)> =
                    later.iter().filter_map(|(m, x)| st.chain.get(m).map(|b| (*m, b.hash, *x))).collect();
                let r = ar.write_base(&key, n, at.hash, v, &later);
                self.archive_try(r);
            }
        }
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
            resolved_from_bal: self.resolved_from_bal.load(std::sync::atomic::Ordering::Relaxed),
            bal_promotions: self.bal_promotions.load(std::sync::atomic::Ordering::Relaxed),
            on_demand_evicted: self.on_demand_evicted.load(std::sync::atomic::Ordering::Relaxed),
            complete_slots: st.complete.values().map(Complete::len).sum(),
            recent_bal_bytes: st.recent.values().map(|w| (w.writes.len() + w.touched.len() + w.storage_reads.len()) * BAL_ENTRY_BYTES).sum(),
            recent_writes: st.recent.values().map(|w| w.writes.len()).sum(),
            recent_reads: st.recent.values().map(|w| w.storage_reads.len()).sum(),
            recent_touched: st.recent.values().map(|w| w.touched.len()).sum(),
            recent_code_bytes: st.recent.values().map(|w| w.code.iter().map(|(_, c)| c.len()).sum::<usize>()).sum(),
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
    /// `BLOCKHASH(n)` for a simulation at `at`: only a hash established as an ancestor of `at`.
    /// In memory that is the canonical ring when `at` is on it. From the archive, a hash is
    /// used only if the archive links it to a block known to be an ancestor of `at` (the oldest
    /// in-memory hash above `n`, or `at` itself for an archived snapshot): an archived hash at
    /// the right height can belong to a fork the chain abandoned (e.g. before reconciliation
    /// after an offline reorg). Anything else is unknown, so the caller resolves headers.
    pub fn block_hash_at(&self, at: &BlockRef, n: u64) -> Option<B256> {
        if n > at.number {
            return None;
        }
        if n == at.number {
            return Some(at.hash);
        }
        let anchor = {
            let st = self.inner.read();
            let mem = |m: u64| st.chain.get(&m).map(|b| b.hash).or_else(|| st.hashes.get(&m).copied());
            if mem(at.number) == Some(at.hash) {
                if let Some(h) = mem(n) {
                    return Some(h);
                }
                // The nearest in-memory ancestor above `n` (canonical, like `at`).
                let k = st.hashes.range(n + 1..at.number).next().map(|(m, h)| (*m, *h));
                let c = st.chain.range(n + 1..at.number).next().map(|(m, b)| (*m, b.hash));
                match (k, c) {
                    (Some(a), Some(b)) => if a.0 <= b.0 { a } else { b },
                    (Some(a), None) | (None, Some(a)) => a,
                    (None, None) => (at.number, at.hash),
                }
            } else {
                (at.number, at.hash)
            }
        };
        self.archive.as_ref().and_then(|a| a.ancestor_hash(n, anchor).ok().flatten())
    }

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
    /// slots). The address isn't watched before this (a pending download keeps nothing): the
    /// writes between `at` and the head are replayed from the BAL index, which must cover all
    /// of them, or the download is too old (`OutOfWindow`: download again). Counts toward the
    /// subscribed-key limit as one key per slot.
    pub fn install_complete_storage(&self, at: &BlockRef, a: Address, slots: Vec<(B256, U256)>) -> Result<(), StateError> {
        let mut st = self.inner.write();
        if st.chain.get(&at.number).map(|b| b.hash) != Some(at.hash) {
            return Err(StateError::Reorged { number: at.number, hash: at.hash });
        }
        let head = st.head().map_or(at.number, |h| h.number);
        if head > at.number && st.recent.range(at.number + 1..=head).count() as u64 != head - at.number {
            return Err(StateError::OutOfWindow(at.number));
        }
        let later: Vec<(u64, StateKey, U256)> = st
            .recent
            .range(at.number + 1..)
            .flat_map(|(n, w)| w.writes.iter().filter(|(k, _)| matches!(k, StateKey::Storage(x, _) if *x == a)).map(move |(k, v)| (*n, *k, *v)))
            .collect();
        let mut c = Complete { block: at.number, slots: Arc::new(slots.into_iter().collect()), extra: HashSet::default() };
        for (_, k, _) in &later {
            if let StateKey::Storage(_, s) = k {
                c.note_write(*s);
            }
        }
        // Slots written between the download block and now count toward the budget too.
        let max = self.max_subscribed.load(std::sync::atomic::Ordering::Relaxed);
        let others: usize = st.complete.iter().filter(|(x, _)| **x != a).map(|(_, c)| c.len()).sum();
        if st.watched_keys.len() + others + c.len() > max {
            self.admissions_refused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Err(StateError::LimitReached(c.len()));
        }
        st.watched_addresses.insert(a);
        st.temp.retain(|k, _| match k {
            StateKey::Balance(x) | StateKey::Nonce(x) | StateKey::CodeHash(x) | StateKey::Storage(x, _) => *x != a,
        });
        for (n, k, v) in later {
            st.put(k, n, v);
        }
        st.dropped_complete.remove(&a);
        st.complete.insert(a, c);
        Ok(())
    }

    /// Slots held for a fully downloaded contract (downloaded plus written since).
    pub fn complete_storage_len(&self, a: &Address) -> Option<usize> {
        self.inner.read().complete.get(a).map(Complete::len)
    }

    /// Stops keeping a contract's whole storage (e.g. it grew past its limit): the address is
    /// no longer tracked, and its slots' versions are dropped unless watched individually.
    pub fn drop_complete_storage(&self, a: &Address) {
        self.inner.write().drop_complete(a);
    }

    /// Why a whole-storage contract was dropped by the store (subscribed-key budget), if it was.
    /// Taking it clears it.
    pub fn take_complete_drop(&self, a: &Address) -> Option<String> {
        self.inner.write().dropped_complete.remove(a)
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
            // A subscription (key or address) may own it now: its versions are not the TTL's.
            if !st.owned(&k) {
                st.versions.remove(&k);
            }
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
        // Not tracked, but a recent block wrote it: that write is its value at this block,
        // provided every block from the write to this one is indexed (a gap could hide a later
        // write). Saves a SNAP fetch for state that changed recently.
        let n = self.block.number;
        for (m, w) in st.recent.range(..=n).rev() {
            if let Some(v) = w.lookup(key) {
                if st.recent.range(m..=&n).count() as u64 == n - m + 1 {
                    self.store.resolved_from_bal.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return Ok(StateValue::Known(v));
                }
                break;
            }
        }
        Ok(StateValue::Unknown)
    }
}

#[cfg(test)]
mod tests;
