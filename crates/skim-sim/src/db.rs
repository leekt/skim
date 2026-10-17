//! revm `DatabaseRef` over an MVCC [`Snapshot`]. Unknown state never reads as zero to the
//! caller: it is recorded as a miss, and a run with misses is never reported (§7.2).

use std::sync::Mutex;

use alloy_primitives::{Address, B256, U256};
use revm::{DatabaseRef, bytecode::Bytecode, primitives::KECCAK_EMPTY, state::AccountInfo};
use skim_bal::StateKey;
use skim_state::{Snapshot, StateValue};

/// What one execution read, and what it couldn't.
#[derive(Default, Debug, Clone)]
pub struct Access {
    pub miss_accounts: Vec<Address>,
    pub miss_storage: Vec<(Address, U256)>,
    pub miss_code: Vec<B256>,
    /// `BLOCKHASH(n)` for blocks whose hash skim doesn't know. Never answered as zero.
    pub miss_block_hashes: Vec<u64>,
    pub accounts: Vec<Address>,
    pub storage: Vec<(Address, U256)>,
    pub block_hashes: Vec<u64>,
}

impl Access {
    /// Misses that need state (SNAP): accounts, storage, code. Block-hash misses are resolved
    /// with headers from any eth peer.
    pub fn has_state_misses(&self) -> bool {
        !(self.miss_accounts.is_empty() && self.miss_storage.is_empty() && self.miss_code.is_empty())
    }

    pub fn has_misses(&self) -> bool {
        !(self.miss_accounts.is_empty()
            && self.miss_storage.is_empty()
            && self.miss_code.is_empty()
            && self.miss_block_hashes.is_empty())
    }

    pub fn dedup(&mut self) {
        for v in [&mut self.miss_accounts, &mut self.accounts] {
            v.sort();
            v.dedup();
        }
        for v in [&mut self.miss_storage, &mut self.storage] {
            v.sort();
            v.dedup();
        }
        self.miss_code.sort();
        self.miss_code.dedup();
        self.miss_block_hashes.sort();
        self.miss_block_hashes.dedup();
    }
}

pub struct SnapDb<'a> {
    pub snap: &'a Snapshot,
    /// BLOCKHASH source (recent header hashes by number).
    pub block_hash: &'a (dyn Fn(u64) -> Option<B256> + Sync),
    pub access: Mutex<Access>,
    /// Analyzed bytecode by code hash, shared across calls (jump-table analysis is per contract,
    /// not per call). Code is content-addressed, so entries never go stale.
    pub code_cache: &'a parking_lot::RwLock<std::collections::HashMap<B256, Bytecode>>,
}

/// Analyzed contracts kept in [`SnapDb::code_cache`] before it is reset.
pub const CODE_CACHE_MAX: usize = 4096;

impl SnapDb<'_> {
    fn get(&self, k: StateKey) -> StateValue {
        self.snap.get(&k).unwrap_or(StateValue::Unknown)
    }
}

impl DatabaseRef for SnapDb<'_> {
    type Error = std::convert::Infallible;

    fn basic_ref(&self, a: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let mut acc = self.access.lock().unwrap();
        acc.accounts.push(a);
        match (self.get(StateKey::Balance(a)), self.get(StateKey::Nonce(a)), self.get(StateKey::CodeHash(a))) {
            (StateValue::Known(balance), StateValue::Known(nonce), StateValue::Known(ch)) => {
                let code_hash = B256::from(ch);
                if balance.is_zero() && nonce.is_zero() && code_hash == KECCAK_EMPTY {
                    return Ok(None); // empty account (EIP-161) == absent for execution
                }
                Ok(Some(AccountInfo { balance, nonce: nonce.to(), code_hash, code: None, ..Default::default() }))
            }
            _ => {
                acc.miss_accounts.push(a);
                Ok(None)
            }
        }
    }

    fn code_by_hash_ref(&self, h: B256) -> Result<Bytecode, Self::Error> {
        if h == KECCAK_EMPTY {
            return Ok(Bytecode::default());
        }
        if let Some(b) = self.code_cache.read().get(&h) {
            return Ok(b.clone());
        }
        match self.snap.code(&h) {
            Some(c) => {
                let b = Bytecode::new_raw(c);
                let mut cache = self.code_cache.write();
                if cache.len() >= CODE_CACHE_MAX {
                    cache.clear();
                }
                cache.insert(h, b.clone());
                Ok(b)
            }
            None => {
                self.access.lock().unwrap().miss_code.push(h);
                Ok(Bytecode::default())
            }
        }
    }

    fn storage_ref(&self, a: Address, slot: U256) -> Result<U256, Self::Error> {
        let mut acc = self.access.lock().unwrap();
        acc.storage.push((a, slot));
        match self.get(StateKey::Storage(a, slot)) {
            StateValue::Known(v) => Ok(v),
            StateValue::Unknown => {
                acc.miss_storage.push((a, slot));
                Ok(U256::ZERO)
            }
        }
    }

    fn block_hash_ref(&self, n: u64) -> Result<B256, Self::Error> {
        let mut a = self.access.lock().unwrap();
        a.block_hashes.push(n);
        match (self.block_hash)(n) {
            Some(h) => Ok(h),
            None => {
                // The run is discarded (has_misses); the placeholder never reaches a caller.
                a.miss_block_hashes.push(n);
                Ok(B256::ZERO)
            }
        }
    }
}
