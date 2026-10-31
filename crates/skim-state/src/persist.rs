//! Restart persistence: the latest value of every subscribed key, the block it is valid at,
//! the bytecode those keys point to, and the EL node key and peers.
//!
//! This is a cache for restarts, never a source of truth. A snapshot is used only after the
//! chain from its block to the current head is re-verified (headers linked by hash, each BAL
//! matching its header) and its writes replayed. Anything that doesn't check out is discarded
//! and refetched, exactly as without a snapshot.

use std::path::Path;

use alloy_primitives::{B256, Bytes, U256};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use skim_bal::StateKey;

use crate::archive::{decode_key, encode_key};

const STATE: TableDefinition<&[u8; 53], &[u8; 32]> = TableDefinition::new("state");
const CODE: TableDefinition<&[u8; 32], &[u8]> = TableDefinition::new("code");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    #[error("persist db: {0}")]
    Db(String),
}

macro_rules! db_err {
    ($($t:ty),*) => {$(impl From<$t> for PersistError { fn from(e: $t) -> Self { Self::Db(e.to_string()) } })*};
}
db_err!(redb::DatabaseError, redb::TransactionError, redb::TableError, redb::StorageError, redb::CommitError);

/// A saved state snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    pub number: u64,
    pub hash: B256,
    pub values: Vec<(StateKey, U256)>,
    pub code: Vec<(B256, Bytes)>,
}

pub struct Persist {
    db: Database,
}

impl Persist {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PersistError> {
        if let Some(dir) = path.as_ref().parent() {
            std::fs::create_dir_all(dir).map_err(|e| PersistError::Db(e.to_string()))?;
        }
        // Small, bounded page cache: written once per run, read once at startup.
        let db = Database::builder().set_cache_size(8 << 20).create(path)?;
        let tx = db.begin_write()?;
        tx.open_table(STATE)?;
        tx.open_table(CODE)?;
        tx.open_table(META)?;
        tx.commit()?;
        Ok(Self { db })
    }

    /// Replaces the snapshot with `values` at block (`number`, `hash`), in one transaction.
    pub fn save(&self, number: u64, hash: B256, values: &[(StateKey, U256)], code: &[(B256, Bytes)]) -> Result<(), PersistError> {
        let tx = self.db.begin_write()?;
        {
            let mut st = tx.open_table(STATE)?;
            st.retain(|_, _| false)?;
            for (k, v) in values {
                st.insert(&encode_key(k), &v.to_be_bytes::<32>())?;
            }
            let mut c = tx.open_table(CODE)?;
            c.retain(|_, _| false)?;
            for (h, b) in code {
                c.insert(&h.0, b.as_ref())?;
            }
            let mut m = tx.open_table(META)?;
            m.insert("number", number.to_be_bytes().as_slice())?;
            m.insert("hash", hash.as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn load(&self) -> Result<Option<Saved>, PersistError> {
        let tx = self.db.begin_read()?;
        let m = tx.open_table(META)?;
        let (Some(n), Some(h)) = (m.get("number")?, m.get("hash")?) else { return Ok(None) };
        let number = u64::from_be_bytes(n.value().try_into().map_err(|_| PersistError::Db("bad meta".into()))?);
        let hash = B256::from_slice(h.value());
        let st = tx.open_table(STATE)?;
        let mut values = Vec::with_capacity(st.len()? as usize);
        for e in st.iter()? {
            let (k, v) = e?;
            if let Some(k) = decode_key(k.value()) {
                values.push((k, U256::from_be_bytes(*v.value())));
            }
        }
        let c = tx.open_table(CODE)?;
        let mut code = Vec::new();
        for e in c.iter()? {
            let (h, b) = e?;
            code.push((B256::from(*h.value()), Bytes::copy_from_slice(b.value())));
        }
        Ok(Some(Saved { number, hash, values, code }))
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>, PersistError> {
        let tx = self.db.begin_read()?;
        Ok(tx.open_table(META)?.get(key)?.map(|v| v.value().to_vec()))
    }

    pub fn set_meta(&self, key: &str, value: &[u8]) -> Result<(), PersistError> {
        let tx = self.db.begin_write()?;
        tx.open_table(META)?.insert(key, value)?;
        tx.commit()?;
        Ok(())
    }
}

/// Replays verified BAL writes over a snapshot: `blocks` are the writes of each block after the
/// snapshot's block, in order. Only keys in the snapshot are kept; code for changed code
/// hashes comes from the blocks.
pub fn replay<'a>(saved: &Saved, blocks: impl IntoIterator<Item = &'a crate::BlockWrites>) -> crate::StateAndCode {
    let mut values: std::collections::HashMap<StateKey, U256> = saved.values.iter().copied().collect();
    let mut code: std::collections::HashMap<B256, Bytes> = saved.code.iter().cloned().collect();
    for w in blocks {
        for (k, v) in &w.writes {
            if let Some(slot) = values.get_mut(k) {
                *slot = *v;
            }
        }
        for (h, c) in &w.code {
            code.insert(*h, c.clone());
        }
    }
    let wanted: std::collections::HashSet<B256> = values
        .iter()
        .filter(|(k, _)| matches!(k, StateKey::CodeHash(_)))
        .map(|(_, v)| B256::from(*v))
        .collect();
    code.retain(|h, _| wanted.contains(h));
    let mut v: Vec<_> = values.into_iter().collect();
    v.sort();
    (v, code.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, keccak256};

    fn slot(i: u64) -> StateKey {
        StateKey::Storage(Address::repeat_byte(1), U256::from(i))
    }

    #[test]
    fn save_load_roundtrip_and_replace() {
        let d = tempfile::tempdir().unwrap();
        let p = Persist::open(d.path().join("s.redb")).unwrap();
        assert_eq!(p.load().unwrap(), None);
        let c = Bytes::from_static(b"code");
        p.save(10, B256::repeat_byte(9), &[(slot(1), U256::from(5))], &[(keccak256(&c), c.clone())]).unwrap();
        p.save(11, B256::repeat_byte(8), &[(slot(2), U256::from(6))], &[]).unwrap();
        let s = p.load().unwrap().unwrap();
        assert_eq!((s.number, s.hash), (11, B256::repeat_byte(8)));
        assert_eq!(s.values, vec![(slot(2), U256::from(6))]); // replaced, not merged
        assert!(s.code.is_empty());
        p.set_meta("node_key", &[7; 32]).unwrap();
        assert_eq!(p.get_meta("node_key").unwrap(), Some(vec![7; 32]));
    }

    #[test]
    fn replay_applies_only_snapshot_keys_in_order() {
        let saved = Saved { number: 10, hash: B256::ZERO, values: vec![(slot(1), U256::from(1)), (slot(2), U256::from(2))], code: vec![] };
        let w = |ws: Vec<(StateKey, u64)>| crate::BlockWrites {
            writes: ws.into_iter().map(|(k, v)| (k, U256::from(v))).collect(),
            ..Default::default()
        };
        let (v, _) = replay(&saved, &[w(vec![(slot(1), 10), (slot(3), 30)]), w(vec![(slot(1), 11)])]);
        assert_eq!(v, vec![(slot(1), U256::from(11)), (slot(2), U256::from(2))]);
    }
}
