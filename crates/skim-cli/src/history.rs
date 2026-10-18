//! Canonical headers since Amsterdam (Glamsterdam) on disk, so `eth_getLogs` and receipts work
//! over the post-fork history, not just the ~1 day of headers kept in memory. Earlier blocks are
//! out of scope: a header without `block_access_list_hash` marks the floor.
//!
//! Rows are stored by height and may include an abandoned fork's headers (e.g. written before
//! a restart that followed an offline reorg). So nothing is trusted by height: every read walks
//! down by hash from an anchor the caller knows is canonical ([`HeaderStore::linked_range`],
//! [`HeaderStore::linked_low`]), and stops at the first header that isn't the expected parent.

use std::path::Path;

use alloy_consensus::Header;
use alloy_primitives::B256;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
#[cfg(test)]
use redb::ReadableTableMetadata;

/// number -> (hash, header RLP)
const HEADERS: TableDefinition<u64, (&[u8; 32], &[u8])> = TableDefinition::new("headers");
/// "floor": the first post-fork block (once a walk reached a pre-fork header).
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
/// (filter set, block hash) -> the block's logs matching that filter set (possibly none): what a
/// module's scans fetched, kept so the same blocks are never fetched again for it. Stored per
/// filter set, not per block, so the store grows with what modules ask for, not with the chain.
const MATCHED: TableDefinition<&[u8; 64], &[u8]> = TableDefinition::new("matched_logs");
/// The earlier per-block table (every log of every fetched block): dropped on open.
const OLD_LOGS: TableDefinition<&[u8; 32], &[u8]> = TableDefinition::new("logs");

/// One matching log with its position: transaction index and hash, log index in the block.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchedLog {
    pub tx_index: u32,
    pub log_index: u32,
    pub tx_hash: B256,
    pub log: alloy_primitives::Log,
}

pub fn encode_matched(logs: &[MatchedLog]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(logs.len() as u32).to_be_bytes());
    for m in logs {
        b.extend_from_slice(&m.tx_index.to_be_bytes());
        b.extend_from_slice(&m.log_index.to_be_bytes());
        b.extend_from_slice(m.tx_hash.as_slice());
        b.extend_from_slice(m.log.address.as_slice());
        b.push(m.log.topics().len() as u8);
        for t in m.log.topics() {
            b.extend_from_slice(t.as_slice());
        }
        b.extend_from_slice(&(m.log.data.data.len() as u32).to_be_bytes());
        b.extend_from_slice(&m.log.data.data);
    }
    b
}

pub fn decode_matched(mut b: &[u8]) -> Option<Vec<MatchedLog>> {
    fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        if b.len() < n {
            return None;
        }
        let (x, rest) = b.split_at(n);
        *b = rest;
        Some(x)
    }
    let u32_ = |b: &mut &[u8]| take(b, 4).map(|x| u32::from_be_bytes(x.try_into().unwrap()));
    let n = u32_(&mut b)? as usize;
    let mut out = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        let tx_index = u32_(&mut b)?;
        let log_index = u32_(&mut b)?;
        let tx_hash = B256::from_slice(take(&mut b, 32)?);
        let address = alloy_primitives::Address::from_slice(take(&mut b, 20)?);
        let k = take(&mut b, 1)?[0] as usize;
        let mut topics = Vec::with_capacity(k);
        for _ in 0..k {
            topics.push(B256::from_slice(take(&mut b, 32)?));
        }
        let len = u32_(&mut b)? as usize;
        let data = alloy_primitives::Bytes::copy_from_slice(take(&mut b, len)?);
        out.push(MatchedLog { tx_index, log_index, tx_hash, log: alloy_primitives::Log::new_unchecked(address, topics, data) });
    }
    b.is_empty().then_some(out)
}

fn matched_key(filters: &B256, block: &B256) -> [u8; 64] {
    let mut k = [0u8; 64];
    k[..32].copy_from_slice(filters.as_slice());
    k[32..].copy_from_slice(block.as_slice());
    k
}

/// redb page cache for the header store.
const CACHE_BYTES: usize = 16 << 20;

pub struct HeaderStore {
    db: Database,
}

#[derive(Debug, thiserror::Error)]
#[error("header store: {0}")]
pub struct StoreError(String);

macro_rules! from {
    ($($t:ty),*) => {$(impl From<$t> for StoreError { fn from(e: $t) -> Self { Self(e.to_string()) } })*};
}
from!(redb::Error, redb::DatabaseError, redb::TransactionError, redb::TableError, redb::StorageError, redb::CommitError);

/// A header's parent hash, straight from its RLP (the first field), without decoding it all.
fn parent_of(rlp: &[u8]) -> Result<B256, StoreError> {
    let mut b = rlp;
    let _ = alloy_rlp::Header::decode(&mut b).map_err(|e| StoreError(e.to_string()))?;
    let p: B256 = alloy_rlp::Decodable::decode(&mut b).map_err(|e| StoreError(e.to_string()))?;
    Ok(p)
}

/// Post-Amsterdam: the header commits to a block access list.
pub fn post_fork(h: &Header) -> bool {
    h.block_access_list_hash.is_some()
}

impl HeaderStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        // Bounded page cache: reads walk many headers, and redb's default cache (1 GiB) would
        // keep most of the file in memory.
        let mut db = Database::builder().set_cache_size(CACHE_BYTES).create(path)?;
        let tx = db.begin_write()?;
        tx.open_table(HEADERS)?;
        tx.open_table(META)?;
        tx.open_table(MATCHED)?;
        let dropped = tx.delete_table(OLD_LOGS)?;
        tx.commit()?;
        if dropped {
            // The old per-block logs could be gigabytes: give the space back.
            let _ = db.compact();
        }
        Ok(Self { db })
    }

    /// The chain's new tip (gossip, the chain placer). A different header at its height is a reorg:
    /// that height and everything above it go first.
    pub fn put_tip(&self, hash: B256, h: &Header) -> Result<(), StoreError> {
        if !post_fork(h) {
            return Ok(());
        }
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(HEADERS)?;
            if t.get(h.number)?.is_some_and(|g| g.value().0 != &hash.0) {
                let above: Vec<u64> = t.range(h.number..)?.map(|e| e.map(|(k, _)| k.value())).collect::<Result<_, _>>()?;
                for n in above {
                    t.remove(n)?;
                }
            }
            t.insert(h.number, (&hash.0, alloy_rlp::encode(h).as_slice()))?;
        }
        tx.commit()?;
        Ok(())
    }

    /// A hash-linked run fetched downward from a canonical header (any order): each height is
    /// overwritten with the run's header. Nothing else is removed: the run's descendants are the
    /// chain it was fetched from, and rows below it are checked by hash when read.
    pub fn put_run(&self, run: &[(B256, &Header)]) -> Result<(), StoreError> {
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(HEADERS)?;
            for (hash, h) in run.iter().filter(|(_, h)| post_fork(h)) {
                t.insert(h.number, (&hash.0, alloy_rlp::encode(*h).as_slice()))?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub fn get(&self, n: u64) -> Result<Option<(B256, Header)>, StoreError> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(HEADERS)?;
        let Some(g) = t.get(n)? else { return Ok(None) };
        let (hash, rlp) = g.value();
        let h = <Header as alloy_rlp::Decodable>::decode(&mut &rlp[..]).map_err(|e| StoreError(e.to_string()))?;
        Ok(Some((B256::from(*hash), h)))
    }

    /// Walking down from height `top`, whose canonical hash is `hash`: the lowest stored header
    /// still on that chain, as `(number, parent_hash)` (each step must be the previous header's
    /// parent, by hash). `None` if the row at `top` isn't `hash`.
    pub fn linked_low(&self, top: u64, hash: B256) -> Result<Option<(u64, B256)>, StoreError> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(HEADERS)?;
        let mut expect = hash;
        let mut low = None;
        for (i, e) in t.range(..=top)?.rev().enumerate() {
            let (k, v) = e?;
            let (n, (h, rlp)) = (k.value(), v.value());
            if n != top - i as u64 || h != &expect.0 {
                break;
            }
            expect = parent_of(rlp)?;
            low = Some((n, expect));
            if n == 0 {
                break;
            }
        }
        Ok(low)
    }

    /// The headers in `[from, to]` that are ancestors of the canonical header at height `above`
    /// (`above > to`, whose parent hash is `parent`), read in one transaction by walking down by
    /// hash from it. `None` if the
    /// stored chain breaks before reaching `from` (missing, or another fork's header).
    pub fn linked_range(&self, from: u64, to: u64, above: u64, parent: B256) -> Result<Option<Vec<(B256, Header)>>, StoreError> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(HEADERS)?;
        let mut expect = parent;
        let mut out = Vec::new();
        let mut n = above;
        for e in t.range(from..above)?.rev() {
            let (k, v) = e?;
            let (m, (h, rlp)) = (k.value(), v.value());
            if m + 1 != n || h != &expect.0 {
                return Ok(None);
            }
            if m <= to {
                let hdr = <Header as alloy_rlp::Decodable>::decode(&mut &rlp[..]).map_err(|e| StoreError(e.to_string()))?;
                out.push((B256::from(*h), hdr));
            }
            expect = parent_of(rlp)?;
            n = m;
        }
        if n != from {
            return Ok(None);
        }
        out.reverse();
        Ok(Some(out))
    }

    /// Stores, for the filter set `filters` (a fingerprint), each block's matching logs.
    pub fn put_matched(&self, filters: &B256, blocks: &[(B256, &[MatchedLog])]) -> Result<(), StoreError> {
        if blocks.is_empty() {
            return Ok(());
        }
        // Encode and compress first; the write transaction only stores finished bytes.
        let mut values = Vec::with_capacity(blocks.len());
        for (h, logs) in blocks {
            let raw = encode_matched(logs);
            let v = if raw.len() > 256 { [&[1u8][..], &zstd::bulk::compress(&raw, 3).map_err(|e| StoreError(e.to_string()))?].concat() } else { [&[0u8][..], &raw].concat() };
            values.push((matched_key(filters, h), v));
        }
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(MATCHED)?;
            for (k, v) in &values {
                t.insert(k, v.as_slice())?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The stored matches for whichever of `blocks` were scanned with `filters` before.
    pub fn get_matched(&self, filters: &B256, blocks: &[B256]) -> Result<std::collections::HashMap<B256, Vec<MatchedLog>>, StoreError> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(MATCHED)?;
        let mut out = std::collections::HashMap::new();
        for h in blocks {
            let Some(g) = t.get(&matched_key(filters, h))? else { continue };
            let v = g.value();
            let raw = match v.first() {
                Some(1) => zstd::stream::decode_all(&v[1..]).ok(),
                Some(0) => Some(v[1..].to_vec()),
                _ => None,
            };
            if let Some(m) = raw.as_deref().and_then(decode_matched) {
                out.insert(*h, m);
            }
        }
        Ok(out)
    }

    pub fn floor(&self) -> Result<Option<u64>, StoreError> {
        let tx = self.db.begin_read()?;
        Ok(tx.open_table(META)?.get("floor")?.map(|g| g.value()))
    }

    pub fn set_floor(&self, n: u64) -> Result<(), StoreError> {
        let tx = self.db.begin_write()?;
        tx.open_table(META)?.insert("floor", n)?;
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub fn len(&self) -> Result<u64, StoreError> {
        let tx = self.db.begin_read()?;
        Ok(tx.open_table(HEADERS)?.len()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A post-Amsterdam header: every optional field up to the access-list hash is present.
    fn amsterdam(number: u64, parent_hash: B256, tag: u8) -> Header {
        Header {
            number,
            parent_hash,
            extra_data: vec![tag].into(),
            base_fee_per_gas: Some(7),
            withdrawals_root: Some(B256::ZERO),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(B256::ZERO),
            requests_hash: Some(B256::ZERO),
            block_access_list_hash: Some(B256::ZERO),
            ..Default::default()
        }
    }

    fn chain(from: u64, to: u64, fork: u8) -> Vec<(B256, Header)> {
        let mut out = Vec::new();
        let mut parent = B256::repeat_byte(0xaa);
        for n in from..=to {
            let h = amsterdam(n, parent, fork);
            let hash = h.hash_slow();
            parent = hash;
            out.push((hash, h));
        }
        out
    }

    fn refs(c: &[(B256, Header)]) -> Vec<(B256, &Header)> {
        c.iter().map(|(x, h)| (*x, h)).collect()
    }

    /// A98 -> A99 -> A100 on disk; B99 -> B100 -> B101 forked from A98.
    fn forked() -> (tempfile::TempDir, HeaderStore, Vec<(B256, Header)>, Vec<(B256, Header)>) {
        let d = tempfile::tempdir().unwrap();
        let s = HeaderStore::open(d.path().join("h.redb")).unwrap();
        let a = chain(98, 100, 0);
        for (x, h) in &a {
            s.put_tip(*x, h).unwrap();
        }
        let mut b = Vec::new();
        let mut parent = a[0].0;
        for n in 99..=101 {
            let h = amsterdam(n, parent, 1);
            parent = h.hash_slow();
            b.push((parent, h));
        }
        (d, s, a, b)
    }

    #[test]
    fn history_after_an_offline_reorg_never_reads_the_old_fork() {
        // Ninth review, P1: restart at B101; query 100 (fetches B100), then 99. A99 is still
        // stored at 99 and must not pass as B100's parent.
        let (_d, s, a, b) = forked();
        let (b99, b100, b101) = (&b[0], &b[1], &b[2]);
        // Before any repair, nothing below B101 is on its chain.
        assert_eq!(s.linked_range(100, 100, 101, b101.1.parent_hash).unwrap().map(|v| v.len()), None);
        assert_eq!(s.linked_low(100, b100.0).unwrap(), None);
        // Query 100: B100 fetched (linked to B101).
        s.put_run(&refs(std::slice::from_ref(b100))).unwrap();
        let r = s.linked_range(100, 100, 101, b101.1.parent_hash).unwrap().unwrap();
        assert_eq!(r[0].0, b100.0);
        // Query 99: the walk stops at A99 (not B100's parent), so 99 isn't served from disk.
        assert_eq!(s.linked_range(99, 99, 101, b101.1.parent_hash).unwrap().map(|v| v.len()), None);
        assert_eq!(s.linked_low(100, b100.0).unwrap(), Some((100, b99.0)), "the run ends at 100: 99 must be fetched");
        assert_eq!(s.get(99).unwrap().unwrap().0, a[1].0, "A99 is still stored, just never used");
        // After fetching B99, 98..=100 are B101's ancestors (A98 is the fork point).
        s.put_run(&refs(std::slice::from_ref(b99))).unwrap();
        let r = s.linked_range(98, 100, 101, b101.1.parent_hash).unwrap().unwrap();
        assert_eq!(r.iter().map(|x| x.0).collect::<Vec<_>>(), vec![a[0].0, b99.0, b100.0]);
    }

    #[test]
    fn a_descending_repair_batch_keeps_every_header_it_brings() {
        // Ninth review, P2: disk A98 A99 A100 B101; the repair batch arrives newest first
        // (B100, B99). Every repaired height must stay.
        let (_d, s, a, b) = forked();
        s.put_tip(b[2].0, &b[2].1).unwrap();
        s.put_run(&refs(&[b[1].clone(), b[0].clone()])).unwrap();
        assert_eq!(s.get(100).unwrap().unwrap().0, b[1].0);
        assert_eq!(s.get(99).unwrap().unwrap().0, b[0].0);
        assert_eq!(s.get(101).unwrap().unwrap().0, b[2].0);
        assert_eq!(s.linked_low(101, b[2].0).unwrap(), Some((98, a[0].1.parent_hash)));
    }

    #[test]
    fn a_new_tip_at_a_stored_height_drops_everything_above_it() {
        let d = tempfile::tempdir().unwrap();
        let s = HeaderStore::open(d.path().join("h.redb")).unwrap();
        let a = chain(10, 20, 0);
        for (x, h) in &a {
            s.put_tip(*x, h).unwrap();
        }
        let b = amsterdam(15, a[4].0, 1);
        s.put_tip(b.hash_slow(), &b).unwrap();
        assert_eq!(s.len().unwrap(), 6);
        assert!(s.get(16).unwrap().is_none());
    }

    #[test]
    fn matched_logs_round_trip_per_filter_set() {
        let logs = vec![
            MatchedLog { tx_index: 0, log_index: 3, tx_hash: B256::repeat_byte(1), log: alloy_primitives::Log::new_unchecked(alloy_primitives::Address::repeat_byte(9), vec![B256::repeat_byte(3)], vec![1, 2, 3].into()) },
            MatchedLog { tx_index: 2, log_index: 9, tx_hash: B256::repeat_byte(2), log: alloy_primitives::Log::new_unchecked(alloy_primitives::Address::repeat_byte(8), vec![], vec![7; 600].into()) },
        ];
        assert_eq!(decode_matched(&encode_matched(&logs)), Some(logs.clone()));
        let d = tempfile::tempdir().unwrap();
        let s = HeaderStore::open(d.path().join("h.redb")).unwrap();
        let (f1, f2, b) = (B256::repeat_byte(0xf1), B256::repeat_byte(0xf2), B256::repeat_byte(7));
        s.put_matched(&f1, &[(b, &logs), (B256::repeat_byte(6), &[])]).unwrap();
        let got = s.get_matched(&f1, &[b, B256::repeat_byte(6), B256::repeat_byte(5)]).unwrap();
        assert_eq!(got.get(&b), Some(&logs));
        assert_eq!(got.get(&B256::repeat_byte(6)), Some(&vec![]), "scanned, nothing matched");
        assert!(!got.contains_key(&B256::repeat_byte(5)));
        assert!(s.get_matched(&f2, &[b]).unwrap().is_empty(), "another filter set has its own entries");
    }

    #[test]
    fn pre_fork_headers_are_not_stored() {
        let d = tempfile::tempdir().unwrap();
        let s = HeaderStore::open(d.path().join("h.redb")).unwrap();
        let h = Header { number: 5, ..Default::default() };
        s.put_tip(h.hash_slow(), &h).unwrap();
        s.put_run(&[(h.hash_slow(), &h)]).unwrap();
        assert_eq!(s.len().unwrap(), 0);
    }
}
