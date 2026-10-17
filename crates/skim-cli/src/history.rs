//! Canonical headers since Amsterdam (Glamsterdam) on disk, so `eth_getLogs` and receipts work
//! over the post-fork history, not just the ~1 day of headers kept in memory. Earlier blocks are
//! out of scope: a header without `block_access_list_hash` marks the floor.
//!
//! Only a hash-linked chain is stored: headers come from gossip/the chain placer (linked by the
//! chain itself) or from a walk down from the oldest stored header, checked hash by hash. A
//! different header at a stored height drops that height and everything above it (a reorg).

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

    /// Stores linked headers (any order). A header replacing a different one at its height drops
    /// everything stored above that height first.
    pub fn put(&self, headers: &[(B256, &Header)]) -> Result<(), StoreError> {
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(HEADERS)?;
            for (hash, h) in headers {
                if !post_fork(h) {
                    continue;
                }
                let replaced = t.get(h.number)?.is_some_and(|g| g.value().0 != &hash.0);
                if replaced {
                    let above: Vec<u64> = t.range(h.number..)?.map(|e| e.map(|(k, _)| k.value())).collect::<Result<_, _>>()?;
                    for n in above {
                        t.remove(n)?;
                    }
                }
                t.insert(h.number, (&hash.0, alloy_rlp::encode(h).as_slice()))?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get(&self, n: u64) -> Result<Option<(B256, Header)>, StoreError> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(HEADERS)?;
        let Some(g) = t.get(n)? else { return Ok(None) };
        let (hash, rlp) = g.value();
        let h = <Header as alloy_rlp::Decodable>::decode(&mut &rlp[..]).map_err(|e| StoreError(e.to_string()))?;
        Ok(Some((B256::from(*hash), h)))
    }

    /// Headers in `[from, to]` (missing heights are absent from the result).
    pub fn range(&self, from: u64, to: u64) -> Result<Vec<(B256, Header)>, StoreError> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(HEADERS)?;
        let mut out = Vec::new();
        for e in t.range(from..=to)? {
            let (_, v) = e?;
            let (hash, rlp) = v.value();
            let h = <Header as alloy_rlp::Decodable>::decode(&mut &rlp[..]).map_err(|e| StoreError(e.to_string()))?;
            out.push((B256::from(*hash), h));
        }
        Ok(out)
    }

    /// The bottom of the stored chain that is contiguous up to `top`: `(number, parent_hash)` of
    /// the lowest header in the unbroken run ending at `top`.
    pub fn contiguous_low(&self, top: u64) -> Result<Option<(u64, B256)>, StoreError> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(HEADERS)?;
        let mut low: Option<(u64, B256)> = None;
        let mut expect: Option<u64> = None;
        for e in t.range(..=top)?.rev() {
            let (k, v) = e?;
            let n = k.value();
            if expect.is_some_and(|x| x != n) {
                break;
            }
            let h = <Header as alloy_rlp::Decodable>::decode(&mut &v.value().1[..]).map_err(|e| StoreError(e.to_string()))?;
            low = Some((n, h.parent_hash));
            expect = Some(n.saturating_sub(1));
            if n == 0 {
                break;
            }
        }
        Ok(low)
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

    #[test]
    fn a_replaced_height_drops_everything_above_it() {
        let d = tempfile::tempdir().unwrap();
        let s = HeaderStore::open(d.path().join("h.redb")).unwrap();
        let a = chain(10, 20, 0);
        s.put(&a.iter().map(|(x, h)| (*x, h)).collect::<Vec<_>>()).unwrap();
        assert_eq!(s.len().unwrap(), 11);
        assert_eq!(s.contiguous_low(20).unwrap().map(|x| x.0), Some(10));
        // A different block at 15 (reorg): 15..=20 are gone, then the new 15 is stored.
        let b = amsterdam(15, a[4].0, 1);
        s.put(&[(b.hash_slow(), &b)]).unwrap();
        assert_eq!(s.len().unwrap(), 6);
        assert_eq!(s.get(15).unwrap().unwrap().0, b.hash_slow());
        assert!(s.get(16).unwrap().is_none());
    }

    #[test]
    fn pre_fork_headers_are_not_stored() {
        let d = tempfile::tempdir().unwrap();
        let s = HeaderStore::open(d.path().join("h.redb")).unwrap();
        let h = Header { number: 5, ..Default::default() };
        s.put(&[(h.hash_slow(), &h)]).unwrap();
        assert_eq!(s.len().unwrap(), 0);
    }
}
