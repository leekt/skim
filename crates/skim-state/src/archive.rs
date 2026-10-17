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
//! - `K` is **covered** at `N`: its value was known at or before `N` within that segment
//!
//! Otherwise it is [`ArchiveRead::NotCovered`], never a guess. Reorgs truncate exactly what the
//! orphaned blocks wrote, using a per-block journal.

use std::{collections::HashSet, path::Path};

use alloy_primitives::{Address, B256, Bytes, U256};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use crate::StateKey;

/// Encoded state key: tag (1) + address (20) + slot (32).
const K: usize = 53;
/// Version key: encoded state key + big-endian block number.
const VK: usize = K + 8;

const VERSIONS: TableDefinition<&[u8; VK], &[u8; 32]> = TableDefinition::new("versions");
const COVERAGE: TableDefinition<&[u8; K], u64> = TableDefinition::new("coverage");
const ARCHIVED: TableDefinition<&[u8; K], ()> = TableDefinition::new("archived");
const HEADERS: TableDefinition<u64, &[u8]> = TableDefinition::new("headers");
const HASHES: TableDefinition<u64, &[u8; 32]> = TableDefinition::new("hashes");
const BY_HASH: TableDefinition<&[u8; 32], u64> = TableDefinition::new("by_hash");
const CODE: TableDefinition<&[u8; 32], &[u8]> = TableDefinition::new("code");
const JOURNAL: TableDefinition<u64, &[u8]> = TableDefinition::new("journal");
/// Keys whose coverage starts at a BAL **write** (not a base): the value before that write is in
/// no BAL, so their history can never be extended backwards.
const WRITE_BOUND: TableDefinition<&[u8; K], ()> = TableDefinition::new("write_bound");
/// Keys whose coverage-start block is known not to write them (a backfill scanned it), so the
/// value there also held just before it. A plain base is a post-state: the base block itself
/// may have changed the key, so extending backwards needs that block's BAL.
const PRE_CLEAN: TableDefinition<&[u8; K], ()> = TableDefinition::new("pre_clean");
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
db_err!(redb::DatabaseError, redb::TransactionError, redb::TableError, redb::StorageError, redb::CommitError);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveRead {
    Value(U256),
    /// The archive's block at that height is not the one asked for (a reorg replaced it).
    OtherFork,
    /// Not answerable at this block. `from` is where this key's coverage starts, if anywhere.
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

pub struct Archive {
    db: Database,
    /// Runs inside `backfill`'s write transaction, after its checks (tests: interleavings).
    #[cfg(test)]
    pub(crate) hook: parking_lot::Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl Archive {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ArchiveError> {
        let db = Database::create(path)?;
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
        tx.commit()?;
        Ok(Self {
            db,
            #[cfg(test)]
            hook: Default::default(),
        })
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

    /// Marks keys as archived (idempotent). Their history starts at their first base or write.
    pub fn add_keys(&self, keys: &[StateKey]) -> Result<(), ArchiveError> {
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(ARCHIVED)?;
            for k in keys {
                t.insert(&encode_key(k), ())?;
            }
        }
        tx.commit()?;
        Ok(())
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
        let tx = self.db.begin_write()?;
        {
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
            Self::write_entries(&tx, b)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Records that `ek` has a version at `block`, so a reorg below `block` removes it. Every
    /// version the archive stores must be journaled: BAL writes, bases, replayed writes and
    /// backfilled values alike.
    fn journal_add(tx: &redb::WriteTransaction, block: u64, ek: &[u8; K]) -> Result<(), ArchiveError> {
        let mut j = tx.open_table(JOURNAL)?;
        let mut cur = j.get(block)?.map(|g| g.value().to_vec()).unwrap_or_default();
        if !cur.chunks_exact(K).any(|c| c == ek) {
            cur.extend_from_slice(ek);
            j.insert(block, cur.as_slice())?;
        }
        Ok(())
    }

    fn write_entries(tx: &redb::WriteTransaction, b: &ArchiveBlock<'_>) -> Result<(), ArchiveError> {
        let mut versions = tx.open_table(VERSIONS)?;
        let mut cov = tx.open_table(COVERAGE)?;
        let mut journal = Vec::with_capacity(b.writes.len() * K);
        for (k, v) in b.writes {
            let ek = encode_key(k);
            versions.insert(&vkey(&ek, b.number), &v.to_be_bytes::<32>())?;
            // Coverage starts at the earliest block whose value is known (a gap fill can move it
            // earlier: a write fixes the value from that block on).
            let cur = cov.get(&ek)?.map(|g| g.value());
            if cur.is_none_or(|c| b.number < c) {
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

    /// Every archived key with its coverage start, and whether that start is a base value
    /// (extendable backwards by a BAL scan) rather than a write (never extendable).
    pub fn coverages(&self) -> Result<Vec<(StateKey, u64, bool)>, ArchiveError> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(COVERAGE)?;
        let wb = tx.open_table(WRITE_BOUND)?;
        let mut out = Vec::new();
        for e in t.iter()? {
            let (k, c) = e?;
            let extendable = wb.get(k.value())?.is_none();
            if let Some(key) = decode_key(k.value()) {
                out.push((key, c.value(), extendable));
            }
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
    /// For each archived key whose coverage starts at `c` with `from < c <= scan_end + 1`:
    /// - **no write in `[from, c)`**: a BAL lists every write, so the value was constant; coverage
    ///   moves to `from` with the value at `c`
    /// - **writes**: they are recorded, and coverage moves to the first write (the value before it
    ///   isn't in any BAL)
    pub fn backfill(&self, blocks: &[ArchiveBlock<'_>], s_parent: Option<B256>, scope: &HashSet<StateKey>) -> Result<BackfillReport, ArchiveError> {
        let (Some(first), Some(last)) = (blocks.first(), blocks.last()) else { return Ok(BackfillReport::default()) };
        if blocks.windows(2).any(|w| w[1].number != w[0].number + 1 || w[1].parent_hash != w[0].hash) {
            return Err(ArchiveError::GapMismatch("backfill blocks are not a linked chain".into()));
        }
        let (from, scan_end) = (first.number, last.number);
        // Every check runs inside the write transaction that commits the result. Write
        // transactions are serialized, and a rollback is one too: it lands either before (the
        // checks see it) or after (it truncates what this wrote, through the journal).
        let tx = self.db.begin_write()?;
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
            let mut out = Vec::new();
            for e in cov.iter()? {
                let (k, c) = e?;
                let ek = *k.value();
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
        {
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
                    Self::journal_add(&tx, from, &ek)?;
                    cov.insert(&ek, from)?;
                    clean.insert(&ek, ())?; // `from` was scanned and doesn't write k
                    report.keys_unchanged += 1;
                } else {
                    for (n, v) in &writes {
                        versions.insert(&vkey(&ek, *n), &v.to_be_bytes::<32>())?;
                        Self::journal_add(&tx, *n, &ek)?;
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
        }
        tx.commit()?;
        Ok(report)
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
        let tx = self.db.begin_write()?;
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
        // The blocks carry the writes of the keys archived when the scan started. A key archived
        // since and covered from before the end of the gap would get no versions for it: retry
        // with a fresh scope instead of merging.
        {
            let cov = tx.open_table(COVERAGE)?;
            for e in cov.iter()? {
                let (k, c) = e?;
                if c.value() <= last.number && decode_key(k.value()).is_some_and(|key| !scope.contains(&key)) {
                    return Err(ArchiveError::GapMismatch("archived key set changed during the scan; retry".into()));
                }
            }
        }
        for b in blocks {
            Self::write_entries(&tx, b)?;
        }
        {
            let mut seg = tx.open_table(SEGMENTS)?;
            seg.remove(ns)?;
            seg.insert(ps, ne)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Reconciliation after a fork the archive missed (e.g. a reorg while skim was offline):
    /// forgets blocks `lo..=hi`, the tail of the segment that holds them, which then ends at
    /// `lo - 1` (or disappears). A key whose coverage started in those blocks keeps coverage only
    /// from its next archived version above them (write-bound), if any.
    pub fn truncate_range(&self, lo: u64, hi: u64) -> Result<(), ArchiveError> {
        let tx = self.db.begin_write()?;
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
        {
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
                    .map(|(k, _)| u64::from_be_bytes(k.value()[K..].try_into().unwrap()));
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
        }
        tx.commit()?;
        Ok(())
    }

    /// A key's value became known at block `block` with hash `hash` (e.g. a verified SNAP base),
    /// plus its writes in later blocks `(number, hash, value)`. Every block must be the one the
    /// archive holds at that height (checked in the committing transaction): a value from another
    /// fork is never stored under this fork's header. Coverage starts at the earliest such block.
    pub fn write_base(&self, key: &StateKey, block: u64, hash: B256, value: U256, later: &[(u64, B256, U256)]) -> Result<(), ArchiveError> {
        let ek = encode_key(key);
        let tx = self.db.begin_write()?;
        {
            let hashes = tx.open_table(HASHES)?;
            for (n, h) in std::iter::once((block, hash)).chain(later.iter().map(|(n, h, _)| (*n, *h))) {
                if hashes.get(n)?.map(|g| B256::from(*g.value())) != Some(h) {
                    return Err(ArchiveError::GapMismatch(format!("block {n} ({h}) is not the archived block at that height")));
                }
            }
        }
        let later: Vec<(u64, U256)> = later.iter().map(|(n, _, v)| (*n, *v)).collect();
        let later = later.as_slice();
        {
            let mut versions = tx.open_table(VERSIONS)?;
            // A BAL write at `block` is authoritative; don't overwrite it with the base.
            if versions.get(&vkey(&ek, block))?.is_none() {
                versions.insert(&vkey(&ek, block), &value.to_be_bytes::<32>())?;
            }
            for (n, v) in later {
                versions.insert(&vkey(&ek, *n), &v.to_be_bytes::<32>())?;
            }
            drop(versions);
            Self::journal_add(&tx, block, &ek)?;
            for (n, _) in later {
                Self::journal_add(&tx, *n, &ek)?;
            }
            let mut cov = tx.open_table(COVERAGE)?;
            let cur = cov.get(&ek)?.map(|g| g.value());
            if cur.is_none_or(|c| block < c) {
                cov.insert(&ek, block)?;
                tx.open_table(WRITE_BOUND)?.remove(&ek)?;
                tx.open_table(PRE_CLEAN)?.remove(&ek)?;
            }
            tx.open_table(ARCHIVED)?.insert(&ek, ())?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn put_code(&self, hash: B256, code: &[u8]) -> Result<(), ArchiveError> {
        let tx = self.db.begin_write()?;
        tx.open_table(CODE)?.insert(&hash.0, code)?;
        tx.commit()?;
        Ok(())
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
        let tx = self.db.begin_write()?;
        {
            let mut journal = tx.open_table(JOURNAL)?;
            let mut versions = tx.open_table(VERSIONS)?;
            let mut cov = tx.open_table(COVERAGE)?;
            let mut headers = tx.open_table(HEADERS)?;
            let mut hashes = tx.open_table(HASHES)?;
            let mut by_hash = tx.open_table(BY_HASH)?;
            let blocks: Vec<(u64, Vec<u8>)> = journal
                .range((n + 1)..)?
                .map(|e| e.map(|(b, j)| (b.value(), j.value().to_vec())))
                .collect::<Result<_, _>>()?;
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
            let stale: Vec<[u8; K]> = cov
                .iter()?
                .filter_map(|e| e.ok())
                .filter(|(_, c)| c.value() > n)
                .map(|(k, _)| *k.value())
                .collect();
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
        }
        tx.commit()?;
        Ok(())
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
        let in_segment = {
            let seg = tx.open_table(SEGMENTS)?;
            let r = seg.range(..=number)?.next_back().transpose()?;
            r.map(|(s, e)| (s.value(), e.value()))
        };
        let Some((seg_start, seg_end)) = in_segment.filter(|(_, e)| number <= *e) else {
            return Ok(ArchiveRead::NotCovered { from });
        };
        let _ = seg_end;
        // The newest version at or below `number`, and it must lie within this segment.
        let v = tx.open_table(VERSIONS)?;
        let (lo, hi) = (vkey(&ek, 0), vkey(&ek, number));
        let found = v.range::<&[u8; VK]>(&lo..=&hi)?.next_back().transpose()?;
        match (from, found) {
            (Some(c), Some((k, val))) if c <= number => {
                let at = u64::from_be_bytes(k.value()[K..].try_into().unwrap());
                if at < seg_start {
                    // The last known version predates a gap: writes in the gap are unknown.
                    return Ok(ArchiveRead::NotCovered { from });
                }
                Ok(ArchiveRead::Value(U256::from_be_bytes(*val.value())))
            }
            _ => Ok(ArchiveRead::NotCovered { from }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
