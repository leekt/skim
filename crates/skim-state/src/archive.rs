//! Selective state archive (PROJECT.md §7.6, M4): every version of *archived* keys, on disk.
//!
//! BALs carry absolute post-values, so the archive stores a key's base value (when it became
//! known) and then only the blocks that changed it:
//!
//! ```text
//! (slot X, 100) -> 10   base, coverage starts at 100
//! (slot X, 102) -> 20   BAL write; 101 and 103 store nothing
//! ```
//!
//! A read of key `K` at block `N` is answered only if:
//! - `N` lies in a **segment**: a run of consecutive blocks whose BALs were all processed
//!   (restarts without gap fill end a segment)
//! - `K` is **covered** at `N`: `N` lies in one of `K`'s coverage **intervals**, and its value
//!   was known at or before `N` within that segment
//!
//! Otherwise it is [`ArchiveRead::NotCovered`], never a guess. Reorgs truncate exactly what the
//! orphaned blocks wrote, using a per-block journal.
//!
//! ## Coverage intervals
//!
//! A key's coverage is a set of intervals. Each one promises: a version exists at its start,
//! and every write to the key in the interval, on the archived chain, is stored as a version.
//! - The **current** interval is `COVERAGE` (start) plus, once the key is released,
//!   `COVERAGE_END` (last block). Only it grows: writes, bases, backfill and gap fill move its
//!   start, as before.
//! - **Earlier** intervals are `INTERVALS` entries `(key, start) -> end`, closed when a released
//!   key is archived again. They never grow, only shrink when a reorg or reconciliation drops
//!   blocks they cover. A read in the gap between two intervals is refused: nothing recorded the
//!   key's writes there.
//!
//! **Format.** Archives written before intervals have no `INTERVALS` table, and their
//! `COVERAGE`/`COVERAGE_END` mean exactly what the current interval means now. [`Archive::open`]
//! creates the empty table, which is the whole migration: no existing entry is rewritten or
//! reinterpreted, and an older skim reading a newer archive sees only current intervals (it
//! refuses reads in earlier ones, never misanswers them).
//!
//! ## Durability and the store lock
//!
//! A redb commit fsyncs. The store applies archive writes while holding its own write lock (so
//! memory and archive change together), and a sync there would stall every reader once per
//! block. Instead a writer holds the archive's [`Hold`] (taken *before* the store lock): every
//! commit made under it is non-durable (visible to readers at once, no fsync), and the outermost
//! `Hold` makes them durable with one commit when it is dropped, after the store lock is
//! released. Calls made without a `Hold` (the healer, tests) commit durably as before.

use std::{cell::Cell, collections::HashSet, path::Path};

use alloy_primitives::{Address, B256, Bytes, U256};
use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition, WriteTransaction};

use crate::StateKey;

/// Encoded state key: tag (1) + address (20) + slot (32).
const K: usize = 53;
/// Version key: encoded state key + big-endian block number.
const VK: usize = K + 8;

const VERSIONS: TableDefinition<&[u8; VK], &[u8; 32]> = TableDefinition::new("versions");
/// Start of each key's current coverage interval.
const COVERAGE: TableDefinition<&[u8; K], u64> = TableDefinition::new("coverage");
const ARCHIVED: TableDefinition<&[u8; K], ()> = TableDefinition::new("archived");
const HEADERS: TableDefinition<u64, &[u8]> = TableDefinition::new("headers");
const HASHES: TableDefinition<u64, &[u8; 32]> = TableDefinition::new("hashes");
const BY_HASH: TableDefinition<&[u8; 32], u64> = TableDefinition::new("by_hash");
const CODE: TableDefinition<&[u8; 32], &[u8]> = TableDefinition::new("code");
const JOURNAL: TableDefinition<u64, &[u8]> = TableDefinition::new("journal");
/// Keys whose current coverage starts at a BAL **write** (not a base): the value before that
/// write is in no BAL, so their history can never be extended backwards.
const WRITE_BOUND: TableDefinition<&[u8; K], ()> = TableDefinition::new("write_bound");
/// Keys whose coverage-start block is known not to write them (a backfill scanned it), so the
/// value there also held just before it. A plain base is a post-state: the base block itself
/// may have changed the key, so extending backwards needs that block's BAL.
const PRE_CLEAN: TableDefinition<&[u8; K], ()> = TableDefinition::new("pre_clean");
/// Keys no longer archived: the last block their current coverage reaches. Reads above it are
/// not covered (later writes were not recorded). Archiving the key again closes this interval
/// into `INTERVALS` and starts a new current one.
const COVERAGE_END: TableDefinition<&[u8; K], u64> = TableDefinition::new("coverage_end");
/// Earlier, closed coverage intervals: (key, first block) -> last block (inclusive). Added with
/// the interval format; absent (so empty) in older archives.
const INTERVALS: TableDefinition<&[u8; VK], u64> = TableDefinition::new("coverage_intervals");
/// Segments: first block -> last block (inclusive) of each run of consecutive processed blocks.
const SEGMENTS: TableDefinition<u64, u64> = TableDefinition::new("segments");

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("archive db: {0}")]
    Db(String),
    #[error("block {0} does not extend the archive (last {1:?})")]
    NotNext(u64, Option<u64>),
    #[error("gap fill does not fit: {0}")]
    GapMismatch(String),
}

macro_rules! db_err {
    ($($t:ty),*) => { $(impl From<$t> for ArchiveError { fn from(e: $t) -> Self { Self::Db(e.to_string()) } })* };
}
db_err!(redb::DatabaseError, redb::TransactionError, redb::TableError, redb::StorageError, redb::CommitError, redb::SetDurabilityError);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveRead {
    Value(U256),
    /// The archive's block at that height is not the one asked for (a reorg replaced it).
    OtherFork,
    /// Not answerable at this block. `from` is where this key's current coverage starts, if
    /// anywhere.
    NotCovered { from: Option<u64> },
}

pub fn encode_key(k: &StateKey) -> [u8; K] {
    let mut out = [0u8; K];
    let (tag, a, slot): (u8, &Address, Option<&U256>) = match k {
        StateKey::Balance(a) => (1, a, None),
        StateKey::Nonce(a) => (2, a, None),
        StateKey::CodeHash(a) => (3, a, None),
        StateKey::Storage(a, s) => (4, a, Some(s)),
    };
    out[0] = tag;
    out[1..21].copy_from_slice(a.as_slice());
    if let Some(s) = slot {
        out[21..].copy_from_slice(&s.to_be_bytes::<32>());
    }
    out
}

pub fn decode_key(b: &[u8; K]) -> Option<StateKey> {
    let a = Address::from_slice(&b[1..21]);
    Some(match b[0] {
        1 => StateKey::Balance(a),
        2 => StateKey::Nonce(a),
        3 => StateKey::CodeHash(a),
        4 => StateKey::Storage(a, U256::from_be_slice(&b[21..])),
        _ => return None,
    })
}

fn vkey(k: &[u8; K], block: u64) -> [u8; VK] {
    let mut out = [0u8; VK];
    out[..K].copy_from_slice(k);
    out[K..].copy_from_slice(&block.to_be_bytes());
    out
}

fn vkey_block(v: &[u8; VK]) -> u64 {
    u64::from_be_bytes(v[K..].try_into().unwrap())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackfillReport {
    pub new_blocks: u64,
    /// Keys never written in the scanned range: coverage extended to its start.
    pub keys_unchanged: u64,
    /// Keys written in the range: coverage extended to their first write.
    pub keys_written: u64,
    /// Keys whose coverage starts beyond the scanned range (not extended).
    pub keys_unscanned: u64,
    /// Keys whose coverage starts at a write: never extendable.
    pub keys_write_bound: u64,
}

/// One processed block for the archive.
pub struct ArchiveBlock<'a> {
    pub number: u64,
    pub hash: B256,
    /// Coverage carries over from block `number - 1` only if this is its hash.
    pub parent_hash: B256,
    /// RLP-encoded header (execution environment for historical calls).
    pub header_rlp: &'a [u8],
    /// Final writes of this block to archived keys.
    pub writes: &'a [(StateKey, U256)],
}

/// Who may write the archive, and how many of its commits still await a durable one.
#[derive(Default)]
struct Gate {
    depth: Cell<u32>,
    dirty: Cell<bool>,
}

/// Exclusive write access to the archive for the calling thread (reentrant). Commits made while
/// a `Hold` is outstanding are non-durable; dropping the outermost one makes them durable with a
/// single synced commit. See the module docs: the store takes this *before* its own write lock,
/// so the sync happens after that lock is released and readers never wait on it.
///
/// Lock order: `Hold`, then the store lock. A thread holding the store's write lock never waits
/// for a `Hold` (it already has one), so a sync in progress can delay only other writers.
pub struct Hold<'a> {
    ar: &'a Archive,
    gate: parking_lot::ReentrantMutexGuard<'a, Gate>,
}

impl Hold<'_> {
    fn nested(&self) -> bool {
        self.gate.depth.get() > 1
    }
}

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        let d = self.gate.depth.get() - 1;
        self.gate.depth.set(d);
        if d == 0 && self.gate.dirty.get() {
            match self.ar.sync() {
                Ok(()) => self.gate.dirty.set(false),
                Err(e) => {
                    // Left dirty: the next outermost release retries. Until then the data is
                    // visible but a crash may revert it (see `Archive::sync`).
                    self.ar.sync_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tracing::warn!(error = %e, "archive: durable commit failed");
                }
            }
        }
    }
}

pub struct Archive {
    db: Database,
    gate: parking_lot::ReentrantMutex<Gate>,
    sync_errors: std::sync::atomic::AtomicU64,
    /// Runs inside `backfill`'s write transaction, after its checks (tests: interleavings).
    #[cfg(test)]
    pub(crate) hook: parking_lot::Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// Runs inside the durable commit of [`Archive::sync`], with redb's writer held (tests:
    /// readers must not wait for it).
    #[cfg(test)]
    pub(crate) sync_hook: parking_lot::Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl Archive {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ArchiveError> {
        // Bounded page cache (redb's default is 1 GiB): history reads are point lookups.
        let db = Database::builder().set_cache_size(64 << 20).create(path)?;
        // Creates any missing table. For an archive written before coverage intervals this adds
        // the empty INTERVALS table and touches nothing else: the old layout *is* the new one
        // with no earlier intervals (module docs, "Format").
        let tx = db.begin_write()?;
        tx.open_table(VERSIONS)?;
        tx.open_table(COVERAGE)?;
        tx.open_table(ARCHIVED)?;
        tx.open_table(HEADERS)?;
        tx.open_table(HASHES)?;
        tx.open_table(BY_HASH)?;
        tx.open_table(CODE)?;
        tx.open_table(JOURNAL)?;
        tx.open_table(SEGMENTS)?;
        tx.open_table(WRITE_BOUND)?;
        tx.open_table(PRE_CLEAN)?;
        tx.open_table(COVERAGE_END)?;
        tx.open_table(INTERVALS)?;
        tx.commit()?;
        Ok(Self {
            db,
            gate: Default::default(),
            sync_errors: Default::default(),
            #[cfg(test)]
            hook: Default::default(),
            #[cfg(test)]
            sync_hook: Default::default(),
        })
    }

    /// Takes the archive's write access for this thread until the guard drops (see [`Hold`]).
    pub fn hold(&self) -> Hold<'_> {
        let gate = self.gate.lock();
        gate.depth.set(gate.depth.get() + 1);
        Hold { ar: self, gate }
    }

    /// Durable commits that failed (their data stays visible, and is retried by the next one).
    pub fn sync_errors(&self) -> u64 {
        self.sync_errors.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Makes every earlier non-durable commit durable: an empty commit with
    /// [`Durability::Immediate`] persists them all (redb). Until it returns, a crash reverts the
    /// file to the last durable commit, which is the state before the writer's whole `Hold`
    /// (each holder syncs before releasing the gate, so at most one holder's commits are ever
    /// pending): the same as crashing a moment earlier.
    fn sync(&self) -> Result<(), ArchiveError> {
        let tx = self.db.begin_write()?;
        #[cfg(test)]
        if let Some(h) = self.sync_hook.lock().as_ref() {
            h();
        }
        tx.commit()?;
        Ok(())
    }

    /// One write transaction. Inside an outer [`Hold`] it commits without fsync (the holder syncs
    /// later); on its own it commits durably. An error aborts it (nothing is written).
    fn write<T>(&self, f: impl FnOnce(&WriteTransaction) -> Result<T, ArchiveError>) -> Result<T, ArchiveError> {
        let hold = self.hold();
        let nested = hold.nested();
        let out = {
            let mut tx = self.db.begin_write()?;
            if nested {
                tx.set_durability(Durability::None)?;
            }
            let out = f(&tx)?;
            tx.commit()?;
            out
        };
        // Nested: the holder makes it durable. Alone: this commit was durable, and so is
        // everything committed before it.
        hold.gate.dirty.set(nested);
        Ok(out)
    }

    pub fn archived_keys(&self) -> Result<HashSet<StateKey>, ArchiveError> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(ARCHIVED)?;
        let mut out = HashSet::new();
        for e in t.iter()? {
            let (k, _) = e?;
            if let Some(k) = decode_key(k.value()) {
                out.insert(k);
            }
        }
        Ok(out)
    }

    /// Ends the key's current interval: an ended one (`[start, end]`) is kept as an earlier
    /// interval, so its history stays readable; the current start and its flags are cleared.
    fn close_current(tx: &WriteTransaction, ek: &[u8; K]) -> Result<(), ArchiveError> {
        let start = tx.open_table(COVERAGE)?.remove(ek)?.map(|g| g.value());
        let end = tx.open_table(COVERAGE_END)?.remove(ek)?.map(|g| g.value());
        if let (Some(s), Some(e)) = (start, end)
            && e >= s
        {
            tx.open_table(INTERVALS)?.insert(&vkey(ek, s), e)?;
        }
        tx.open_table(WRITE_BOUND)?.remove(ek)?;
        tx.open_table(PRE_CLEAN)?.remove(ek)?;
        Ok(())
    }

    /// Marks keys as archived (idempotent). Their history starts at their first base or write.
    pub fn add_keys(&self, keys: &[StateKey]) -> Result<(), ArchiveError> {
        self.write(|tx| {
            for k in keys {
                let ek = encode_key(k);
                tx.open_table(ARCHIVED)?.insert(&ek, ())?;
                // Archived again after its coverage ended: the blocks since were not recorded,
                // so the ended interval is closed (still readable) and new coverage starts from
                // the next write rather than bridging that gap.
                if tx.open_table(COVERAGE_END)?.get(&ek)?.is_some() {
                    Self::close_current(tx, &ek)?;
                }
            }
            Ok(())
        })
    }

    /// Last block of the open segment.
    pub fn last_block(&self) -> Result<Option<u64>, ArchiveError> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(SEGMENTS)?;
        Ok(t.last()?.map(|(_, end)| end.value()))
    }

    pub fn segments(&self) -> Result<Vec<(u64, u64)>, ArchiveError> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(SEGMENTS)?;
        let mut out = Vec::new();
        for e in t.iter()? {
            let (s, e) = e?;
            out.push((s.value(), e.value()));
        }
        Ok(out)
    }

    /// Records a processed block. A block extends the open segment only if it is the next
    /// number *and* its parent is the archived block at the segment's end: coverage belongs to
    /// one chain, not to a run of block numbers. Anything else starts a new segment, so nothing
    /// carries over (e.g. the first block after a restart that followed an offline reorg); the
    /// healer reconciles such boundaries.
    pub fn write_block(&self, b: &ArchiveBlock<'_>) -> Result<(), ArchiveError> {
        self.write(|tx| {
            let end_hash = |end: u64| -> Result<Option<B256>, ArchiveError> {
                Ok(tx.open_table(HASHES)?.get(end)?.map(|g| B256::from(*g.value())))
            };
            let last = tx.open_table(SEGMENTS)?.last()?.map(|(s, e)| (s.value(), e.value()));
            let mut seg = tx.open_table(SEGMENTS)?;
            match last {
                Some((start, end)) if end + 1 == b.number && end_hash(end)? == Some(b.parent_hash) => {
                    seg.insert(start, b.number)?;
                }
                Some((_, end)) if end + 1 == b.number => {
                    tracing::warn!(block = b.number, "archive: block does not link to the archived chain; new segment, nothing carried over");
                    seg.insert(b.number, b.number)?;
                }
                Some((_, end)) if b.number <= end => return Err(ArchiveError::NotNext(b.number, Some(end))),
                _ => {
                    seg.insert(b.number, b.number)?;
                }
            }
            drop(seg);
            Self::write_entries(tx, b, None)
        })
    }

    /// Records that `ek` has a version at `block`, so a reorg below `block` removes it. Every
    /// version the archive stores must be journaled: BAL writes, bases, replayed writes and
    /// backfilled values alike.
    fn journal_add(tx: &WriteTransaction, block: u64, ek: &[u8; K]) -> Result<(), ArchiveError> {
        let mut j = tx.open_table(JOURNAL)?;
        let mut cur = j.get(block)?.map(|g| g.value().to_vec()).unwrap_or_default();
        if !cur.chunks_exact(K).any(|c| c == ek) {
            cur.extend_from_slice(ek);
            j.insert(block, cur.as_slice())?;
        }
        Ok(())
    }

    /// Stores a block's writes. `gap_before`: the block fills the gap before the segment that
    /// starts there, rather than extending the archive at its top.
    fn write_entries(tx: &WriteTransaction, b: &ArchiveBlock<'_>, gap_before: Option<u64>) -> Result<(), ArchiveError> {
        let mut versions = tx.open_table(VERSIONS)?;
        let mut cov = tx.open_table(COVERAGE)?;
        let mut journal = Vec::with_capacity(b.writes.len() * K);
        for (k, v) in b.writes {
            let ek = encode_key(k);
            versions.insert(&vkey(&ek, b.number), &v.to_be_bytes::<32>())?;
            let cur = cov.get(&ek)?.map(|g| g.value());
            // Coverage starts at the earliest block whose value is known. At the top of the
            // archive that is this write if nothing covers the key yet. A gap fill may move a
            // start back into the gap only if the key was covered from the very start of the
            // next segment: the gap's writes then join recorded ones. A start later than that
            // (or none) means blocks between the gap and it were not recorded for the key (it
            // wasn't archived, or was released), and coverage must not bridge them.
            let start_here = match (gap_before, cur) {
                (None, None) => true,
                (None, Some(c)) => b.number < c,
                (Some(ns), Some(c)) => b.number < c && c == ns,
                (Some(_), None) => false,
            };
            if start_here {
                cov.insert(&ek, b.number)?;
                tx.open_table(WRITE_BOUND)?.insert(&ek, ())?;
                tx.open_table(PRE_CLEAN)?.remove(&ek)?;
            }
            journal.extend_from_slice(&ek);
        }
        tx.open_table(JOURNAL)?.insert(b.number, journal.as_slice())?;
        tx.open_table(HEADERS)?.insert(b.number, b.header_rlp)?;
        tx.open_table(HASHES)?.insert(b.number, &b.hash.0)?;
        tx.open_table(BY_HASH)?.insert(&b.hash.0, b.number)?;
        Ok(())
    }

    /// Every archived key with its current coverage start, and whether that start is a base
    /// value (extendable backwards by a BAL scan) rather than a write (never extendable).
    pub fn coverages(&self) -> Result<Vec<(StateKey, u64, bool)>, ArchiveError> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(COVERAGE)?;
        let wb = tx.open_table(WRITE_BOUND)?;
        let ended = tx.open_table(COVERAGE_END)?;
        let mut out = Vec::new();
        for e in t.iter()? {
            let (k, c) = e?;
            if ended.get(k.value())?.is_some() {
                continue; // no longer archived: its history isn't extended any more
            }
            let extendable = wb.get(k.value())?.is_none();
            if let Some(key) = decode_key(k.value()) {
                out.push((key, c.value(), extendable));
            }
        }
        Ok(out)
    }

    /// Every coverage interval of `key`, oldest first, as `(first, last)`; `last` is `None` for
    /// a current interval that is still open.
    pub fn intervals(&self, key: &StateKey) -> Result<Vec<(u64, Option<u64>)>, ArchiveError> {
        let ek = encode_key(key);
        let tx = self.db.begin_read()?;
        let mut out = Vec::new();
        for e in tx.open_table(INTERVALS)?.range::<&[u8; VK]>(&vkey(&ek, 0)..=&vkey(&ek, u64::MAX))? {
            let (k, end) = e?;
            out.push((vkey_block(k.value()), Some(end.value())));
        }
        if let Some(c) = tx.open_table(COVERAGE)?.get(&ek)? {
            out.push((c.value(), tx.open_table(COVERAGE_END)?.get(&ek)?.map(|g| g.value())));
        }
        Ok(out)
    }

    /// Extends history backwards from a **fully scanned** run of BALs (M4.2).
    ///
    /// `blocks` are consecutive `[from, scan_end]`, each carrying *all* of that block's writes to
    /// archived keys. Blocks before the first segment's start `S` are new: their headers are
    /// recorded, and the last of them must be the parent of `S` (`s_parent`), so the segment
    /// grows backwards to `from`.
    ///
    /// For each archived key whose current coverage starts at `c` with `from < c <= scan_end + 1`:
    /// - **no write in `[from, c)`**: a BAL lists every write, so the value was constant; coverage
    ///   moves to `from` with the value at `c`
    /// - **writes**: they are recorded, and coverage moves to the first write (the value before it
    ///   isn't in any BAL)
    ///
    /// The scan lists every write in `[from, c)`, so this holds even across an earlier
    /// interval's release gap; the earlier interval then overlaps the current one, harmlessly
    /// (both promise only recorded writes).
    pub fn backfill(&self, blocks: &[ArchiveBlock<'_>], s_parent: Option<B256>, scope: &HashSet<StateKey>) -> Result<BackfillReport, ArchiveError> {
        let (Some(first), Some(last)) = (blocks.first(), blocks.last()) else { return Ok(BackfillReport::default()) };
        if blocks.windows(2).any(|w| w[1].number != w[0].number + 1 || w[1].parent_hash != w[0].hash) {
            return Err(ArchiveError::GapMismatch("backfill blocks are not a linked chain".into()));
        }
        let (from, scan_end) = (first.number, last.number);
        // Every check runs inside the write transaction that commits the result. Write
        // transactions are serialized, and a rollback is one too: it lands either before (the
        // checks see it) or after (it truncates what this wrote, through the journal).
        self.write(|tx| {
            let segs: Vec<(u64, u64)> =
                tx.open_table(SEGMENTS)?.iter()?.map(|e| e.map(|(s, e)| (s.value(), e.value()))).collect::<Result<_, _>>()?;
            let Some(&(s, e)) = segs.first() else {
                return Err(ArchiveError::GapMismatch("empty archive".into()));
            };
            // The scanned part at or after S must lie in the first segment.
            if scan_end >= s && scan_end > e {
                return Err(ArchiveError::GapMismatch(format!("scan end {scan_end} beyond the first segment (ends {e})")));
            }
            let hash_at = |n: u64| -> Result<Option<B256>, ArchiveError> { Ok(tx.open_table(HASHES)?.get(n)?.map(|g| B256::from(*g.value()))) };
            let extends = from < s;
            if extends {
                if scan_end + 1 < s {
                    return Err(ArchiveError::GapMismatch(format!("scan {from}..={scan_end} does not reach the segment start {s}")));
                }
                let link = blocks.iter().find(|b| b.number == s - 1).map(|b| b.hash);
                if link.is_none() || link != s_parent || hash_at(s)?.is_none() {
                    return Err(ArchiveError::GapMismatch("backfill does not link to the first segment".into()));
                }
            }
            // Blocks at or after S must be the ones the archive holds now.
            for b in blocks.iter().filter(|b| b.number >= s) {
                if hash_at(b.number)? != Some(b.hash) {
                    return Err(ArchiveError::GapMismatch(format!("block {} differs from the archived chain", b.number)));
                }
            }
            let covs: Vec<(StateKey, u64, bool)> = {
                let cov = tx.open_table(COVERAGE)?;
                let wb = tx.open_table(WRITE_BOUND)?;
                let ended = tx.open_table(COVERAGE_END)?;
                let mut out = Vec::new();
                for e in cov.iter()? {
                    let (k, c) = e?;
                    let ek = *k.value();
                    if ended.get(&ek)?.is_some() {
                        continue;
                    }
                    if let Some(key) = decode_key(&ek) {
                        out.push((key, c.value(), wb.get(&ek)?.is_none()));
                    }
                }
                out
            };
            #[cfg(test)]
            if let Some(h) = self.hook.lock().as_ref() {
                h();
            }
            let mut report = BackfillReport::default();
            if extends {
                let mut headers = tx.open_table(HEADERS)?;
                let mut hashes = tx.open_table(HASHES)?;
                let mut by_hash = tx.open_table(BY_HASH)?;
                let mut journal = tx.open_table(JOURNAL)?;
                for b in blocks.iter().filter(|b| b.number < s) {
                    headers.insert(b.number, b.header_rlp)?;
                    hashes.insert(b.number, &b.hash.0)?;
                    by_hash.insert(&b.hash.0, b.number)?;
                    journal.insert(b.number, &[][..])?;
                }
                report.new_blocks = s - from;
            }
            let mut versions = tx.open_table(VERSIONS)?;
            let mut cov = tx.open_table(COVERAGE)?;
            let mut wb = tx.open_table(WRITE_BOUND)?;
            let mut clean = tx.open_table(PRE_CLEAN)?;
            for (k, c, extendable) in covs {
                if c <= from {
                    continue;
                }
                // The scan kept only the writes of the keys archived when it started: a key
                // archived since has unscanned writes in these blocks. It waits for a later pass.
                if !scope.contains(&k) {
                    report.keys_unscanned += 1;
                    continue;
                }
                if !extendable {
                    // Coverage starts at a write: the value before it is unknowable from BALs.
                    report.keys_write_bound += 1;
                    continue;
                }
                let ek = encode_key(&k);
                // Did block c itself write k? From its BAL if scanned, else only if an earlier
                // backfill proved c clean. Unknown -> can't extend this time.
                let writes_k = |b: &ArchiveBlock<'_>| b.writes.iter().find(|(wk, _)| *wk == k).map(|(_, v)| *v);
                let wrote_at_c = match blocks.iter().find(|b| b.number == c) {
                    Some(b) => writes_k(b).is_some(),
                    None if clean.get(&ek)?.is_some() && c <= scan_end + 1 => false,
                    None => {
                        report.keys_unscanned += 1;
                        continue;
                    }
                };
                let writes: Vec<(u64, U256)> =
                    blocks.iter().filter(|b| b.number < c).filter_map(|b| writes_k(b).map(|v| (b.number, v))).collect();
                if writes.is_empty() && wrote_at_c {
                    // The base is block c's post-state and c changed it: the earlier value is in
                    // no BAL we have. Coverage stays at c, and can never be extended.
                    wb.insert(&ek, ())?;
                    clean.remove(&ek)?;
                    report.keys_write_bound += 1;
                    continue;
                }
                if writes.is_empty() {
                    let at_c = versions.get(&vkey(&ek, c))?.map(|g| *g.value());
                    let Some(v) = at_c else {
                        report.keys_unscanned += 1;
                        continue;
                    };
                    versions.insert(&vkey(&ek, from), &v)?;
                    Self::journal_add(tx, from, &ek)?;
                    cov.insert(&ek, from)?;
                    clean.insert(&ek, ())?; // `from` was scanned and doesn't write k
                    report.keys_unchanged += 1;
                } else {
                    for (n, v) in &writes {
                        versions.insert(&vkey(&ek, *n), &v.to_be_bytes::<32>())?;
                        Self::journal_add(tx, *n, &ek)?;
                    }
                    cov.insert(&ek, writes[0].0)?;
                    wb.insert(&ek, ())?;
                    clean.remove(&ek)?;
                    report.keys_written += 1;
                }
            }
            if extends {
                let mut seg = tx.open_table(SEGMENTS)?;
                seg.remove(s)?;
                seg.insert(from, e)?;
            }
            Ok(report)
        })
    }

    /// Fills the gap between two segments with the missing blocks (in order) and merges them.
    /// `blocks` must start right after a segment's end, end right before the next segment's
    /// start, and link by hash to both: `first_parent` = hash at the previous end, and the last
    /// block's hash = parent of the next segment's first header (checked by `next_parent`).
    pub fn fill_gap(&self, blocks: &[ArchiveBlock<'_>], scope: &HashSet<StateKey>) -> Result<(), ArchiveError> {
        let (Some(first), Some(last)) = (blocks.first(), blocks.last()) else { return Ok(()) };
        if blocks.windows(2).any(|w| w[1].number != w[0].number + 1 || w[1].parent_hash != w[0].hash) {
            return Err(ArchiveError::GapMismatch("gap blocks are not a linked chain".into()));
        }
        // Checks inside the committing write transaction (see `backfill`).
        self.write(|tx| {
            let segs: Vec<(u64, u64)> =
                tx.open_table(SEGMENTS)?.iter()?.map(|e| e.map(|(s, e)| (s.value(), e.value()))).collect::<Result<_, _>>()?;
            let prev = segs.iter().find(|(_, e)| *e + 1 == first.number).copied();
            let next = segs.iter().find(|(s, _)| *s == last.number + 1).copied();
            let (Some((ps, _)), Some((ns, ne))) = (prev, next) else {
                return Err(ArchiveError::GapMismatch(format!("{}..={} is not between two segments", first.number, last.number)));
            };
            let hash_at = |n: u64| -> Result<Option<B256>, ArchiveError> { Ok(tx.open_table(HASHES)?.get(n)?.map(|g| B256::from(*g.value()))) };
            if hash_at(first.number - 1)? != Some(first.parent_hash) {
                return Err(ArchiveError::GapMismatch("first block does not link to the previous segment".into()));
            }
            let next_rlp = tx.open_table(HEADERS)?.get(ns)?.map(|g| g.value().to_vec());
            let next_parent =
                next_rlp.and_then(|r| <alloy_consensus::Header as alloy_rlp::Decodable>::decode(&mut r.as_slice()).ok()).map(|h| h.parent_hash);
            if next_parent != Some(last.hash) {
                return Err(ArchiveError::GapMismatch("last block does not link to the next segment".into()));
            }
            // The blocks carry the writes of the keys archived when the scan started. A key
            // archived since and covered from before the end of the gap would get no versions
            // for it: retry with a fresh scope instead of merging.
            // Keys whose coverage ended (and earlier intervals) are outside the scan too: an
            // interval spanning the gap must stop before it (its blocks carry none of their
            // writes).
            Self::clamp_intervals(tx, first.number, last.number)?;
            {
                let cov = tx.open_table(COVERAGE)?;
                let ended = tx.open_table(COVERAGE_END)?;
                for e in cov.iter()? {
                    let (k, c) = e?;
                    if ended.get(k.value())?.is_some() {
                        continue;
                    }
                    if c.value() <= last.number && decode_key(k.value()).is_some_and(|key| !scope.contains(&key)) {
                        return Err(ArchiveError::GapMismatch("archived key set changed during the scan; retry".into()));
                    }
                }
            }
            for b in blocks {
                Self::write_entries(tx, b, Some(ns))?;
            }
            let mut seg = tx.open_table(SEGMENTS)?;
            seg.remove(ns)?;
            seg.insert(ps, ne)?;
            Ok(())
        })
    }

    /// Reconciliation after a fork the archive missed (e.g. a reorg while skim was offline):
    /// forgets blocks `lo..=hi`, the tail of the segment that holds them, which then ends at
    /// `lo - 1` (or disappears). A key whose current coverage started in those blocks keeps
    /// coverage only from its next archived version above them (write-bound), if any.
    pub fn truncate_range(&self, lo: u64, hi: u64) -> Result<(), ArchiveError> {
        self.write(|tx| {
            {
                let mut seg = tx.open_table(SEGMENTS)?;
                let owner = seg.range(..=lo)?.next_back().transpose()?.map(|(s, e)| (s.value(), e.value()));
                let Some((s, _)) = owner.filter(|(_, e)| *e == hi) else {
                    return Err(ArchiveError::GapMismatch(format!("{lo}..={hi} is not the tail of a segment")));
                };
                if s >= lo {
                    seg.remove(s)?;
                } else {
                    seg.insert(s, lo - 1)?;
                }
            }
            Self::clamp_intervals(tx, lo, hi)?;
            let mut journal = tx.open_table(JOURNAL)?;
            let mut versions = tx.open_table(VERSIONS)?;
            let mut cov = tx.open_table(COVERAGE)?;
            let mut headers = tx.open_table(HEADERS)?;
            let mut hashes = tx.open_table(HASHES)?;
            let mut by_hash = tx.open_table(BY_HASH)?;
            let blocks: Vec<(u64, Vec<u8>)> =
                journal.range(lo..=hi)?.map(|e| e.map(|(b, j)| (b.value(), j.value().to_vec()))).collect::<Result<_, _>>()?;
            for (b, j) in &blocks {
                for ek in j.chunks_exact(K) {
                    let ek: [u8; K] = ek.try_into().unwrap();
                    versions.remove(&vkey(&ek, *b))?;
                }
                journal.remove(*b)?;
                headers.remove(*b)?;
                if let Some(h) = hashes.remove(*b)? {
                    by_hash.remove(h.value())?;
                }
            }
            let starts: Vec<[u8; K]> =
                cov.iter()?.filter_map(|e| e.ok()).filter(|(_, c)| (lo..=hi).contains(&c.value())).map(|(k, _)| *k.value()).collect();
            for ek in starts {
                let next = versions
                    .range::<&[u8; VK]>(&vkey(&ek, hi + 1)..=&vkey(&ek, u64::MAX))?
                    .next()
                    .transpose()?
                    .map(|(k, _)| vkey_block(k.value()));
                match next {
                    Some(n) => {
                        cov.insert(&ek, n)?;
                        tx.open_table(WRITE_BOUND)?.insert(&ek, ())?;
                        tx.open_table(PRE_CLEAN)?.remove(&ek)?;
                    }
                    None => {
                        cov.remove(&ek)?;
                    }
                }
            }
            Ok(())
        })
    }

    /// Stops archiving `key`: its history stays readable up to `last` (the last block whose
    /// writes were recorded for it), and reads above that are not covered. With `last` before
    /// the coverage start (or `None`), nothing remains covered by the current interval (earlier
    /// intervals stay).
    pub fn end_coverage(&self, key: &StateKey, last: Option<u64>) -> Result<(), ArchiveError> {
        let ek = encode_key(key);
        self.write(|tx| {
            tx.open_table(ARCHIVED)?.remove(&ek)?;
            let mut cov = tx.open_table(COVERAGE)?;
            let start = cov.get(&ek)?.map(|g| g.value());
            match (start, last) {
                (Some(c), Some(l)) if l >= c => {
                    tx.open_table(COVERAGE_END)?.insert(&ek, l)?;
                }
                _ => {
                    cov.remove(&ek)?;
                    tx.open_table(COVERAGE_END)?.remove(&ek)?;
                }
            }
            Ok(())
        })
    }

    /// Blocks `lo..=hi` are being dropped or replaced, and the intervals that don't grow (ended
    /// current ones and earlier ones) won't get their writes recorded there: an interval that
    /// reaches into them now ends at `lo - 1`, or is dropped if it starts in them. Intervals
    /// entirely above `hi` are untouched.
    fn clamp_intervals(tx: &WriteTransaction, lo: u64, hi: u64) -> Result<(), ArchiveError> {
        let clamp = |s: u64, e: u64| -> Option<Option<u64>> {
            // None: untouched; Some(None): dropped; Some(Some(l)): ends at l.
            (s <= hi && e >= lo).then(|| (s < lo).then(|| lo - 1))
        };
        {
            let mut end = tx.open_table(COVERAGE_END)?;
            let mut cov = tx.open_table(COVERAGE)?;
            let ended: Vec<([u8; K], u64)> =
                end.iter()?.map(|e| e.map(|(k, l)| (*k.value(), l.value()))).collect::<Result<_, _>>()?;
            for (ek, l) in ended {
                let Some(c) = cov.get(&ek)?.map(|g| g.value()) else {
                    end.remove(&ek)?; // an end without a start covers nothing
                    continue;
                };
                match clamp(c, l) {
                    None => {}
                    Some(Some(l)) => {
                        end.insert(&ek, l)?;
                    }
                    Some(None) => {
                        end.remove(&ek)?;
                        cov.remove(&ek)?;
                    }
                }
            }
        }
        let mut iv = tx.open_table(INTERVALS)?;
        let all: Vec<([u8; VK], u64)> = iv.iter()?.map(|e| e.map(|(k, l)| (*k.value(), l.value()))).collect::<Result<_, _>>()?;
        for (k, e) in all {
            match clamp(vkey_block(&k), e) {
                None => {}
                Some(Some(l)) => {
                    iv.insert(&k, l)?;
                }
                Some(None) => {
                    iv.remove(&k)?;
                }
            }
        }
        Ok(())
    }

    /// A key's value became known at block `block` with hash `hash` (e.g. a verified SNAP base),
    /// plus its writes in later blocks `(number, hash, value)`. Every block must be the one the
    /// archive holds at that height (checked in the committing transaction): a value from another
    /// fork is never stored under this fork's header. Coverage starts at the earliest such block.
    pub fn write_base(&self, key: &StateKey, block: u64, hash: B256, value: U256, later: &[(u64, B256, U256)]) -> Result<(), ArchiveError> {
        let ek = encode_key(key);
        self.write(|tx| {
            {
                let hashes = tx.open_table(HASHES)?;
                for (n, h) in std::iter::once((block, hash)).chain(later.iter().map(|(n, h, _)| (*n, *h))) {
                    if hashes.get(n)?.map(|g| B256::from(*g.value())) != Some(h) {
                        return Err(ArchiveError::GapMismatch(format!("block {n} ({h}) is not the archived block at that height")));
                    }
                }
            }
            let later: Vec<(u64, U256)> = later.iter().map(|(n, _, v)| (*n, *v)).collect();
            {
                let mut versions = tx.open_table(VERSIONS)?;
                // A BAL write at `block` is authoritative; don't overwrite it with the base.
                if versions.get(&vkey(&ek, block))?.is_none() {
                    versions.insert(&vkey(&ek, block), &value.to_be_bytes::<32>())?;
                }
                for (n, v) in &later {
                    versions.insert(&vkey(&ek, *n), &v.to_be_bytes::<32>())?;
                }
            }
            Self::journal_add(tx, block, &ek)?;
            for (n, _) in &later {
                Self::journal_add(tx, *n, &ek)?;
            }
            // Coverage that had ended restarts here: the blocks between were not recorded. The
            // ended interval is kept as an earlier one.
            let restart = tx.open_table(COVERAGE_END)?.get(&ek)?.is_some();
            if restart {
                Self::close_current(tx, &ek)?;
            }
            let mut cov = tx.open_table(COVERAGE)?;
            let cur = cov.get(&ek)?.map(|g| g.value());
            if cur.is_none_or(|c| block < c) {
                cov.insert(&ek, block)?;
                tx.open_table(WRITE_BOUND)?.remove(&ek)?;
                tx.open_table(PRE_CLEAN)?.remove(&ek)?;
            }
            tx.open_table(ARCHIVED)?.insert(&ek, ())?;
            Ok(())
        })
    }

    pub fn put_code(&self, hash: B256, code: &[u8]) -> Result<(), ArchiveError> {
        self.write(|tx| {
            tx.open_table(CODE)?.insert(&hash.0, code)?;
            Ok(())
        })
    }

    pub fn code(&self, hash: &B256) -> Result<Option<Bytes>, ArchiveError> {
        let tx = self.db.begin_read()?;
        Ok(tx.open_table(CODE)?.get(&hash.0)?.map(|g| Bytes::copy_from_slice(g.value())))
    }

    pub fn header_rlp(&self, number: u64) -> Result<Option<Vec<u8>>, ArchiveError> {
        let tx = self.db.begin_read()?;
        Ok(tx.open_table(HEADERS)?.get(number)?.map(|g| g.value().to_vec()))
    }

    pub fn hash(&self, number: u64) -> Result<Option<B256>, ArchiveError> {
        let tx = self.db.begin_read()?;
        Ok(tx.open_table(HASHES)?.get(number)?.map(|g| B256::from(*g.value())))
    }

    /// The archived hash at `n`, only if the archive proves it an ancestor of `anchor` (a block
    /// the caller knows is on the chain it asks about): `n` and the anchor are in one segment
    /// (every block in a segment is the parent of the next) and the anchor is archived under
    /// that hash. Read in one transaction, so a concurrent truncation can't split the check.
    pub fn ancestor_hash(&self, n: u64, anchor: (u64, B256)) -> Result<Option<B256>, ArchiveError> {
        let (k, kh) = anchor;
        if n > k {
            return Ok(None);
        }
        let tx = self.db.begin_read()?;
        let segs = tx.open_table(SEGMENTS)?;
        let same_segment = segs.range(..=n)?.next_back().transpose()?.is_some_and(|(_, e)| e.value() >= k);
        if !same_segment {
            return Ok(None);
        }
        let hashes = tx.open_table(HASHES)?;
        if hashes.get(k)?.map(|g| B256::from(*g.value())) != Some(kh) {
            return Ok(None);
        }
        Ok(hashes.get(n)?.map(|g| B256::from(*g.value())))
    }

    pub fn number_of(&self, hash: &B256) -> Result<Option<u64>, ArchiveError> {
        let tx = self.db.begin_read()?;
        Ok(tx.open_table(BY_HASH)?.get(&hash.0)?.map(|g| g.value()))
    }

    /// Reorg: forget blocks above `n` (their versions, headers, journal) and end the segment at
    /// `n`. Coverage that started in a forgotten block is withdrawn.
    pub fn truncate_above(&self, n: u64) -> Result<(), ArchiveError> {
        self.write(|tx| {
            // The blocks above `n` get replaced, and an ended key's writes in them won't be
            // recorded (nor an earlier interval's).
            Self::clamp_intervals(tx, n + 1, u64::MAX)?;
            let mut journal = tx.open_table(JOURNAL)?;
            let mut versions = tx.open_table(VERSIONS)?;
            let mut cov = tx.open_table(COVERAGE)?;
            let mut headers = tx.open_table(HEADERS)?;
            let mut hashes = tx.open_table(HASHES)?;
            let mut by_hash = tx.open_table(BY_HASH)?;
            let blocks: Vec<(u64, Vec<u8>)> =
                journal.range((n + 1)..)?.map(|e| e.map(|(b, j)| (b.value(), j.value().to_vec()))).collect::<Result<_, _>>()?;
            for (b, j) in &blocks {
                for ek in j.chunks_exact(K) {
                    let ek: &[u8; K] = ek.try_into().unwrap();
                    versions.remove(&vkey(ek, *b))?;
                    if cov.get(ek)?.is_some_and(|c| c.value() == *b) {
                        cov.remove(ek)?;
                    }
                }
                journal.remove(*b)?;
                headers.remove(*b)?;
                if let Some(h) = hashes.remove(*b)? {
                    by_hash.remove(h.value())?;
                }
            }
            // Coverage that starts above `n` describes forgotten blocks, whatever wrote it.
            let stale: Vec<[u8; K]> =
                cov.iter()?.filter_map(|e| e.ok()).filter(|(_, c)| c.value() > n).map(|(k, _)| *k.value()).collect();
            for ek in &stale {
                cov.remove(ek)?;
            }
            let mut seg = tx.open_table(SEGMENTS)?;
            let open: Vec<(u64, u64)> = seg.iter()?.map(|e| e.map(|(s, e)| (s.value(), e.value()))).collect::<Result<_, _>>()?;
            for (s, e) in open {
                if s > n {
                    seg.remove(s)?;
                } else if e > n {
                    seg.insert(s, n)?;
                }
            }
            Ok(())
        })
    }

    /// The value of `key` at block `number`, if the archive covers it.
    pub fn get(&self, key: &StateKey, number: u64) -> Result<ArchiveRead, ArchiveError> {
        self.read(key, number, None)
    }

    /// [`Self::get`] for a specific block: the hash check and the value come from the same read
    /// transaction, so a reorg can't slip between them.
    pub fn get_at(&self, key: &StateKey, number: u64, hash: B256) -> Result<ArchiveRead, ArchiveError> {
        self.read(key, number, Some(hash))
    }

    fn read(&self, key: &StateKey, number: u64, expect: Option<B256>) -> Result<ArchiveRead, ArchiveError> {
        let ek = encode_key(key);
        let tx = self.db.begin_read()?;
        if let Some(h) = expect
            && tx.open_table(HASHES)?.get(number)?.map(|g| B256::from(*g.value())) != Some(h)
        {
            return Ok(ArchiveRead::OtherFork);
        }
        let from = tx.open_table(COVERAGE)?.get(&ek)?.map(|g| g.value());
        // The interval holding `number`: the current one, else an earlier one.
        let current = from.filter(|c| *c <= number);
        let current = match current {
            Some(_) if tx.open_table(COVERAGE_END)?.get(&ek)?.is_some_and(|l| number > l.value()) => None,
            c => c,
        };
        let start = match current {
            Some(c) => Some(c),
            None => {
                let iv = tx.open_table(INTERVALS)?;
                let r = iv.range::<&[u8; VK]>(&vkey(&ek, 0)..=&vkey(&ek, number))?.next_back().transpose()?;
                r.map(|(k, e)| (vkey_block(k.value()), e.value())).filter(|(_, e)| number <= *e).map(|(s, _)| s)
            }
        };
        let Some(start) = start else {
            return Ok(ArchiveRead::NotCovered { from });
        };
        let seg_start = {
            let seg = tx.open_table(SEGMENTS)?;
            let r = seg.range(..=number)?.next_back().transpose()?;
            r.map(|(s, e)| (s.value(), e.value())).filter(|(_, e)| number <= *e).map(|(s, _)| s)
        };
        let Some(seg_start) = seg_start else {
            return Ok(ArchiveRead::NotCovered { from });
        };
        // The newest version at or below `number`, and it must lie within this segment and this
        // interval (an interval starts at a version, so within the interval it is always found).
        let v = tx.open_table(VERSIONS)?;
        let found = v.range::<&[u8; VK]>(&vkey(&ek, 0)..=&vkey(&ek, number))?.next_back().transpose()?;
        match found {
            // A version before the segment predates a gap: writes in the gap are unknown.
            Some((k, val)) if vkey_block(k.value()) >= seg_start.max(start) => Ok(ArchiveRead::Value(U256::from_be_bytes(*val.value()))),
            _ => Ok(ArchiveRead::NotCovered { from }),
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use redb::{ReadableTableMetadata, TableHandle};

    const A: Address = Address::repeat_byte(0xaa);

    fn slot(i: u64) -> StateKey {
        StateKey::Storage(A, U256::from(i))
    }

    fn blk(a: &Archive, n: u64, writes: &[(StateKey, u64)]) {
        let w: Vec<_> = writes.iter().map(|(k, v)| (*k, U256::from(*v))).collect();
        a.write_block(&ArchiveBlock { number: n, hash: B256::with_last_byte(n as u8), parent_hash: B256::with_last_byte((n - 1) as u8), header_rlp: &[0xc0], writes: &w }).unwrap();
    }

    fn val(a: &Archive, k: StateKey, n: u64) -> Option<u64> {
        match a.get(&k, n).unwrap() {
            ArchiveRead::Value(v) => Some(v.to()),
            ArchiveRead::NotCovered { .. } | ArchiveRead::OtherFork => None,
        }
    }

    #[test]
    fn stores_only_changes_and_answers_any_covered_block() {
        let d = tempfile::tempdir().unwrap();
        let a = Archive::open(d.path().join("a.redb")).unwrap();
        blk(&a, 100, &[]);
        a.write_base(&slot(1), 100, B256::with_last_byte(100), U256::from(10), &[]).unwrap();
        blk(&a, 101, &[]);
        blk(&a, 102, &[(slot(1), 20)]);
        blk(&a, 103, &[]);
        assert_eq!(val(&a, slot(1), 99), None); // before coverage
        assert_eq!(val(&a, slot(1), 100), Some(10));
        assert_eq!(val(&a, slot(1), 101), Some(10));
        assert_eq!(val(&a, slot(1), 102), Some(20));
        assert_eq!(val(&a, slot(1), 103), Some(20));
        assert_eq!(val(&a, slot(1), 104), None); // beyond the archive
        assert_eq!(val(&a, slot(2), 102), None); // never archived
    }

    #[test]
    fn late_discovered_key_is_not_backdated() {
        let d = tempfile::tempdir().unwrap();
        let a = Archive::open(d.path().join("a.redb")).unwrap();
        for n in 1..=5 {
            blk(&a, n, &[]);
        }
        a.write_base(&slot(3), 4, B256::with_last_byte(4), U256::from(7), &[]).unwrap();
        assert_eq!(val(&a, slot(3), 3), None);
        assert!(matches!(a.get(&slot(3), 3).unwrap(), ArchiveRead::NotCovered { from: Some(4) }));
        assert_eq!(val(&a, slot(3), 5), Some(7));
    }

    #[test]
    fn reorg_truncates_exactly_the_orphaned_writes() {
        let d = tempfile::tempdir().unwrap();
        let a = Archive::open(d.path().join("a.redb")).unwrap();
        blk(&a, 1, &[]);
        a.write_base(&slot(1), 1, B256::with_last_byte(1), U256::from(1), &[]).unwrap();
        blk(&a, 2, &[(slot(1), 2)]);
        blk(&a, 3, &[(slot(1), 3), (slot(9), 9)]);
        a.truncate_above(2).unwrap();
        assert_eq!(val(&a, slot(1), 2), Some(2));
        assert_eq!(val(&a, slot(1), 3), None); // block 3 forgotten
        assert_eq!(a.last_block().unwrap(), Some(2));
        assert!(matches!(a.get(&slot(9), 2).unwrap(), ArchiveRead::NotCovered { from: None }));
        blk(&a, 3, &[(slot(1), 33)]); // the new branch
        assert_eq!(val(&a, slot(1), 3), Some(33));
    }

    #[test]
    fn reorg_removes_late_base_and_replayed_versions() {
        // Review finding: versions written by write_base (a key archived after the fact, with
        // the writes replayed since) were not journaled, so a reorg kept the old fork's value.
        let d = tempfile::tempdir().unwrap();
        let a = Archive::open(d.path().join("a.redb")).unwrap();
        blk(&a, 100, &[]);
        blk(&a, 101, &[]);
        a.write_base(&slot(1), 100, B256::with_last_byte(100), U256::from(10), &[(101, B256::with_last_byte(101), U256::from(20))]).unwrap();
        assert_eq!(val(&a, slot(1), 101), Some(20));
        a.truncate_above(100).unwrap();
        blk(&a, 101, &[]); // 101' doesn't touch the key
        assert_eq!(val(&a, slot(1), 101), Some(10));
        // A base above the rollback point is withdrawn entirely, not left dangling.
        a.write_base(&slot(2), 101, B256::with_last_byte(101), U256::from(5), &[]).unwrap();
        a.truncate_above(100).unwrap();
        blk(&a, 101, &[]);
        assert_eq!(val(&a, slot(2), 101), None);
    }

    #[test]
    fn backfill_extends_unchanged_keys_fully_and_written_keys_from_their_first_write() {
        let d = tempfile::tempdir().unwrap();
        let a = Archive::open(d.path().join("a.redb")).unwrap();
        // Archive starts at 100: slot 1 = 10, slot 2 = 20 (bases); slot 3 discovered late at 103.
        blk(&a, 100, &[]);
        a.write_base(&slot(1), 100, B256::with_last_byte(100), U256::from(10), &[]).unwrap();
        a.write_base(&slot(2), 100, B256::with_last_byte(100), U256::from(20), &[]).unwrap();
        for n in 101..=104 {
            blk(&a, n, &[]);
        }
        a.write_base(&slot(3), 103, B256::with_last_byte(103), U256::from(30), &[]).unwrap();
        // Scan BALs 90..=103 (through slot 3's base block): slot 2 written at 95 (=21) and 97
        // (=20); slot 3 written at 101 (=30).
        let (w95, w97, w101) = ([(slot(2), U256::from(21))], [(slot(2), U256::from(20))], [(slot(3), U256::from(30))]);
        let hashes: Vec<B256> = (90..=103u64).map(|n| B256::with_last_byte(n as u8)).collect();
        let blocks: Vec<ArchiveBlock<'_>> = (90..=103u64)
            .map(|n| ArchiveBlock {
                number: n,
                hash: hashes[(n - 90) as usize],
                parent_hash: B256::with_last_byte((n - 1) as u8),
                header_rlp: &[0xc0],
                writes: match n { 95 => &w95, 97 => &w97, 101 => &w101, _ => &[] },
            })
            .collect();
        // Must link to the first segment: block 99 is the parent of 100.
        assert!(a.backfill(&blocks, Some(B256::with_last_byte(1)), &a.archived_keys().unwrap()).is_err());
        let r = a.backfill(&blocks, Some(B256::with_last_byte(99)), &a.archived_keys().unwrap()).unwrap();
        assert_eq!(r, BackfillReport { new_blocks: 10, keys_unchanged: 1, keys_written: 2, keys_unscanned: 0, keys_write_bound: 0 });
        assert_eq!(a.segments().unwrap(), vec![(90, 104)]);
        // slot 1: never written -> constant back to 90.
        assert_eq!(val(&a, slot(1), 90), Some(10));
        // slot 2: written at 95 and 97 -> known from 95 only.
        assert_eq!(val(&a, slot(2), 94), None);
        assert_eq!(val(&a, slot(2), 95), Some(21));
        assert_eq!(val(&a, slot(2), 96), Some(21));
        assert_eq!(val(&a, slot(2), 98), Some(20));
        // slot 3 (late key): its earlier write at 101 now covers 101..103.
        assert_eq!(val(&a, slot(3), 100), None);
        assert_eq!(val(&a, slot(3), 101), Some(30));
        assert_eq!(val(&a, slot(3), 102), Some(30));

        // Regression: a second, deeper backfill must not treat a first *write* as a base and
        // extend it backwards (its pre-write value is in no BAL).
        let deeper: Vec<ArchiveBlock<'_>> = (85..=89u64)
            .map(|n| ArchiveBlock { number: n, hash: B256::with_last_byte(n as u8), parent_hash: B256::with_last_byte((n - 1) as u8), header_rlp: &[0xc0], writes: &[] })
            .collect();
        let r2 = a.backfill(&deeper, Some(B256::with_last_byte(89)), &a.archived_keys().unwrap()).unwrap();
        assert_eq!(r2.keys_write_bound, 2);
        assert_eq!(val(&a, slot(2), 85), None);
        assert_eq!(val(&a, slot(2), 94), None);
        assert_eq!(val(&a, slot(3), 100), None);
        assert_eq!(val(&a, slot(1), 85), Some(10)); // the unchanged key keeps extending
    }

    #[test]
    fn reads_for_a_replaced_block_are_refused() {
        // Review finding: archived reads went by number only, so a handle to A100 read B100.
        let d = tempfile::tempdir().unwrap();
        let a = Archive::open(d.path().join("a.redb")).unwrap();
        blk(&a, 99, &[]);
        a.write_base(&slot(1), 99, B256::with_last_byte(99), U256::from(1), &[]).unwrap();
        blk(&a, 100, &[(slot(1), 2)]);
        let a100 = B256::with_last_byte(100);
        assert_eq!(a.get_at(&slot(1), 100, a100).unwrap(), ArchiveRead::Value(U256::from(2)));
        a.truncate_above(99).unwrap();
        let b100 = B256::repeat_byte(0xb0);
        a.write_block(&ArchiveBlock { number: 100, hash: b100, parent_hash: B256::with_last_byte(99), header_rlp: &[0xc0], writes: &[(slot(1), U256::from(3))] }).unwrap();
        assert_eq!(a.get_at(&slot(1), 100, a100).unwrap(), ArchiveRead::OtherFork);
        assert_eq!(a.get_at(&slot(1), 100, b100).unwrap(), ArchiveRead::Value(U256::from(3)));
    }

    #[test]
    fn backfill_checks_the_base_block_itself() {
        // Review finding: K was 10 in 90..=99 and became 20 at block 100. K was archived late,
        // with base (100, 20). The old backfill scanned only < 100, saw no write, and claimed
        // K = 20 back to 90.
        let d = tempfile::tempdir().unwrap();
        let a = Archive::open(d.path().join("a.redb")).unwrap();
        for n in 100..=103 {
            blk(&a, n, &[]); // K not archived yet when these were written
        }
        a.write_base(&slot(1), 100, B256::with_last_byte(100), U256::from(20), &[]).unwrap(); // late key, base at 100
        a.write_base(&slot(2), 100, B256::with_last_byte(100), U256::from(7), &[]).unwrap(); // late key, unchanged since 90
        let w100 = [(slot(1), U256::from(20))];
        let blocks: Vec<ArchiveBlock<'_>> = (90..=100u64)
            .map(|n| ArchiveBlock { number: n, hash: B256::with_last_byte(n as u8), parent_hash: B256::with_last_byte((n - 1) as u8), header_rlp: &[0xc0], writes: if n == 100 { &w100 } else { &[] } })
            .collect();
        let r = a.backfill(&blocks, Some(B256::with_last_byte(99)), &a.archived_keys().unwrap()).unwrap();
        assert_eq!(r.keys_write_bound, 1);
        assert_eq!(val(&a, slot(1), 95), None); // was 10: refuse, never 20
        assert_eq!(val(&a, slot(1), 100), Some(20));
        assert_eq!(val(&a, slot(2), 90), Some(7)); // block 100 didn't write it: extends
        // A deeper pass needn't rescan 90: it was proven clean for slot 2.
        let deeper: Vec<ArchiveBlock<'_>> =
            (85..=89u64).map(|n| ArchiveBlock { number: n, hash: B256::with_last_byte(n as u8), parent_hash: B256::with_last_byte((n - 1) as u8), header_rlp: &[0xc0], writes: &[] }).collect();
        a.backfill(&deeper, Some(B256::with_last_byte(89)), &a.archived_keys().unwrap()).unwrap();
        assert_eq!(val(&a, slot(2), 85), Some(7));
        assert_eq!(val(&a, slot(1), 89), None);
    }

    #[test]
    fn a_late_gap_fill_merges_segments_and_restores_continuity() {
        let d = tempfile::tempdir().unwrap();
        let a = Archive::open(d.path().join("a.redb")).unwrap();
        blk(&a, 10, &[]);
        a.write_base(&slot(1), 10, B256::with_last_byte(10), U256::from(5), &[]).unwrap();
        blk(&a, 11, &[]);
        // restart: 12..14 missing. Block 15's header names 14 as its parent.
        let h15 = alloy_rlp::encode(alloy_consensus::Header { number: 15, parent_hash: B256::with_last_byte(14), ..Default::default() });
        a.write_block(&ArchiveBlock { number: 15, hash: B256::with_last_byte(15), parent_hash: B256::with_last_byte(14), header_rlp: &h15, writes: &[] })
            .unwrap();
        a.write_base(&slot(1), 15, B256::with_last_byte(15), U256::from(8), &[]).unwrap();
        assert_eq!(val(&a, slot(1), 13), None);
        let w13 = [(slot(1), U256::from(8))];
        let gap = [
            ArchiveBlock { number: 12, hash: B256::with_last_byte(12), parent_hash: B256::with_last_byte(11), header_rlp: &[0xc0], writes: &[] },
            ArchiveBlock { number: 13, hash: B256::with_last_byte(13), parent_hash: B256::with_last_byte(12), header_rlp: &[0xc0], writes: &w13 },
            ArchiveBlock { number: 14, hash: B256::with_last_byte(14), parent_hash: B256::with_last_byte(13), header_rlp: &[0xc0], writes: &[] },
        ];
        // Must link on both sides.
        let unlinked = [ArchiveBlock { number: 12, hash: B256::with_last_byte(12), parent_hash: B256::with_last_byte(99), header_rlp: &[0xc0], writes: &[] }];
        assert!(a.fill_gap(&unlinked, &a.archived_keys().unwrap()).is_err());
        a.fill_gap(&gap, &a.archived_keys().unwrap()).unwrap();
        assert_eq!(a.segments().unwrap(), vec![(10, 15)]);
        assert_eq!(val(&a, slot(1), 12), Some(5));
        assert_eq!(val(&a, slot(1), 13), Some(8));
        assert_eq!(val(&a, slot(1), 15), Some(8));
    }

    #[test]
    fn gaps_split_segments_and_persist_across_reopen() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("a.redb");
        {
            let a = Archive::open(&path).unwrap();
            blk(&a, 10, &[]);
            a.write_base(&slot(1), 10, B256::with_last_byte(10), U256::from(5), &[]).unwrap();
            blk(&a, 11, &[]);
        }
        let a = Archive::open(&path).unwrap(); // restart
        assert_eq!(a.last_block().unwrap(), Some(11));
        blk(&a, 15, &[]); // restarted without filling 12..14
        assert_eq!(a.segments().unwrap(), vec![(10, 11), (15, 15)]);
        assert_eq!(val(&a, slot(1), 11), Some(5));
        assert_eq!(val(&a, slot(1), 13), None); // inside the gap
        assert_eq!(val(&a, slot(1), 15), None); // writes in the gap are unknown
        a.write_base(&slot(1), 15, B256::with_last_byte(15), U256::from(6), &[]).unwrap(); // refetched after restart
        assert_eq!(val(&a, slot(1), 15), Some(6));
        assert!(a.archived_keys().unwrap().contains(&slot(1)));
    }

    // ---- Fifth review: archive coverage belongs to one chain and one key set -------------------

    fn header(n: u64, parent: B256) -> Vec<u8> {
        alloy_rlp::encode(alloy_consensus::Header { number: n, parent_hash: parent, ..Default::default() })
    }

    #[test]
    fn a_restart_after_an_offline_reorg_never_inherits_the_old_fork() {
        // Archive ends at A100 with K = 10. While skim is offline, A100 is replaced by B100
        // (K = 20). The first block after the restart is B101, which doesn't write K.
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("a.redb");
        let a100 = B256::with_last_byte(100);
        {
            let a = Archive::open(&path).unwrap();
            blk(&a, 99, &[]);
            a.write_base(&slot(1), 99, B256::with_last_byte(99), U256::from(10), &[]).unwrap();
            blk(&a, 100, &[]);
        }
        let a = Archive::open(&path).unwrap();
        let (b100, b101) = (B256::repeat_byte(0xb0), B256::repeat_byte(0xb1));
        let h = header(101, b100);
        a.write_block(&ArchiveBlock { number: 101, hash: b101, parent_hash: b100, header_rlp: &h, writes: &[] }).unwrap();
        assert_ne!(a.get_at(&slot(1), 101, b101).unwrap(), ArchiveRead::Value(U256::from(10)), "must not inherit A100's value");
        assert_eq!(a.segments().unwrap(), vec![(99, 100), (101, 101)]);
        // Reconciliation: A100 is orphaned (the common ancestor is 99). Dropping it and filling
        // the gap with B100 gives B's value.
        a.truncate_range(100, 100).unwrap();
        let w = [(slot(1), U256::from(20))];
        let hb = header(100, B256::with_last_byte(99));
        a.fill_gap(&[ArchiveBlock { number: 100, hash: b100, parent_hash: B256::with_last_byte(99), header_rlp: &hb, writes: &w }], &a.archived_keys().unwrap())
            .unwrap();
        assert_eq!(a.segments().unwrap(), vec![(99, 101)]);
        assert_eq!(a.get_at(&slot(1), 101, b101).unwrap(), ArchiveRead::Value(U256::from(20)));
        assert_eq!(a.get_at(&slot(1), 100, a100).unwrap(), ArchiveRead::OtherFork);
        assert_eq!(a.get_at(&slot(1), 99, B256::with_last_byte(99)).unwrap(), ArchiveRead::Value(U256::from(10)));
    }

    #[test]
    fn a_key_archived_during_a_backfill_scan_is_not_extended_by_it() {
        // K was 10 before block 120 and 20 from it. The scan of 100..=121 started before K was
        // archived, so block 120's write to K was filtered out. K is archived (base at 121 = 20)
        // before the scan commits: the scan must not conclude "unchanged" and extend 20 back.
        let d = tempfile::tempdir().unwrap();
        let a = Archive::open(d.path().join("a.redb")).unwrap();
        for n in 121..=125 {
            blk(&a, n, &[]);
        }
        let scope = a.archived_keys().unwrap(); // frozen when the scan started: K not in it
        assert!(!scope.contains(&slot(7)));
        a.write_base(&slot(7), 121, B256::with_last_byte(121), U256::from(20), &[]).unwrap(); // archived mid-scan
        let blocks: Vec<ArchiveBlock<'_>> = (100..=121u64)
            .map(|n| ArchiveBlock { number: n, hash: B256::with_last_byte(n as u8), parent_hash: B256::with_last_byte((n - 1) as u8), header_rlp: &[0xc0], writes: &[] })
            .collect();
        a.backfill(&blocks, Some(B256::with_last_byte(120)), &scope).unwrap();
        assert_eq!(val(&a, slot(7), 110), None, "history before the key was established must stay uncovered");
        assert_eq!(val(&a, slot(7), 121), Some(20));
        // The next pass (with K in scope) scans K's write and covers it from there.
        let w120 = [(slot(7), U256::from(20))];
        let again: Vec<ArchiveBlock<'_>> = (100..=121u64)
            .map(|n| ArchiveBlock { number: n, hash: B256::with_last_byte(n as u8), parent_hash: B256::with_last_byte((n - 1) as u8), header_rlp: &[0xc0], writes: if n == 120 { &w120 } else { &[] } })
            .collect();
        a.backfill(&again, Some(B256::with_last_byte(120)), &a.archived_keys().unwrap()).unwrap();
        assert_eq!(val(&a, slot(7), 110), None);
        assert_eq!(val(&a, slot(7), 120), Some(20));
    }

    #[test]
    fn a_fork_switch_during_a_backfill_never_leaves_old_fork_values_under_new_hashes() {
        // Backfill collected fork A (K written at 104 = 40). Between its checks and its commit,
        // the chain rolls back to 102 and fork B (no K write) is applied. Old code validated
        // first and committed later, leaving K@104 = 40 under B104's hash.
        let d = tempfile::tempdir().unwrap();
        let a = std::sync::Arc::new(Archive::open(d.path().join("a.redb")).unwrap());
        for n in 100..=110 {
            blk(&a, n, &[]);
        }
        a.write_base(&slot(9), 108, B256::with_last_byte(108), U256::from(50), &[]).unwrap();
        let w104 = [(slot(9), U256::from(40))];
        let blocks: Vec<ArchiveBlock<'_>> = (100..=108u64)
            .map(|n| ArchiveBlock { number: n, hash: B256::with_last_byte(n as u8), parent_hash: B256::with_last_byte((n - 1) as u8), header_rlp: &[0xc0], writes: if n == 104 { &w104 } else { &[] } })
            .collect();
        let b = |n: u64| B256::repeat_byte(0xb0 + (n - 100) as u8);
        let switcher: std::sync::Arc<parking_lot::Mutex<Option<std::thread::JoinHandle<()>>>> = Default::default();
        {
            let (a2, sw) = (a.clone(), switcher.clone());
            *a.hook.lock() = Some(Box::new(move || {
                let a3 = a2.clone();
                *sw.lock() = Some(std::thread::spawn(move || {
                    a3.truncate_above(102).unwrap();
                    for n in 103..=110u64 {
                        let parent = if n == 103 { B256::with_last_byte(102) } else { b(n - 1) };
                        a3.write_block(&ArchiveBlock { number: n, hash: b(n), parent_hash: parent, header_rlp: &[0xc0], writes: &[] }).unwrap();
                    }
                }));
                std::thread::sleep(std::time::Duration::from_millis(100)); // let the switch try to run
            }));
        }
        let _ = a.backfill(&blocks, Some(B256::with_last_byte(99)), &a.archived_keys().unwrap());
        *a.hook.lock() = None;
        let h = switcher.lock().take().unwrap();
        h.join().unwrap();
        assert_ne!(a.get_at(&slot(9), 104, b(104)).unwrap(), ArchiveRead::Value(U256::from(40)), "fork A's value under B104's hash");
        assert_eq!(a.hash(104).unwrap(), Some(b(104)));
    }

    #[test]
    fn ended_coverage_answers_up_to_its_end_and_restarts_when_archived_again() {
        let d = tempfile::tempdir().unwrap();
        let a = Archive::open(d.path().join("a.redb")).unwrap();
        blk(&a, 100, &[]);
        a.write_base(&slot(1), 100, B256::with_last_byte(100), U256::from(10), &[]).unwrap();
        blk(&a, 101, &[(slot(1), 11)]);
        a.end_coverage(&slot(1), Some(101)).unwrap();
        assert!(!a.archived_keys().unwrap().contains(&slot(1)));
        assert!(a.coverages().unwrap().is_empty(), "an ended key is not reported as archived");
        // Block 102 changed the key, but it is no longer archived: nothing records that.
        blk(&a, 102, &[]);
        assert_eq!(val(&a, slot(1), 100), Some(10));
        assert_eq!(val(&a, slot(1), 101), Some(11));
        assert_eq!(val(&a, slot(1), 102), None, "past the end: refused, not the stale 11");
        // Archived again from a base at 103: a new interval starts there. The earlier one stays
        // readable (TODO 30, REVIEW C9: it used to be replaced), and the gap is never bridged.
        blk(&a, 103, &[]);
        a.write_base(&slot(1), 103, B256::with_last_byte(103), U256::from(30), &[]).unwrap();
        assert_eq!(val(&a, slot(1), 100), Some(10));
        assert_eq!(val(&a, slot(1), 101), Some(11));
        assert_eq!(val(&a, slot(1), 102), None, "between the intervals: refused");
        assert_eq!(val(&a, slot(1), 103), Some(30));
        assert!(matches!(a.get(&slot(1), 102).unwrap(), ArchiveRead::NotCovered { from: Some(103) }));
        // Or archived again with coverage from its next write.
        a.end_coverage(&slot(1), Some(103)).unwrap();
        a.add_keys(&[slot(1)]).unwrap();
        blk(&a, 104, &[]);
        assert_eq!(val(&a, slot(1), 104), None);
        blk(&a, 105, &[(slot(1), 50)]);
        assert_eq!(val(&a, slot(1), 105), Some(50));
        assert_eq!(a.intervals(&slot(1)).unwrap(), vec![(100, Some(101)), (103, Some(103)), (105, None)]);
        assert_eq!((val(&a, slot(1), 101), val(&a, slot(1), 102), val(&a, slot(1), 103), val(&a, slot(1), 104)), (Some(11), None, Some(30), None));
    }

    #[test]
    fn a_reorg_below_an_ended_coverage_lowers_its_end() {
        let d = tempfile::tempdir().unwrap();
        let a = Archive::open(d.path().join("a.redb")).unwrap();
        blk(&a, 100, &[]);
        a.write_base(&slot(1), 100, B256::with_last_byte(100), U256::from(10), &[]).unwrap();
        blk(&a, 101, &[]);
        blk(&a, 102, &[(slot(1), 12)]);
        a.end_coverage(&slot(1), Some(102)).unwrap();
        // Reorg to 100; the new 101..102 aren't recorded for the key (it isn't archived).
        a.truncate_above(100).unwrap();
        blk(&a, 101, &[]);
        blk(&a, 102, &[]);
        assert_eq!(val(&a, slot(1), 100), Some(10));
        assert_eq!(val(&a, slot(1), 101), None, "the new fork's 101 is past the lowered end");
        assert_eq!(val(&a, slot(1), 102), None);
    }

    #[test]
    fn a_gap_fill_succeeds_with_ended_keys_and_stops_their_coverage_before_it() {
        let d = tempfile::tempdir().unwrap();
        let a = Archive::open(d.path().join("a.redb")).unwrap();
        blk(&a, 10, &[]);
        a.write_base(&slot(1), 10, B256::with_last_byte(10), U256::from(5), &[]).unwrap();
        a.write_base(&slot(2), 10, B256::with_last_byte(10), U256::from(6), &[]).unwrap();
        blk(&a, 11, &[]);
        let h15 = alloy_rlp::encode(alloy_consensus::Header { number: 15, parent_hash: B256::with_last_byte(14), ..Default::default() });
        a.write_block(&ArchiveBlock { number: 15, hash: B256::with_last_byte(15), parent_hash: B256::with_last_byte(14), header_rlp: &h15, writes: &[] })
            .unwrap();
        // slot(2) stops being archived with its end claimed at 15, across the 12..=14 gap.
        a.end_coverage(&slot(2), Some(15)).unwrap();
        let gap = [
            ArchiveBlock { number: 12, hash: B256::with_last_byte(12), parent_hash: B256::with_last_byte(11), header_rlp: &[0xc0], writes: &[] },
            ArchiveBlock { number: 13, hash: B256::with_last_byte(13), parent_hash: B256::with_last_byte(12), header_rlp: &[0xc0], writes: &[] },
            ArchiveBlock { number: 14, hash: B256::with_last_byte(14), parent_hash: B256::with_last_byte(13), header_rlp: &[0xc0], writes: &[] },
        ];
        a.fill_gap(&gap, &a.archived_keys().unwrap()).expect("an ended key doesn't block the fill");
        assert_eq!(val(&a, slot(1), 13), Some(5));
        assert_eq!(val(&a, slot(2), 11), Some(6));
        assert_eq!(val(&a, slot(2), 13), None, "the gap carried none of its writes");
    }

    // ---- TODO 30: coverage as intervals ---------------------------------------------------------

    #[test]
    fn earlier_intervals_survive_restarts_and_shrink_only_with_reorgs() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("a.redb");
        {
            let a = Archive::open(&path).unwrap();
            blk(&a, 100, &[]);
            a.write_base(&slot(1), 100, B256::with_last_byte(100), U256::from(10), &[]).unwrap();
            blk(&a, 101, &[(slot(1), 11)]);
            blk(&a, 102, &[]);
            a.end_coverage(&slot(1), Some(102)).unwrap();
            blk(&a, 103, &[]); // released: a write here would not be recorded
            blk(&a, 104, &[]);
            a.write_base(&slot(1), 104, B256::with_last_byte(104), U256::from(14), &[]).unwrap();
            blk(&a, 105, &[(slot(1), 15)]);
        }
        let a = Archive::open(&path).unwrap(); // restart: intervals are on disk
        assert_eq!(a.intervals(&slot(1)).unwrap(), vec![(100, Some(102)), (104, None)]);
        let reads: Vec<Option<u64>> = (99..=106).map(|n| val(&a, slot(1), n)).collect();
        assert_eq!(reads, vec![None, Some(10), Some(11), Some(11), None, Some(14), Some(15), None]);
        // A reorg into the earlier interval shortens it and withdraws the current one.
        a.truncate_above(101).unwrap();
        assert_eq!(a.intervals(&slot(1)).unwrap(), vec![(100, Some(101))]);
        blk(&a, 102, &[]); // the new 102 is not recorded for the key
        assert_eq!((val(&a, slot(1), 101), val(&a, slot(1), 102)), (Some(11), None));
        // A reorg below an earlier interval's start drops it.
        a.truncate_above(99).unwrap();
        assert!(a.intervals(&slot(1)).unwrap().is_empty());
    }

    #[test]
    fn reconciliation_shortens_only_the_intervals_that_reach_into_the_dropped_blocks() {
        let d = tempfile::tempdir().unwrap();
        let a = Archive::open(d.path().join("a.redb")).unwrap();
        for n in 10..=12 {
            blk(&a, n, &[]);
        }
        for k in [slot(1), slot(2)] {
            a.write_base(&k, 10, B256::with_last_byte(10), U256::from(5), &[]).unwrap();
        }
        a.end_coverage(&slot(1), Some(11)).unwrap();
        a.end_coverage(&slot(2), Some(12)).unwrap();
        // After a restart: a new segment at 20, where both keys are archived again.
        let h20 = header(20, B256::with_last_byte(19));
        a.write_block(&ArchiveBlock { number: 20, hash: B256::with_last_byte(20), parent_hash: B256::with_last_byte(19), header_rlp: &h20, writes: &[] }).unwrap();
        for k in [slot(1), slot(2)] {
            a.write_base(&k, 20, B256::with_last_byte(20), U256::from(7), &[]).unwrap();
        }
        // Block 12 turns out to be orphaned.
        a.truncate_range(12, 12).unwrap();
        assert_eq!(a.intervals(&slot(1)).unwrap(), vec![(10, Some(11)), (20, None)], "doesn't reach 12: untouched");
        assert_eq!(a.intervals(&slot(2)).unwrap(), vec![(10, Some(11)), (20, None)], "reached 12: ends before it");
        assert_eq!((val(&a, slot(2), 11), val(&a, slot(2), 20)), (Some(5), Some(7)));
    }

    #[test]
    fn a_gap_fill_never_bridges_blocks_not_recorded_for_a_key() {
        // Segments [10, 11] and [15, 17]. Slot 1 was archived at 16 (block 15 wasn't recorded for
        // it); slot 2 was covered from 15, the next segment's start. The gap 12..=14 writes both.
        let d = tempfile::tempdir().unwrap();
        let a = Archive::open(d.path().join("a.redb")).unwrap();
        blk(&a, 10, &[]);
        blk(&a, 11, &[]);
        let h15 = header(15, B256::with_last_byte(14));
        a.write_block(&ArchiveBlock { number: 15, hash: B256::with_last_byte(15), parent_hash: B256::with_last_byte(14), header_rlp: &h15, writes: &[] }).unwrap();
        blk(&a, 16, &[]);
        blk(&a, 17, &[]);
        a.write_base(&slot(1), 16, B256::with_last_byte(16), U256::from(6), &[]).unwrap();
        a.write_base(&slot(2), 15, B256::with_last_byte(15), U256::from(6), &[]).unwrap();
        let w13 = [(slot(1), U256::from(3)), (slot(2), U256::from(3))];
        let gap: Vec<ArchiveBlock<'_>> = (12..=14u64)
            .map(|n| ArchiveBlock { number: n, hash: B256::with_last_byte(n as u8), parent_hash: B256::with_last_byte((n - 1) as u8), header_rlp: &[0xc0], writes: if n == 13 { &w13 } else { &[] } })
            .collect();
        a.fill_gap(&gap, &a.archived_keys().unwrap()).unwrap();
        assert_eq!(a.segments().unwrap(), vec![(10, 17)]);
        // Slot 1: block 15 may have changed it unrecorded, so 13..=15 stay refused.
        assert_eq!((val(&a, slot(1), 13), val(&a, slot(1), 15), val(&a, slot(1), 16)), (None, None, Some(6)));
        // Slot 2: every block from the gap's write on is recorded.
        assert_eq!((val(&a, slot(2), 12), val(&a, slot(2), 13), val(&a, slot(2), 14), val(&a, slot(2), 15)), (None, Some(3), Some(3), Some(6)));
    }

    /// Copies an archive into the layout written before coverage intervals: the same tables,
    /// written directly, with no `coverage_intervals` table.
    fn write_pre_interval_layout(src: &Archive, dst: &Path, extra: impl FnOnce(&WriteTransaction)) {
        let db = Database::create(dst).unwrap();
        let r = src.db.begin_read().unwrap();
        assert!(r.open_table(INTERVALS).unwrap().is_empty().unwrap(), "the source holds no intervals");
        let w = db.begin_write().unwrap();
        macro_rules! copy {
            ($($t:expr),*) => {$({
                let from = r.open_table($t).unwrap();
                let mut to = w.open_table($t).unwrap();
                for e in from.iter().unwrap() {
                    let (k, v) = e.unwrap();
                    to.insert(k.value(), v.value()).unwrap();
                }
            })*};
        }
        copy!(VERSIONS, COVERAGE, ARCHIVED, HEADERS, HASHES, BY_HASH, CODE, JOURNAL, SEGMENTS, WRITE_BOUND, PRE_CLEAN, COVERAGE_END);
        extra(&w);
        w.commit().unwrap();
    }

    fn assert_same_archive(a: &Archive, b: &Archive, keys: &[StateKey], blocks: std::ops::RangeInclusive<u64>) {
        for k in keys {
            for n in blocks.clone() {
                assert_eq!(a.get(k, n).unwrap(), b.get(k, n).unwrap(), "{k:?} at {n}");
                let h = B256::with_last_byte(n as u8);
                assert_eq!(a.get_at(k, n, h).unwrap(), b.get_at(k, n, h).unwrap(), "{k:?} at {n} ({h})");
            }
            assert_eq!(a.intervals(k).unwrap(), b.intervals(k).unwrap(), "{k:?}");
        }
        assert_eq!(a.coverages().unwrap(), b.coverages().unwrap());
        assert_eq!(a.segments().unwrap(), b.segments().unwrap());
        assert_eq!(a.archived_keys().unwrap(), b.archived_keys().unwrap());
        for n in blocks {
            assert_eq!(a.hash(n).unwrap(), b.hash(n).unwrap());
            assert_eq!(a.header_rlp(n).unwrap(), b.header_rlp(n).unwrap());
        }
    }

    #[test]
    fn an_archive_in_the_pre_interval_layout_opens_and_reads_identically() {
        let d = tempfile::tempdir().unwrap();
        let reference = Archive::open(d.path().join("ref.redb")).unwrap();
        // Every kind of coverage the old format holds: a base extended by backfill (pre-clean),
        // a write-bound key, an ended key, a key archived with nothing covered yet, two segments.
        let a = &reference;
        blk(a, 10, &[]);
        a.write_base(&slot(1), 10, B256::with_last_byte(10), U256::from(1), &[]).unwrap();
        a.add_keys(&[slot(2), slot(5)]).unwrap();
        blk(a, 11, &[(slot(1), 2)]);
        blk(a, 12, &[(slot(2), 5)]);
        a.write_base(&slot(3), 12, B256::with_last_byte(12), U256::from(7), &[]).unwrap();
        blk(a, 13, &[(slot(3), 8)]);
        a.end_coverage(&slot(3), Some(13)).unwrap();
        blk(a, 14, &[]);
        blk(a, 17, &[]); // a restart without gap fill: new segment
        a.write_base(&slot(1), 17, B256::with_last_byte(17), U256::from(3), &[]).unwrap();
        blk(a, 18, &[(slot(2), 6)]);
        a.put_code(B256::repeat_byte(0xc0), &[0x60, 0x00]).unwrap();
        let scan: Vec<ArchiveBlock<'_>> = (5..=10u64)
            .map(|n| ArchiveBlock { number: n, hash: B256::with_last_byte(n as u8), parent_hash: B256::with_last_byte((n - 1) as u8), header_rlp: &[0xc0], writes: &[] })
            .collect();
        let r = a.backfill(&scan, Some(B256::with_last_byte(9)), &a.archived_keys().unwrap()).unwrap();
        assert_eq!((r.keys_unchanged, r.keys_write_bound), (1, 1));

        let old = d.path().join("old.redb");
        // Old releases also left versions of a re-archived key outside any coverage (its start
        // was deleted on re-archiving): slot 5 has one, and it must stay unanswered.
        write_pre_interval_layout(&reference, &old, |w| {
            let ek = encode_key(&slot(5));
            w.open_table(VERSIONS).unwrap().insert(&vkey(&ek, 11), &U256::from(9).to_be_bytes::<32>()).unwrap();
        });
        let before: Vec<String> = Database::open(&old).unwrap().begin_read().unwrap().list_tables().unwrap().map(|t| t.name().to_string()).collect();
        assert!(before.contains(&"coverage".to_string()) && !before.contains(&"coverage_intervals".to_string()));

        let migrated = Archive::open(&old).unwrap();
        assert!(tables_of(&migrated).contains(&"coverage_intervals".to_string()));
        let keys = [slot(1), slot(2), slot(3), slot(4), slot(5)];
        assert_same_archive(&reference, &migrated, &keys, 0..=20);
        assert_eq!(migrated.code(&B256::repeat_byte(0xc0)).unwrap(), Some(Bytes::from_static(&[0x60, 0x00])));
        // Spot checks against what the old code answered.
        assert_eq!(val(&migrated, slot(1), 5), Some(1));
        assert_eq!(val(&migrated, slot(1), 15), None);
        assert_eq!(val(&migrated, slot(2), 11), None);
        assert_eq!(val(&migrated, slot(2), 12), Some(5));
        assert_eq!(val(&migrated, slot(3), 13), Some(8));
        assert_eq!(val(&migrated, slot(3), 14), None);
        assert_eq!(val(&migrated, slot(5), 11), None);
        drop(migrated);

        // Reopened again (already migrated), and then used: the same operations give the same
        // archive, including a first earlier interval for the ended key.
        let migrated = Archive::open(&old).unwrap();
        for ar in [&reference, &migrated] {
            ar.add_keys(&[slot(3)]).unwrap();
            blk(ar, 19, &[(slot(3), 9)]);
            ar.truncate_above(19).unwrap();
        }
        assert_same_archive(&reference, &migrated, &keys, 0..=20);
        assert_eq!(migrated.intervals(&slot(3)).unwrap(), vec![(12, Some(13)), (19, None)]);
        assert_eq!((val(&migrated, slot(3), 13), val(&migrated, slot(3), 18), val(&migrated, slot(3), 19)), (Some(8), None, Some(9)));
    }

    fn tables_of(a: &Archive) -> Vec<String> {
        a.db.begin_read().unwrap().list_tables().unwrap().map(|t| t.name().to_string()).collect()
    }

    // ---- TODO 31: durability outside the store lock ---------------------------------------------

    #[test]
    fn commits_under_a_hold_are_visible_at_once_and_durable_after_it() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("a.redb");
        {
            let a = Archive::open(&path).unwrap();
            let synced = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
            let c = synced.clone();
            *a.sync_hook.lock() = Some(Box::new(move || {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }));
            {
                let _h = a.hold();
                blk(&a, 1, &[]);
                a.write_base(&slot(1), 1, B256::with_last_byte(1), U256::from(10), &[]).unwrap();
                blk(&a, 2, &[(slot(1), 20)]);
                assert_eq!(val(&a, slot(1), 2), Some(20), "visible before the sync");
                assert_eq!(synced.load(std::sync::atomic::Ordering::SeqCst), 0);
            }
            assert_eq!(synced.load(std::sync::atomic::Ordering::SeqCst), 1, "one sync for the whole hold");
            blk(&a, 3, &[]); // alone: durable by itself, no extra sync
            assert_eq!(synced.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(a.sync_errors(), 0);
        }
        let a = Archive::open(&path).unwrap();
        assert_eq!((val(&a, slot(1), 1), val(&a, slot(1), 3)), (Some(10), Some(20)));
    }
}
