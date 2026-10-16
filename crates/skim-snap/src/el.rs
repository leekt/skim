//! [`Transport`] over skim's EL network (reth sessions, snap/1 and snap/2), with **verified**
//! point lookups (PROJECT.md §3.4, §8).
//!
//! Every answer is checked against the requested root before it can become state:
//! - **Account:** a Merkle proof against the state root, for presence or proven absence.
//! - **Storage slot:** the account is fetched (and verified) first, to learn its storage root.
//!   Concurrent lookups share that fetch. Then:
//!   - empty storage trie: every slot is zero
//!   - otherwise the slot needs a proof against that root (present or proven absent)
//!   - a proof-less answer is accepted only if it is the whole trie and rebuilds to that root
//! - **Anything else** is [`AccountAnswer::Unavailable`] / [`SlotAnswer::Unavailable`]: no
//!   proof, a proof that doesn't settle the key, or a response that contradicts its proof.
//!   The scheduler retries elsewhere. Nothing unverified ever becomes a value, zero included.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_rlp::Decodable;
use alloy_trie::TrieAccount;
use parking_lot::{Mutex, RwLock};
use skim_el::{ElNet, ReqError};
use tokio::sync::OnceCell;

use crate::{AccountAnswer, AccountState, EMPTY_ROOT_HASH, PeerId, SlotAnswer, Transport, TransportError, proof};

/// Answers rejected by verification, by reason (for monitoring).
#[derive(Debug, Default)]
pub struct Rejections {
    pub no_proof: AtomicU64,
    pub bad_proof: AtomicU64,
    pub contradiction: AtomicU64,
    pub root_unavailable: AtomicU64,
    /// Response payload bytes received (RLP length) and responses, for traffic accounting.
    pub bytes_in: AtomicU64,
    pub responses: AtomicU64,
}

impl Rejections {
    fn traffic(&self, bytes: usize) {
        self.bytes_in.fetch_add(bytes as u64, Ordering::Relaxed);
        self.responses.fetch_add(1, Ordering::Relaxed);
    }
}

type AccountCell = Arc<OnceCell<Option<AccountState>>>;

#[derive(Clone)]
pub struct ElTransport {
    pub net: ElNet,
    /// Peers the scheduler must not use (experiments, operator policy).
    pub excluded: Arc<RwLock<HashSet<PeerId>>>,
    pub rejections: Arc<Rejections>,
    /// Verified accounts by (state root, address, peer), shared by concurrent slot lookups to
    /// the same peer. Per peer, so one slow peer never blocks lookups routed to others.
    accounts: Arc<Mutex<HashMap<(B256, Address, PeerId), AccountCell>>>,
}

/// A contract's whole storage at a root.
#[derive(Debug)]
pub enum StorageAll {
    /// Every slot (hashed key, value), rebuilt to the account's storage root.
    Complete { slots: Vec<(B256, U256)>, storage_root: B256 },
    /// More than the allowed number of slots.
    TooLarge(usize),
    /// The peer couldn't serve it, or what it served didn't rebuild to the root.
    Unavailable,
}

/// An EIP-1186 proof assembled from SNAP answers and verified against the state root.
#[derive(Debug, Clone)]
pub struct AccountProof {
    /// `None` if the account is proven absent.
    pub account: Option<AccountState>,
    pub account_proof: Vec<Bytes>,
    /// (slot, value, proof) per requested slot.
    pub storage: Vec<(U256, U256, Vec<Bytes>)>,
}

impl ElTransport {
    /// `eth_getProof` from one peer at `root`: the account and each slot, every value checked
    /// against the root (or the account's storage root) before it is returned. `Ok(None)` if the
    /// peer couldn't serve or prove it.
    pub async fn get_proof(&self, peer: PeerId, root: B256, address: Address, slots: &[U256]) -> Result<Option<AccountProof>, TransportError> {
        let ah = keccak256(address);
        let m = self.net.account_range(peer, root, ah, 512).await.0.map_err(map_err)?;
        if m.proof.is_empty() {
            return Ok(None);
        }
        let (leaf, account_proof) = match proof::lookup_path(root, ah, &m.proof) {
            Ok(x) => x,
            Err(_) => return Ok(None),
        };
        let account = match leaf {
            None => None,
            Some(l) => match TrieAccount::decode(&mut l.as_slice()) {
                Ok(a) => Some(AccountState { nonce: a.nonce, balance: a.balance, storage_root: a.storage_root, code_hash: a.code_hash }),
                Err(_) => return Ok(None),
            },
        };
        let mut storage = Vec::with_capacity(slots.len());
        for slot in slots {
            let storage_root = match &account {
                Some(a) if a.storage_root != EMPTY_ROOT_HASH => a.storage_root,
                _ => {
                    storage.push((*slot, U256::ZERO, Vec::new())); // proven absent / empty trie
                    continue;
                }
            };
            let sh = keccak256(slot.to_be_bytes::<32>());
            let m = self.net.storage_range(peer, root, ah, sh, 512).await.0.map_err(map_err)?;
            let page = m.slots.into_iter().next().unwrap_or_default();
            let (value, proof_nodes) = if m.proof.is_empty() {
                // A whole small trie without proof: it must rebuild the storage root; then the
                // proof for this slot is built from it.
                let leaves: Vec<(B256, Bytes)> = page.iter().map(|d| (d.hash, d.data.clone())).collect();
                if leaves.is_empty() || proof::root_of(leaves.clone()) != Some(storage_root) {
                    return Ok(None);
                }
                let built = proof::proof_from_leaves(leaves.clone(), sh);
                let v = leaves.iter().find(|(h, _)| *h == sh).map(|(_, d)| d.clone());
                (v, built)
            } else {
                match proof::lookup_path(storage_root, sh, &m.proof) {
                    Ok((v, path)) => (v.map(Bytes::from), path),
                    Err(_) => return Ok(None),
                }
            };
            let value = match value {
                None => U256::ZERO,
                Some(v) => match U256::decode(&mut v.as_ref()) {
                    Ok(v) => v,
                    Err(_) => return Ok(None),
                },
            };
            storage.push((*slot, value, proof_nodes));
        }
        Ok(Some(AccountProof { account, account_proof, storage }))
    }

    /// Downloads a contract's entire storage at `root` from one peer, page by page, and accepts
    /// it only if all leaves together rebuild the account's (proven) storage root. Pages are not
    /// trusted individually; the end-to-end root check covers them all.
    pub async fn storage_all(&self, peer: PeerId, root: B256, address: Address, max_slots: usize) -> Result<StorageAll, TransportError> {
        let storage_root = match self.account_shared(peer, root, address).await? {
            None => return Ok(StorageAll::Unavailable),
            Some(None) => return Ok(StorageAll::Complete { slots: Vec::new(), storage_root: EMPTY_ROOT_HASH }),
            Some(Some(a)) if a.storage_root == EMPTY_ROOT_HASH => {
                return Ok(StorageAll::Complete { slots: Vec::new(), storage_root: EMPTY_ROOT_HASH });
            }
            Some(Some(a)) => a.storage_root,
        };
        let ah = keccak256(address);
        let mut leaves: Vec<(B256, Bytes)> = Vec::new();
        let mut start = B256::ZERO;
        loop {
            let m = self.net.storage_range_from(peer, root, ah, start, 512 * 1024).await.0.map_err(map_err)?;
            self.rejections.traffic(alloy_rlp::Encodable::length(&m));
            let page = m.slots.into_iter().next().unwrap_or_default();
            if page.is_empty() && m.proof.is_empty() {
                self.rejections.root_unavailable.fetch_add(1, Ordering::Relaxed);
                return Ok(StorageAll::Unavailable);
            }
            let last = page.last().map(|d| d.hash);
            leaves.extend(page.into_iter().map(|d| (d.hash, d.data)));
            if leaves.len() > max_slots {
                return Ok(StorageAll::TooLarge(leaves.len()));
            }
            // No proof: this page reached the end of the trie. With a proof: more may follow.
            match (m.proof.is_empty(), last) {
                (true, _) | (false, None) => break,
                (false, Some(h)) => {
                    let next = U256::from_be_bytes(h.0).checked_add(U256::from(1u8));
                    match next {
                        Some(n) => start = B256::from(n.to_be_bytes::<32>()),
                        None => break,
                    }
                }
            }
        }
        if proof::root_of(leaves.clone()) != Some(storage_root) {
            self.rejections.bad_proof.fetch_add(1, Ordering::Relaxed);
            return Ok(StorageAll::Unavailable);
        }
        let mut slots = Vec::with_capacity(leaves.len());
        for (h, v) in leaves {
            match U256::decode(&mut v.as_ref()) {
                Ok(v) => slots.push((h, v)),
                Err(_) => return Ok(StorageAll::Unavailable),
            }
        }
        Ok(StorageAll::Complete { slots, storage_root })
    }

    pub fn new(net: ElNet) -> Self {
        Self { net, excluded: Default::default(), rejections: Default::default(), accounts: Default::default() }
    }

    pub fn exclude(&self, peer: PeerId) {
        self.excluded.write().insert(peer);
    }

    async fn account_verified(&self, peer: PeerId, root: B256, address: Address) -> Result<AccountAnswer, TransportError> {
        let h = keccak256(address);
        let m = self.net.account_range(peer, root, h, 512).await.0.map_err(map_err)?;
        let r = &self.rejections;
        r.traffic(alloy_rlp::Encodable::length(&m));
        if m.accounts.is_empty() && m.proof.is_empty() {
            r.root_unavailable.fetch_add(1, Ordering::Relaxed);
            return Ok(AccountAnswer::Unavailable);
        }
        if m.proof.is_empty() {
            r.no_proof.fetch_add(1, Ordering::Relaxed);
            return Ok(AccountAnswer::Unavailable);
        }
        let returned = m.accounts.first().filter(|d| d.hash == h).map(|d| d.trie_account());
        match proof::lookup(root, h, &m.proof) {
            Ok(Some(leaf)) => {
                let Ok(acct) = TrieAccount::decode(&mut leaf.as_slice()) else {
                    r.bad_proof.fetch_add(1, Ordering::Relaxed);
                    return Ok(AccountAnswer::Unavailable);
                };
                if returned.is_some_and(|x| x != acct) {
                    r.contradiction.fetch_add(1, Ordering::Relaxed);
                    return Ok(AccountAnswer::Unavailable);
                }
                Ok(AccountAnswer::Found(Some(AccountState {
                    nonce: acct.nonce,
                    balance: acct.balance,
                    storage_root: acct.storage_root,
                    code_hash: acct.code_hash,
                })))
            }
            Ok(None) if returned.is_none() => Ok(AccountAnswer::Found(None)),
            Ok(None) => {
                r.contradiction.fetch_add(1, Ordering::Relaxed);
                Ok(AccountAnswer::Unavailable)
            }
            Err(_) => {
                r.bad_proof.fetch_add(1, Ordering::Relaxed);
                Ok(AccountAnswer::Unavailable)
            }
        }
    }

    /// The verified account at `root`, fetched once per (root, address, peer) across concurrent
    /// callers. Failures aren't cached.
    async fn account_shared(&self, peer: PeerId, root: B256, address: Address) -> Result<Option<Option<AccountState>>, TransportError> {
        let cell = {
            let mut map = self.accounts.lock();
            if map.len() > 8192 {
                map.retain(|_, c| c.initialized());
                if map.len() > 8192 {
                    map.clear();
                }
            }
            map.entry((root, address, peer)).or_default().clone()
        };
        let res = cell
            .get_or_try_init(|| async {
                match self.account_verified(peer, root, address).await? {
                    AccountAnswer::Found(a) => Ok(a),
                    AccountAnswer::Unavailable => Err(TransportError::Other("account unavailable".into())),
                }
            })
            .await;
        match res {
            Ok(a) => Ok(Some(*a)),
            Err(TransportError::Other(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

fn map_err(e: ReqError) -> TransportError {
    match e {
        ReqError::Timeout => TransportError::Timeout,
        // reth drops the response channel when the session closes.
        ReqError::Peer(s) if s.contains("dropped") => TransportError::Disconnected,
        other => TransportError::Other(other.to_string()),
    }
}

impl Transport for ElTransport {
    fn snap_peers(&self) -> Vec<PeerId> {
        let ex = self.excluded.read();
        self.net.peers().into_iter().filter(|(id, p)| (p.snap2 || p.snap1) && !ex.contains(id)).map(|(id, _)| id).collect()
    }

    async fn account(&self, peer: PeerId, root: B256, address: Address) -> Result<AccountAnswer, TransportError> {
        match self.account_shared(peer, root, address).await? {
            Some(a) => Ok(AccountAnswer::Found(a)),
            None => Ok(AccountAnswer::Unavailable),
        }
    }

    async fn storage(&self, peer: PeerId, root: B256, address: Address, slot: U256) -> Result<SlotAnswer, TransportError> {
        let sh = keccak256(slot.to_be_bytes::<32>());
        let (acct, resp) =
            tokio::join!(self.account_shared(peer, root, address), self.net.storage_range(peer, root, keccak256(address), sh, 512));
        let r = &self.rejections;
        let storage_root = match acct? {
            None => return Ok(SlotAnswer::Unavailable),
            Some(None) => return Ok(SlotAnswer::Found(U256::ZERO)), // proven-absent account
            Some(Some(a)) if a.storage_root == EMPTY_ROOT_HASH => return Ok(SlotAnswer::Found(U256::ZERO)),
            Some(Some(a)) => a.storage_root,
        };
        let m = resp.0.map_err(map_err)?;
        r.traffic(alloy_rlp::Encodable::length(&m));
        let slots = m.slots.into_iter().next().unwrap_or_default();
        if m.proof.is_empty() {
            // Only a complete small trie may come without proof: it must rebuild to the root.
            if slots.is_empty() {
                r.root_unavailable.fetch_add(1, Ordering::Relaxed);
                return Ok(SlotAnswer::Unavailable);
            }
            let leaves: Vec<(B256, Bytes)> = slots.iter().map(|d| (d.hash, d.data.clone())).collect();
            if proof::root_of(leaves) != Some(storage_root) {
                r.no_proof.fetch_add(1, Ordering::Relaxed);
                return Ok(SlotAnswer::Unavailable);
            }
            let v = slots.iter().find(|d| d.hash == sh).map(|d| U256::decode(&mut d.data.as_ref()));
            return match v {
                None => Ok(SlotAnswer::Found(U256::ZERO)),
                Some(Ok(v)) => Ok(SlotAnswer::Found(v)),
                Some(Err(_)) => Ok(SlotAnswer::Unavailable),
            };
        }
        let returned = slots.iter().find(|d| d.hash == sh);
        match proof::lookup(storage_root, sh, &m.proof) {
            Ok(Some(leaf)) => {
                if returned.is_some_and(|d| d.data.as_ref() != leaf.as_slice()) {
                    r.contradiction.fetch_add(1, Ordering::Relaxed);
                    return Ok(SlotAnswer::Unavailable);
                }
                match U256::decode(&mut leaf.as_slice()) {
                    Ok(v) => Ok(SlotAnswer::Found(v)),
                    Err(_) => {
                        r.bad_proof.fetch_add(1, Ordering::Relaxed);
                        Ok(SlotAnswer::Unavailable)
                    }
                }
            }
            Ok(None) if returned.is_none() => Ok(SlotAnswer::Found(U256::ZERO)), // proven absent
            Ok(None) => {
                r.contradiction.fetch_add(1, Ordering::Relaxed);
                Ok(SlotAnswer::Unavailable)
            }
            Err(_) => {
                r.bad_proof.fetch_add(1, Ordering::Relaxed);
                Ok(SlotAnswer::Unavailable)
            }
        }
    }

    async fn codes(&self, peer: PeerId, hashes: Vec<B256>) -> Result<Vec<Bytes>, TransportError> {
        // Codes are content-addressed: the scheduler keys each answer by keccak256(code).
        let m = self.net.byte_codes(peer, hashes, 2 << 20).await.0.map_err(map_err)?;
        self.rejections.traffic(alloy_rlp::Encodable::length(&m));
        Ok(m.codes)
    }
}
