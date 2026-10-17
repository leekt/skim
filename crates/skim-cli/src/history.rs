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
        let db = Database::create(path)?;
        let tx = db.begin_write()?;
        tx.open_table(HEADERS)?;
        tx.open_table(META)?;
        tx.commit()?;
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
    fn pre_fork_headers_are_not_stored() {
        let d = tempfile::tempdir().unwrap();
        let s = HeaderStore::open(d.path().join("h.redb")).unwrap();
        let h = Header { number: 5, ..Default::default() };
        s.put_tip(h.hash_slow(), &h).unwrap();
        s.put_run(&[(h.hash_slow(), &h)]).unwrap();
        assert_eq!(s.len().unwrap(), 0);
    }
}
