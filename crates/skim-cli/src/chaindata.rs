//! Chain data beyond state: headers, transactions, receipts and logs, from the P2P network.
//!
//! - **Transactions sent through skim** (`eth_sendRawTransaction`) are gossiped to every
//!   connected EL peer and re-broadcast until a gossiped payload includes them.
//! - **Inclusion** is seen directly: every verified execution payload carries its transactions,
//!   so skim indexes their hashes as blocks arrive.
//! - **Receipts** come from `GetReceipts` for that one block, accepted only if they rebuild the
//!   header's `receipts_root`. Bodies of older blocks (`GetBlockBodies`) must rebuild its
//!   `transactions_root`. Headers further back are fetched and accepted only if they link by hash
//!   to a known header.
//! - **Logs** (`eth_getLogs`): a block is a candidate only if its header `logsBloom` matches the
//!   filter and, when skim saw its BAL, a filtered address is in it (a contract that emits a log
//!   is always in the block's access list). Only candidates' receipts are fetched.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_consensus::{
    Header, Transaction as _, TxReceipt, proofs::ordered_trie_root_with_encoder, transaction::SignerRecoverable,
};
use alloy_consensus::private::alloy_eips::{
    eip2718::{Decodable2718, Encodable2718, Typed2718},
    eip4895::{Withdrawal, Withdrawals},
};
use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_rpc_types_eth::{Filter, FilterBlockOption};
use futures::StreamExt;
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use skim_el::{ElNet, Receipt, SignedTx};

/// Headers kept (about 1.1 days of Sepolia; a header is about 600 bytes).
const HEADERS_MAX: usize = 8192;
/// Blocks whose transaction hashes / BAL addresses are indexed in memory.
const TX_INDEX_BLOCKS: usize = 2048;
/// Candidate blocks whose receipts a range scan holds at once.
const SCAN_CHUNK: usize = 256;
/// Blocks whose receipts / raw transactions are cached.
const BLOCK_CACHE: usize = 128;
/// Blocks whose raw access lists are kept for serving (about 100 KB each on Sepolia).
const SERVE_BALS: usize = 64;
/// Re-broadcast interval and give-up time for sent transactions.
const REBROADCAST: Duration = Duration::from_secs(12);
const SENT_TTL: Duration = Duration::from_secs(30 * 60);
/// Passes over the connected peers before a fetch reports "unavailable", with growing pauses
/// (peers come and go, and a busy one may answer a moment later).
const PASSES: u32 = 3;
const PASS_BACKOFF: Duration = Duration::from_millis(500);
/// How long a header or body fetch waits for a first eligible peer (just after start).
const PEER_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum DataError {
    /// Not served by any peer we tried (pruned, or peers busy): `-32002`.
    Unavailable(String),
    /// Bad request: `-32602` / `-32005` range limit.
    Invalid(String),
    RangeTooLarge(u64),
    /// Outside what skim serves at all (history before Amsterdam): `-32004`.
    Unsupported(String),
}

struct Sent {
    tx: SignedTx,
    from: Address,
    first: Instant,
    last: Instant,
}

#[derive(Default)]
struct Inner {
    /// Canonical headers by number (hash cached).
    headers: BTreeMap<u64, (B256, Arc<Header>)>,
    by_hash: HashMap<B256, u64>,
    /// Transaction hashes per block, from gossiped payloads or fetched bodies.
    txs: BTreeMap<u64, (B256, Arc<Vec<B256>>)>,
    tx_index: HashMap<B256, (u64, u32)>,
    /// Every address in a block's BAL, for blocks skim followed.
    touched: BTreeMap<u64, (B256, Arc<HashSet<Address>>)>,
    sent: HashMap<B256, Sent>,
}

impl Inner {
    fn put_header(&mut self, hash: B256, h: Arc<Header>) {
        if let Some((old, _)) = self.headers.insert(h.number, (hash, h.clone())) {
            self.by_hash.remove(&old);
        }
        self.by_hash.insert(hash, h.number);
    }

    fn drop_header(&mut self, n: u64) {
        if let Some((old, _)) = self.headers.remove(&n) {
            self.by_hash.remove(&old);
        }
    }
}

/// What one retained structure holds, for `skim_status.memory` and the `skim_memory_*` metrics:
/// entries, an estimate of the payload bytes they hold, and the slots allocated for them
/// (collection capacity; spare capacity is memory too). Estimates count payload and fixed
/// per-entry sizes, not allocator overhead, so they explain part of RSS, never all of it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemPart {
    pub part: &'static str,
    pub entries: usize,
    pub bytes: usize,
    pub capacity: usize,
}

/// A small insertion-ordered cache.
struct Cache<V> {
    map: HashMap<B256, V>,
    order: std::collections::VecDeque<B256>,
    cap: usize,
}

impl<V: Clone> Cache<V> {
    fn new(cap: usize) -> Self {
        Self { map: HashMap::new(), order: Default::default(), cap }
    }
    fn get(&self, k: &B256) -> Option<V> {
        self.map.get(k).cloned()
    }
    fn part(&self, part: &'static str, size: impl Fn(&V) -> usize) -> MemPart {
        let bytes = self.map.values().map(&size).sum::<usize>() + self.map.len() * (32 + std::mem::size_of::<V>());
        MemPart { part, entries: self.map.len(), bytes, capacity: self.map.capacity() }
    }
    fn put(&mut self, k: B256, v: V) {
        if self.map.insert(k, v).is_none() {
            self.order.push_back(k);
            while self.order.len() > self.cap {
                if let Some(o) = self.order.pop_front() {
                    self.map.remove(&o);
                }
            }
        }
    }
}

pub struct ChainData {
    /// The EL network. `None` only in tests: no peers, so everything comes from what's held.
    net: Option<ElNet>,
    inner: RwLock<Inner>,
    receipts: Mutex<Cache<Arc<Vec<Receipt>>>>,
    bodies: Mutex<Cache<Arc<Vec<Bytes>>>>,
    /// Block access lists (verified) of recent blocks, for transaction replay.
    bals: Mutex<Cache<Arc<skim_bal::BlockAccessList>>>,
    /// Withdrawals (checked against `withdrawals_root`) and raw BALs (as committed to by the
    /// header) of recent blocks, kept to serve other peers.
    withdrawals: Mutex<Cache<Arc<Vec<Withdrawal>>>>,
    bal_raw: Mutex<Cache<Bytes>>,
    pub max_log_range: u64,
    rr: AtomicUsize,
    /// Post-Amsterdam headers on disk (`history.redb` in the data dir), so logs and receipts
    /// cover the whole post-fork history. `None` with `--ephemeral`: memory only.
    history: std::sync::OnceLock<crate::history::HeaderStore>,
    /// Blocks whose receipts a scan is fetching right now: concurrent scans of the same blocks
    /// (several modules, or eth_getLogs) wait for that fetch and read the stored result.
    pub(crate) inflight: Mutex<HashMap<B256, Arc<tokio::sync::Semaphore>>>,
    /// Tests: what connected peers report as the oldest block they serve.
    #[cfg(test)]
    pub test_peers_from: Mutex<Option<u64>>,
    /// Tests: run once by a block stream at a named point (`before_capture`, `after_capture` of
    /// its header range, `after_scan` of its logs), e.g. to reorganize the chain mid-request.
    #[cfg(test)]
    pub test_hooks: Mutex<HashMap<&'static str, Box<dyn Fn() + Send>>>,
}

fn topics_def(f: &Filter) -> String {
    f.topics
        .iter()
        .map(|set| {
            let mut v: Vec<String> = set.iter().map(|x| x.to_string()).collect();
            v.sort();
            v.join(",")
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// Filters split into atoms: one address with its filter's topic constraints, or (a filter with
/// no address) the topic constraints alone. Each: identity hash, definition, and the atom filter.
pub(crate) fn filter_atoms(filters: &[Filter]) -> Vec<(B256, Vec<u8>, Filter)> {
    let mut out: Vec<(B256, Vec<u8>, Filter)> = Vec::new();
    for f in filters {
        let t = topics_def(f);
        let mut push = |addr: Option<Address>| {
            let def = format!("{}|{t}", addr.map_or("*".to_string(), |a| a.to_string().to_lowercase()));
            let h = keccak256(def.as_bytes());
            if out.iter().all(|(x, _, _)| *x != h) {
                let mut g = Filter::new();
                g.topics = f.topics.clone();
                if let Some(a) = addr {
                    g = g.address(a);
                }
                out.push((h, def.into_bytes(), g));
            }
        };
        if f.address.is_empty() {
            push(None);
        } else {
            for a in f.address.iter() {
                push(Some(*a));
            }
        }
    }
    out
}

/// The atom filter a stored definition describes.
fn atom_from_def(def: &[u8]) -> Option<Filter> {
    let s = std::str::from_utf8(def).ok()?;
    let (addr, topics) = s.split_once('|')?;
    let mut f = Filter::new();
    if addr != "*" {
        f = f.address(addr.parse::<Address>().ok()?);
    }
    for (i, part) in topics.split(';').enumerate().take(4) {
        let set: Vec<B256> = part.split(',').filter(|x| !x.is_empty()).map(|x| x.parse().ok()).collect::<Option<_>>()?;
        if !set.is_empty() {
            f.topics[i] = set.into();
        }
    }
    Some(f)
}

/// CPU-heavy verification (receipt roots: a trie over every receipt) runs off the async
/// runtime, a bounded number at a time, so a long scan's verification can't starve request
/// handling or the block follower.
async fn cpu<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    static POOL: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let pool = POOL.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(std::thread::available_parallelism().map_or(2, |n| n.get()).max(1))));
    cpu_on(pool, f).await
}

async fn cpu_on<T: Send + 'static>(pool: &Arc<tokio::sync::Semaphore>, f: impl FnOnce() -> T + Send + 'static) -> T {
    let permit = pool.clone().acquire_owned().await.expect("never closed");
    // The permit lives in the blocking task: if this future is dropped, the work already started
    // keeps its slot until it finishes, so the cap holds under cancellation.
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f()
    })
    .await
    .expect("verification task panicked")
}

/// Receipts checked against the header's `receipts_root` (off the async runtime).
async fn receipts_match(r: Vec<Receipt>, root: B256) -> (Vec<Receipt>, bool) {
    cpu(move || {
        let ok = Receipt::calculate_receipt_root_no_memo(&r) == root;
        (r, ok)
    })
    .await
}

/// Releases a scan's in-flight blocks (and wakes scans waiting for them) when it's done with
/// them, whether the fetch succeeded or not.
struct InflightGuard<'a> {
    map: &'a Mutex<HashMap<B256, Arc<tokio::sync::Semaphore>>>,
    hashes: Vec<B256>,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        let mut m = self.map.lock();
        for h in &self.hashes {
            if let Some(n) = m.remove(h) {
                n.close();
            }
        }
    }
}

/// A block with logs matching a scan's filters.
pub struct ScanBlock {
    pub number: u64,
    pub hash: B256,
    pub parent_hash: B256,
    pub timestamp: u64,
    pub logs: Vec<Value>,
}

/// A log scan: the blocks with matches, and candidate blocks whose receipts are no longer served
/// by any peer and aren't stored locally. (Which chain it covers is the caller's captured range.)
pub struct Scan {
    pub blocks: Vec<ScanBlock>,
    pub expired: Vec<u64>,
}

fn tx_hashes(raw: &[Bytes]) -> Vec<B256> {
    raw.iter().map(keccak256).collect()
}

impl ChainData {
    pub fn new(net: ElNet, max_log_range: u64) -> Arc<Self> {
        Self::with_net(Some(net), max_log_range)
    }

    /// Without a network (tests): no peers; headers, receipts and transactions come only from
    /// what's been noted.
    #[cfg(test)]
    pub fn offline(max_log_range: u64) -> Arc<Self> {
        Self::with_net(None, max_log_range)
    }

    /// Seeds a block's receipts and transaction hashes, as if fetched and verified (tests).
    #[cfg(test)]
    pub fn note_receipts(&self, number: u64, hash: B256, receipts: Vec<Receipt>, txs: Vec<B256>) {
        self.receipts.lock().put(hash, Arc::new(receipts));
        let mut g = self.inner.write();
        self.index_txs(&mut g, number, hash, txs);
    }

    fn with_net(net: Option<ElNet>, max_log_range: u64) -> Arc<Self> {
        let cd = Arc::new(Self {
            net,
            inner: Default::default(),
            receipts: Mutex::new(Cache::new(BLOCK_CACHE)),
            bodies: Mutex::new(Cache::new(BLOCK_CACHE)),
            bals: Mutex::new(Cache::new(32)),
            withdrawals: Mutex::new(Cache::new(BLOCK_CACHE)),
            bal_raw: Mutex::new(Cache::new(SERVE_BALS)),
            max_log_range,
            rr: AtomicUsize::new(0),
            history: Default::default(),
            inflight: Default::default(),
            #[cfg(test)]
            test_peers_from: Default::default(),
            #[cfg(test)]
            test_hooks: Default::default(),
        });
        let me = Arc::downgrade(&cd);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let Some(cd) = me.upgrade() else { return };
                cd.rebroadcast();
            }
        });
        cd
    }

    /// The post-fork header store, if any (tests).
    #[cfg(test)]
    #[cfg(test)]
    pub fn run_test_hook(&self, stage: &'static str) {
        let f = self.test_hooks.lock().remove(stage);
        if let Some(f) = f {
            f();
        }
    }

    /// Retained block data, by structure (see [`MemPart`]). Walks the caches: a few milliseconds.
    pub fn memory_parts(&self) -> Vec<MemPart> {
        use std::mem::size_of;
        let g = self.inner.read();
        let header = |h: &Header| size_of::<Header>() + h.extra_data.len();
        let mut out = vec![
            MemPart {
                part: "chain.headers",
                entries: g.headers.len(),
                bytes: g.headers.values().map(|(_, h)| 32 + header(h)).sum::<usize>() + g.by_hash.len() * 40,
                capacity: g.by_hash.capacity(),
            },
            MemPart {
                part: "chain.tx_hashes",
                entries: g.txs.len(),
                bytes: g.txs.values().map(|(_, t)| 48 + t.len() * 32).sum(),
                capacity: g.txs.values().map(|(_, t)| t.capacity()).sum(),
            },
            MemPart { part: "chain.tx_index", entries: g.tx_index.len(), bytes: g.tx_index.len() * (32 + 12), capacity: g.tx_index.capacity() },
            MemPart {
                part: "chain.touched",
                entries: g.touched.len(),
                bytes: g.touched.values().map(|(_, t)| 48 + t.len() * 20).sum(),
                capacity: g.touched.values().map(|(_, t)| t.capacity()).sum(),
            },
            MemPart { part: "chain.sent_txs", entries: g.sent.len(), bytes: g.sent.values().map(|s| alloy_rlp::Encodable::length(&s.tx) + 64).sum(), capacity: g.sent.capacity() },
        ];
        drop(g);
        let log_bytes = |l: &alloy_primitives::Log| 20 + l.data.topics().len() * 32 + l.data.data.len() + 48;
        out.push(self.receipts.lock().part("chain.receipts", |r| r.iter().map(|x| size_of::<Receipt>() + x.logs.iter().map(log_bytes).sum::<usize>()).sum()));
        out.push(self.bodies.lock().part("chain.bodies", |b| b.iter().map(|t| t.len() + 24).sum()));
        out.push(self.bals.lock().part("chain.bals_decoded", |b| alloy_rlp::Encodable::length(&**b)));
        out.push(self.withdrawals.lock().part("chain.withdrawals", |w| w.len() * size_of::<Withdrawal>()));
        out.push(self.bal_raw.lock().part("chain.bals_raw", |b| b.len()));
        let inflight = self.inflight.lock().len();
        out.push(MemPart { part: "chain.scan_inflight", entries: inflight, bytes: inflight * 48, capacity: inflight });
        out
    }

    /// Tests: a block's (verified) access list, as gossip would have delivered it.
    #[cfg(test)]
    pub fn note_bal(&self, hash: B256, bal: skim_bal::BlockAccessList) {
        self.bals.lock().put(hash, Arc::new(bal));
    }

    #[cfg(test)]
    pub fn history_store(&self) -> Option<&crate::history::HeaderStore> {
        self.history.get()
    }

    /// Keeps post-Amsterdam headers in `store` from now on (and serves logs from them).
    pub fn set_history(&self, store: crate::history::HeaderStore) {
        let _ = self.history.set(store);
    }

    fn store_tip(&self, hash: B256, h: &Header) {
        if let Some(s) = self.history.get()
            && let Err(e) = s.put_tip(hash, h)
        {
            tracing::warn!("{e}");
        }
    }

    // ---- ingestion -------------------------------------------------------------------------

    /// A canonical header (from gossip, gap fill or the chain placer). A different hash at a
    /// known number is a reorg: everything indexed at and above it is dropped.
    pub fn note_header(&self, h: &Header) {
        let hash = h.hash_slow();
        let mut g = self.inner.write();
        if g.headers.get(&h.number).is_some_and(|(x, _)| *x != hash) {
            let stale: Vec<u64> = g.headers.range(h.number..).map(|(n, _)| *n).collect();
            for n in stale {
                g.drop_header(n);
                if let Some((_, hs)) = g.txs.remove(&n) {
                    for t in hs.iter() {
                        g.tx_index.remove(t);
                    }
                }
                g.touched.remove(&n);
            }
        }
        g.put_header(hash, Arc::new(h.clone()));
        drop(g);
        self.store_tip(hash, h);
        let mut g = self.inner.write();
        while g.headers.len() > HEADERS_MAX {
            let Some((&n, _)) = g.headers.first_key_value() else { break };
            g.drop_header(n);
        }
    }

    /// Transactions, withdrawals, BAL (decoded and raw) and BAL addresses of a verified gossiped
    /// payload.
    #[allow(clippy::too_many_arguments)]
    pub fn note_payload(
        &self,
        number: u64,
        hash: B256,
        raw_txs: Vec<Bytes>,
        withdrawals: Vec<Withdrawal>,
        touched: HashSet<Address>,
        bal: Arc<skim_bal::BlockAccessList>,
        bal_raw: Bytes,
    ) {
        let hashes = tx_hashes(&raw_txs);
        self.bodies.lock().put(hash, Arc::new(raw_txs));
        self.withdrawals.lock().put(hash, Arc::new(withdrawals));
        self.bals.lock().put(hash, bal);
        self.bal_raw.lock().put(hash, bal_raw);
        let mut g = self.inner.write();
        self.index_txs(&mut g, number, hash, hashes);
        g.touched.insert(number, (hash, Arc::new(touched)));
        while g.touched.len() > TX_INDEX_BLOCKS {
            g.touched.pop_first();
        }
    }

    fn index_txs(&self, g: &mut Inner, number: u64, hash: B256, hashes: Vec<B256>) {
        for (i, t) in hashes.iter().enumerate() {
            g.tx_index.insert(*t, (number, i as u32));
            if g.sent.contains_key(t) {
                tracing::info!(tx = %t, number, "sent transaction included");
            }
        }
        if let Some((_, old)) = g.txs.insert(number, (hash, Arc::new(hashes))) {
            for t in old.iter() {
                if g.tx_index.get(t).is_some_and(|(n, _)| *n == number) {
                    g.tx_index.remove(t);
                }
            }
        }
        while g.txs.len() > TX_INDEX_BLOCKS {
            if let Some((_, (_, hs))) = g.txs.pop_first() {
                for t in hs.iter() {
                    g.tx_index.remove(t);
                }
            }
        }
    }

    // ---- sending ---------------------------------------------------------------------------

    /// Decodes, checks and gossips a signed transaction. `nonce`/`balance` are the sender's
    /// state at the head (for the same early errors a node's pool would give).
    pub fn decode_raw(raw: &Bytes) -> Result<(SignedTx, Address), DataError> {
        let tx = SignedTx::decode_2718(&mut raw.as_ref()).map_err(|e| DataError::Invalid(format!("invalid transaction: {e}")))?;
        if tx.is_eip4844() {
            return Err(DataError::Invalid("blob transactions are not supported (no sidecar gossip)".into()));
        }
        if tx.chain_id().is_some_and(|c| c != skim_sim::env::SEPOLIA_CHAIN_ID) {
            return Err(DataError::Invalid(format!("wrong chain id {:?}", tx.chain_id())));
        }
        let from = tx.recover_signer().map_err(|e| DataError::Invalid(format!("invalid signature: {e}")))?;
        Ok((tx, from))
    }

    /// Gossips `tx` and tracks it until inclusion. Returns the hash and how many peers got it.
    pub fn send(&self, tx: SignedTx, from: Address) -> (B256, usize) {
        let hash = *tx.tx_hash();
        let peers = self.net.as_ref().map_or(0, |n| n.broadcast(vec![tx.clone()]));
        let now = Instant::now();
        self.inner.write().sent.entry(hash).or_insert(Sent { tx, from, first: now, last: now });
        tracing::info!(tx = %hash, %from, peers, "transaction broadcast");
        (hash, peers)
    }

    fn rebroadcast(&self) {
        let mut due = Vec::new();
        {
            let mut g = self.inner.write();
            let included: HashSet<B256> = g.sent.keys().filter(|h| g.tx_index.contains_key(*h)).copied().collect();
            g.sent.retain(|h, s| included.contains(h) || s.first.elapsed() < SENT_TTL);
            for (h, s) in g.sent.iter_mut() {
                if !included.contains(h) && s.last.elapsed() >= REBROADCAST {
                    s.last = Instant::now();
                    due.push(s.tx.clone());
                }
            }
        }
        if !due.is_empty() {
            let peers = self.net.as_ref().map_or(0, |n| n.broadcast(due.clone()));
            tracing::debug!(txs = due.len(), peers, "re-broadcast pending transactions");
        }
    }

    /// Highest pending (sent, not yet included) nonce of `from` + 1, if any.
    pub fn pending_nonce(&self, from: Address, latest: u64) -> u64 {
        let g = self.inner.read();
        let mut nonces: Vec<u64> = g
            .sent
            .iter()
            .filter(|(h, s)| s.from == from && !g.tx_index.contains_key(*h) && s.tx.nonce() >= latest)
            .map(|(_, s)| s.tx.nonce())
            .collect();
        nonces.sort();
        // Only a gap-free run counts (a node's pool would not make a gapped tx pending).
        let mut next = latest;
        for n in nonces {
            if n == next {
                next += 1;
            }
        }
        next
    }

    // ---- headers, bodies, receipts ---------------------------------------------------------

    /// The network, for requests to a peer from [`Self::peers`] (which is empty without one).
    fn el(&self) -> &ElNet {
        self.net.as_ref().expect("peers come from a network")
    }

    /// Peers for receipts and access lists (eth/70+ requests), rotated so load spreads.
    fn peers(&self) -> Vec<skim_el::Peer> {
        self.peers_min(70)
    }

    /// Peers for headers and bodies, which every eth version serves. Right after start few peers
    /// speak eth/70+, so these don't share that filter; with none connected yet, waits up to
    /// `PEER_WAIT` for one instead of failing at once.
    async fn header_peers(&self) -> Vec<skim_el::Peer> {
        let deadline = tokio::time::Instant::now() + PEER_WAIT;
        loop {
            let v = self.peers_min(66);
            if !v.is_empty() || tokio::time::Instant::now() >= deadline || self.net.is_none() {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    fn peers_min(&self, eth: u8) -> Vec<skim_el::Peer> {
        let mut v: Vec<_> = self
            .net
            .as_ref()
            .map(|n| n.peers())
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, p)| p.eth_version >= eth && !p.client.starts_with("enrscout"))
            .map(|(id, _)| id)
            .collect();
        if !v.is_empty() {
            let k = self.rr.fetch_add(1, Ordering::Relaxed) % v.len();
            v.rotate_left(k);
        }
        v
    }

    /// The oldest block any connected peer serves receipts for (eth/69 `earliest_block`), and how
    /// many peers reported one. Older receipts come only from what skim already holds.
    pub fn peers_serve_from(&self) -> (Option<u64>, usize) {
        #[cfg(test)]
        if let Some(p) = *self.test_peers_from.lock() {
            return (Some(p), 1);
        }
        let e: Vec<u64> = self
            .net
            .as_ref()
            .map(|n| n.peers())
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, p)| p.eth_version >= 70 && !p.client.starts_with("enrscout"))
            .filter_map(|(_, p)| p.status_earliest)
            .collect();
        (e.iter().min().copied(), e.len())
    }

    /// Where `eth_getLogs` history starts: the fork floor (headers), and what peers still serve.
    pub fn history_json(&self) -> Value {
        let floor = self.history.get().and_then(|s| s.floor().ok().flatten());
        let (peers_from, reporting) = self.peers_serve_from();
        let mem_from = self.inner.read().headers.first_key_value().map(|(n, _)| *n);
        let logs_from = if self.history.get().is_none() {
            // Head-only (`--history off`): the in-memory window.
            mem_from
        } else {
            match (floor, peers_from) {
                (Some(f), Some(p)) => Some(f.max(p)),
                (f, p) => p.or(f),
            }
        };
        json!({
            "fork_floor": floor,
            "peers_serve_from": peers_from,
            "peers_reporting": reporting,
            "logs_from": logs_from,
            "memory_headers_from": mem_from,
            "mode": if self.history.get().is_some() { "local" } else { "off" },
        })
    }

    /// Makes headers down to `number` available: in memory for recent blocks; below that, on
    /// disk (post-Amsterdam history), walking back by hash from the lowest linked header.
    pub(crate) async fn ensure_headers(&self, number: u64) -> Result<(), DataError> {
        let Some(store) = self.history.get() else { return self.ensure_headers_mem(number).await };
        if let Ok(Some(floor)) = store.floor()
            && number < floor
        {
            return Err(DataError::Unsupported(format!("block {number} is before Amsterdam (block {floor}): history before the fork isn't served")));
        }
        let (mut low, mut parent) = {
            let g = self.inner.read();
            let Some((n, (_, h))) = g.headers.first_key_value() else {
                return Err(DataError::Unavailable("no headers yet".into()));
            };
            (*n, h.parent_hash)
        };
        let unavailable = |e: crate::history::StoreError| DataError::Unavailable(e.to_string());
        while low > number {
            // Already stored and on this chain (by hash): jump to the bottom of that run.
            if let Some((l, p)) = store.linked_low(low - 1, parent).map_err(unavailable)? {
                (low, parent) = (l, p);
                continue;
            }
            let want = (low - number).min(1024);
            let mut got = None;
            for pass in 0..PASSES {
                if pass > 0 {
                    tokio::time::sleep(PASS_BACKOFF * pass).await;
                }
                for p in self.header_peers().await.into_iter().take(8) {
                    let hs = match self.el().headers_falling(p, parent.into(), want).await {
                        (Ok(hs), _) => hs,
                        (Err(e), d) => {
                            tracing::debug!(peer = %p, low, ms = d.as_millis() as u64, "history headers: {e}");
                            continue;
                        }
                    };
                    let n = hs.len();
                    let mut expect = parent;
                    let mut ok = Vec::new();
                    for h in hs {
                        let hh = h.hash_slow();
                        if hh != expect || h.number + 1 != low - ok.len() as u64 {
                            break;
                        }
                        expect = h.parent_hash;
                        ok.push((hh, h));
                    }
                    if ok.is_empty() {
                        tracing::debug!(peer = %p, low, returned = n, "history headers: response does not link to our lowest header");
                    } else {
                        got = Some(ok);
                        break;
                    }
                }
                if got.is_some() {
                    break;
                }
            }
            let Some(ok) = got else {
                return Err(DataError::Unavailable(format!("headers below {low} not served by peers")));
            };
            if let Some(pre) = ok.iter().find(|(_, h)| !crate::history::post_fork(h)) {
                let floor = pre.1.number + 1;
                let _ = store.set_floor(floor);
                tracing::info!(floor, "history: reached Amsterdam; earlier blocks aren't served");
                store.put_run(&ok.iter().map(|(x, h)| (*x, h)).collect::<Vec<_>>()).map_err(unavailable)?;
                if number < floor {
                    return Err(DataError::Unsupported(format!("block {number} is before Amsterdam (block {floor}): history before the fork isn't served")));
                }
                return Ok(());
            }
            store.put_run(&ok.iter().map(|(x, h)| (*x, h)).collect::<Vec<_>>()).map_err(unavailable)?;
            let last = &ok.last().expect("non-empty").1;
            (low, parent) = (last.number, last.parent_hash);
            if low % 16384 < 1024 {
                tracing::info!(low, target = number, "history: headers walked back");
            }
        }
        Ok(())
    }

    /// Without a header store: walk back in memory (about a day of headers).
    async fn ensure_headers_mem(&self, number: u64) -> Result<(), DataError> {
        for _ in 0..64 {
            let (oldest, parent) = {
                let g = self.inner.read();
                let Some((n, (_, h))) = g.headers.first_key_value() else {
                    return Err(DataError::Unavailable("no headers yet".into()));
                };
                (*n, h.parent_hash)
            };
            if oldest <= number || oldest == 0 {
                return Ok(());
            }
            let want = (oldest - number).min(256);
            let mut done = false;
            for pass in 0..PASSES {
                if pass > 0 {
                    tokio::time::sleep(PASS_BACKOFF * pass).await;
                }
                for p in self.header_peers().await.into_iter().take(8) {
                    let hs = match self.el().headers_falling(p, parent.into(), want).await {
                        (Ok(hs), _) => hs,
                        (Err(e), _) => {
                            tracing::debug!(peer = %p, "headers: {e}");
                            continue;
                        }
                    };
                    // Accept only a hash-linked chain ending at our oldest header.
                    let mut expect = parent;
                    let mut ok = Vec::new();
                    for h in hs {
                        let hh = h.hash_slow();
                        if hh != expect {
                            break;
                        }
                        expect = h.parent_hash;
                        ok.push((hh, h));
                    }
                    if ok.is_empty() {
                        tracing::debug!(peer = %p, "headers: response does not link to our oldest header");
                        continue;
                    }
                    let mut g = self.inner.write();
                    for (hh, h) in ok {
                        g.put_header(hh, Arc::new(h));
                    }
                    done = true;
                    break;
                }
                if done {
                    break;
                }
            }
            if !done {
                return Err(DataError::Unavailable(format!("headers below {oldest} not served by peers")));
            }
        }
        Ok(())
    }

    /// Raw transactions of a block, verified against `transactions_root`.
    pub async fn block_txs(&self, hash: B256, header: &Header) -> Result<Arc<Vec<Bytes>>, DataError> {
        self.fetch_block_txs(hash, header, true).await
    }

    /// A block's transaction hashes for a log scan: the body is fetched (and verified) but not
    /// kept, so a long scan over history doesn't fill the body cache and tx index with old blocks.
    async fn scan_tx_hashes(&self, n: u64, hash: B256, header: &Header) -> Result<Arc<Vec<B256>>, DataError> {
        if let Some(hs) = self.block_tx_hashes(n, hash) {
            return Ok(hs);
        }
        Ok(Arc::new(tx_hashes(&self.fetch_block_txs(hash, header, false).await?)))
    }

    async fn fetch_block_txs(&self, hash: B256, header: &Header, keep: bool) -> Result<Arc<Vec<Bytes>>, DataError> {
        if let Some(b) = self.bodies.lock().get(&hash) {
            return Ok(b);
        }
        for pass in 0..PASSES {
            if pass > 0 {
                tokio::time::sleep(PASS_BACKOFF * pass).await;
            }
            for p in self.header_peers().await.into_iter().take(5) {
                let mut bodies = match self.el().bodies(p, vec![hash]).await {
                    (Ok(b), _) => b,
                    (Err(e), _) => {
                        tracing::debug!(peer = %p, number = header.number, "bodies: {e}");
                        continue;
                    }
                };
                let Some(body) = bodies.pop() else { continue };
                let raw: Vec<Bytes> = body.transactions.iter().map(|t| Bytes::from(t.encoded_2718())).collect();
                let root = ordered_trie_root_with_encoder(&raw, |t: &Bytes, buf| buf.extend_from_slice(t));
                if root != header.transactions_root {
                    tracing::debug!(peer = %p, number = header.number, "body does not match transactions_root");
                    continue;
                }
                let raw = Arc::new(raw);
                if !keep {
                    return Ok(raw);
                }
                self.bodies.lock().put(hash, raw.clone());
                if let Some(w) = body.withdrawals
                    && Some(alloy_consensus::proofs::calculate_withdrawals_root(&w)) == header.withdrawals_root
                {
                    self.withdrawals.lock().put(hash, Arc::new(w.into_inner()));
                }
                let mut g = self.inner.write();
                self.index_txs(&mut g, header.number, hash, tx_hashes(&raw));
                return Ok(raw);
            }
        }
        Err(DataError::Unavailable(format!("block {} body not served by peers", header.number)))
    }

    /// All receipts of a block, verified against `receipts_root`.
    pub async fn block_receipts(&self, hash: B256, header: &Header) -> Result<Arc<Vec<Receipt>>, DataError> {
        if header.receipts_root == alloy_consensus::EMPTY_ROOT_HASH {
            return Ok(Arc::new(Vec::new()));
        }
        if let Some(r) = self.receipts.lock().get(&hash) {
            return Ok(r);
        }
        for pass in 0..PASSES {
            if pass > 0 {
                tokio::time::sleep(PASS_BACKOFF * pass).await;
            }
            for p in self.peers().into_iter().take(6) {
                let (r, d) = match self.el().block_receipts(p, hash).await {
                    (Ok(r), d) => (r, d),
                    (Err(e), _) => {
                        tracing::debug!(peer = %p, number = header.number, "receipts: {e}");
                        continue;
                    }
                };
                let (r, ok) = receipts_match(r, header.receipts_root).await;
                if !ok {
                    tracing::debug!(peer = %p, number = header.number, n = r.len(), "receipts do not match receipts_root");
                    continue;
                }
                tracing::debug!(number = header.number, n = r.len(), ms = d.as_millis() as u64, "receipts fetched");
                let r = Arc::new(r);
                self.receipts.lock().put(hash, r.clone());
                return Ok(r);
            }
        }
        Err(DataError::Unavailable(format!("block {} receipts not served by peers", header.number)))
    }

    /// Receipts of many blocks (verified against each `receipts_root`), fetched in batches of
    /// hashes per request and several requests in flight: for range scans, where one request
    /// per block would be hundreds of round trips. Blocks no peer served are left out; callers
    /// fall back to [`Self::block_receipts`].
    async fn receipts_many(&self, blocks: &[(B256, Arc<Header>)]) -> HashMap<B256, Arc<Vec<Receipt>>> {
        const PER_REQUEST: usize = 32;
        let mut out = HashMap::new();
        let mut todo = Vec::new();
        for (hash, h) in blocks {
            if h.receipts_root == alloy_consensus::EMPTY_ROOT_HASH {
                out.insert(*hash, Arc::new(Vec::new()));
            } else if let Some(r) = self.receipts.lock().get(hash) {
                out.insert(*hash, r);
            } else {
                todo.push((*hash, h.clone()));
            }
        }
        for _pass in 0..PASSES {
            if todo.is_empty() {
                break;
            }
            let chunks: Vec<Vec<(B256, Arc<Header>)>> = todo.chunks(PER_REQUEST).map(|c| c.to_vec()).collect();
            let got: Vec<Vec<(B256, Arc<Vec<Receipt>>)>> = futures::stream::iter(chunks)
                .map(|chunk| async move {
                    let hashes: Vec<B256> = chunk.iter().map(|(h, _)| *h).collect();
                    for p in self.peers().into_iter().take(3) {
                        let Ok(rs) = self.el().blocks_receipts(p, &hashes).await.0 else { continue };
                        let roots: Vec<(B256, B256)> = chunk.iter().map(|(hash, h)| (*hash, h.receipts_root)).collect();
                        let ok: Vec<_> = cpu(move || {
                            rs.into_iter()
                                .zip(roots)
                                .filter(|(r, (_, root))| Receipt::calculate_receipt_root_no_memo(r) == *root)
                                .map(|(r, (hash, _))| (hash, Arc::new(r)))
                                .collect::<Vec<_>>()
                        })
                        .await;
                        if !ok.is_empty() {
                            return ok;
                        }
                    }
                    Vec::new()
                })
                .buffer_unordered(16)
                .collect()
                .await;
            for (hash, r) in got.into_iter().flatten() {
                out.insert(hash, r);
            }
            todo.retain(|(h, _)| !out.contains_key(h));
        }
        out
    }

    /// Where a transaction is: (number, block hash, header, index), if skim indexed it.
    /// A block's access list, verified against its header (gossip cache, else eth/71 peers).
    pub async fn block_bal(&self, hash: B256, header: &Header) -> Result<Arc<skim_bal::BlockAccessList>, DataError> {
        if let Some(b) = self.bals.lock().get(&hash) {
            return Ok(b);
        }
        for pass in 0..PASSES {
            if pass > 0 {
                tokio::time::sleep(PASS_BACKOFF * pass).await;
            }
            for p in self.peers().into_iter().take(4) {
                let Ok(mut r) = self.el().bals(p, vec![hash]).await.0 else { continue };
                let Some(Some(raw)) = r.0.pop() else { continue };
                if let Ok(bal) = skim_bal::decode_verified(&raw, header.block_access_list_hash.unwrap_or_default()) {
                    let bal = Arc::new(bal);
                    self.bals.lock().put(hash, bal.clone());
                    self.bal_raw.lock().put(hash, raw);
                    return Ok(bal);
                }
            }
        }
        Err(DataError::Unavailable(format!("block {} access list not served by peers", header.number)))
    }

    /// Where a transaction is: (number, block hash, header, index).
    pub fn locate_tx(&self, tx: &B256) -> Option<(u64, B256, Arc<Header>, u32)> {
        self.locate(tx)
    }

    /// A block's raw transactions and their receipts (both verified).
    pub async fn block_txs_and_receipts(&self, hash: B256, header: &Header) -> Result<(Arc<Vec<Bytes>>, Arc<Vec<Receipt>>), DataError> {
        Ok((self.block_txs(hash, header).await?, self.block_receipts(hash, header).await?))
    }

    /// Decodes a raw transaction and recovers its sender.
    pub fn decode_tx(raw: &Bytes) -> Result<(SignedTx, Address), DataError> {
        Self::decode_any(raw)
    }

    fn locate(&self, tx: &B256) -> Option<(u64, B256, Arc<Header>, u32)> {
        let g = self.inner.read();
        let (n, i) = *g.tx_index.get(tx)?;
        let (bh, h) = g.headers.get(&n)?.clone();
        (g.txs.get(&n)?.0 == bh).then_some((n, bh, h, i))
    }

    // ---- RPC views -------------------------------------------------------------------------

    fn tx_json(tx: &SignedTx, from: Address, at: Option<(u64, B256, &Header, u32)>) -> Value {
        let rpc = alloy_rpc_types_eth::Transaction {
            inner: alloy_consensus::transaction::Recovered::new_unchecked(tx.clone(), from),
            block_hash: at.map(|a| a.1),
            block_number: at.map(|a| a.0),
            transaction_index: at.map(|a| a.3 as u64),
            effective_gas_price: at.map(|a| tx.effective_gas_price(a.2.base_fee_per_gas)),
            block_timestamp: None,
        };
        serde_json::to_value(rpc).unwrap_or(Value::Null)
    }

    /// `eth_getTransactionByHash`: sent transactions (pending or included) and transactions of
    /// indexed blocks. `null` if unknown.
    pub async fn transaction(&self, hash: B256) -> Result<Value, DataError> {
        let sent = self.inner.read().sent.get(&hash).map(|s| (s.tx.clone(), s.from));
        match (self.locate(&hash), sent) {
            (Some((n, bh, h, i)), Some((tx, from))) => Ok(Self::tx_json(&tx, from, Some((n, bh, &h, i)))),
            (None, Some((tx, from))) => Ok(Self::tx_json(&tx, from, None)),
            (Some((n, bh, h, i)), None) => {
                let raw = self.block_txs(bh, &h).await?;
                let r = raw.get(i as usize).ok_or_else(|| DataError::Unavailable("index out of range".into()))?;
                let (tx, from) = Self::decode_any(r)?;
                Ok(Self::tx_json(&tx, from, Some((n, bh, &h, i))))
            }
            (None, None) => Ok(Value::Null),
        }
    }

    fn decode_any(raw: &Bytes) -> Result<(SignedTx, Address), DataError> {
        let tx = SignedTx::decode_2718(&mut raw.as_ref()).map_err(|e| DataError::Unavailable(format!("decode: {e}")))?;
        let from = tx.recover_signer().map_err(|e| DataError::Unavailable(format!("signer: {e}")))?;
        Ok((tx, from))
    }

    fn log_json(log: &alloy_primitives::Log, bh: B256, n: u64, ts: u64, txh: B256, ti: u32, li: u64) -> Value {
        json!({
            "address": log.address, "topics": log.topics(), "data": log.data.data,
            "blockHash": bh, "blockNumber": format!("{n:#x}"), "blockTimestamp": format!("{ts:#x}"),
            "transactionHash": txh, "transactionIndex": format!("{ti:#x}"), "logIndex": format!("{li:#x}"),
            "removed": false,
        })
    }

    /// `eth_getTransactionReceipt`. `null` while pending or unknown (clients poll).
    pub async fn receipt(&self, hash: B256) -> Result<Value, DataError> {
        let Some((n, bh, h, i)) = self.locate(&hash) else { return Ok(Value::Null) };
        let receipts = self.block_receipts(bh, &h).await?;
        let raw = self.block_txs(bh, &h).await?;
        Self::receipt_json(n, bh, &h, i as usize, &receipts, &raw)
    }

    /// Receipt `i` of a block, in the shape nodes return.
    fn receipt_json(n: u64, bh: B256, h: &Header, i: usize, receipts: &[Receipt], raw: &[Bytes]) -> Result<Value, DataError> {
        let r = receipts.get(i).ok_or_else(|| DataError::Unavailable("receipt index out of range".into()))?;
        let rawtx = raw.get(i).ok_or_else(|| DataError::Unavailable("transaction index out of range".into()))?;
        let hash = keccak256(rawtx);
        let (tx, from) = Self::decode_any(rawtx)?;
        let prev = if i == 0 { 0 } else { receipts[i - 1].cumulative_gas_used };
        let first_log: u64 = receipts[..i].iter().map(|r| r.logs.len() as u64).sum();
        let logs: Vec<Value> = r
            .logs
            .iter()
            .enumerate()
            .map(|(k, l)| Self::log_json(l, bh, n, h.timestamp, hash, i as u32, first_log + k as u64))
            .collect();
        let contract = tx.to().is_none().then(|| from.create(tx.nonce()));
        Ok(json!({
            "transactionHash": hash, "transactionIndex": format!("{i:#x}"),
            "blockHash": bh, "blockNumber": format!("{n:#x}"),
            "from": from, "to": tx.to(), "contractAddress": contract,
            "cumulativeGasUsed": format!("{:#x}", r.cumulative_gas_used),
            "gasUsed": format!("{:#x}", r.cumulative_gas_used - prev),
            "effectiveGasPrice": format!("{:#x}", tx.effective_gas_price(h.base_fee_per_gas)),
            "logs": logs, "logsBloom": r.bloom(),
            "type": format!("{:#x}", tx.ty()), "status": if r.success { "0x1" } else { "0x0" },
        }))
    }

    /// `eth_getBlockReceipts`: every receipt of a block (verified against `receiptsRoot`).
    pub async fn block_receipts_json(&self, n: u64, bh: B256, h: &Header) -> Result<Value, DataError> {
        let receipts = self.block_receipts(bh, h).await?;
        let raw = self.block_txs(bh, h).await?;
        (0..raw.len()).map(|i| Self::receipt_json(n, bh, h, i, &receipts, &raw)).collect::<Result<Vec<_>, _>>().map(Value::Array)
    }

    /// A block's transactions: hashes, or full objects (body verified against `transactionsRoot`).
    pub async fn block_transactions_json(&self, n: u64, bh: B256, h: &Header, full: bool) -> Result<Vec<Value>, DataError> {
        if h.transactions_root == alloy_consensus::EMPTY_ROOT_HASH {
            return Ok(Vec::new());
        }
        if !full && let Some(hs) = self.block_tx_hashes(n, bh) {
            return Ok(hs.iter().map(|x| json!(x)).collect());
        }
        let raw = self.block_txs(bh, h).await?;
        if !full {
            return Ok(raw.iter().map(|r| json!(keccak256(r))).collect());
        }
        raw.iter()
            .enumerate()
            .map(|(i, r)| Self::decode_any(r).map(|(tx, from)| Self::tx_json(&tx, from, Some((n, bh, h, i as u32)))))
            .collect()
    }

    /// `eth_feeHistory` for `count` blocks ending at `newest` (resolved). Base fees and gas ratios
    /// come from headers; reward percentiles need the blocks' transactions and receipts, fetched
    /// (verified) only when percentiles are asked for. Blob fee fields are omitted: they depend on
    /// the network's blob-parameter schedule, which skim doesn't track yet.
    pub async fn fee_history(&self, count: u64, newest: u64, percentiles: &[f64]) -> Result<Value, DataError> {
        if count == 0 {
            return Ok(json!({"oldestBlock": "0x0", "baseFeePerGas": [], "gasUsedRatio": []}));
        }
        if percentiles.iter().any(|p| !(0.0..=100.0).contains(p)) || percentiles.windows(2).any(|w| w[0] > w[1]) {
            return Err(DataError::Invalid("reward percentiles must be ascending, within 0..=100".into()));
        }
        let count = count.min(1024).min(newest + 1);
        let oldest = newest + 1 - count;
        self.ensure_headers(oldest).await?;
        let headers: Vec<(B256, Arc<Header>)> = {
            let g = self.inner.read();
            (oldest..=newest)
                .map(|n| g.headers.get(&n).cloned().ok_or_else(|| DataError::Unavailable(format!("no header for block {n}"))))
                .collect::<Result<_, _>>()?
        };
        let mut base_fees: Vec<Value> = headers.iter().map(|(_, h)| json!(format!("{:#x}", h.base_fee_per_gas.unwrap_or_default()))).collect();
        let last = &headers.last().expect("count >= 1").1;
        let next = last.next_block_base_fee(alloy_consensus::private::alloy_eips::eip1559::BaseFeeParams::ethereum()).unwrap_or_default();
        base_fees.push(json!(format!("{next:#x}")));
        let ratios: Vec<Value> = headers.iter().map(|(_, h)| json!(if h.gas_limit == 0 { 0.0 } else { h.gas_used as f64 / h.gas_limit as f64 })).collect();
        let mut out = json!({"oldestBlock": format!("{oldest:#x}"), "baseFeePerGas": base_fees, "gasUsedRatio": ratios});
        if !percentiles.is_empty() {
            let rewards: Vec<Result<Value, DataError>> = futures::stream::iter(headers.iter().cloned())
                .map(|(bh, h)| async move {
                    let base = h.base_fee_per_gas.unwrap_or_default();
                    if h.gas_used == 0 || h.transactions_root == alloy_consensus::EMPTY_ROOT_HASH {
                        return Ok(json!(percentiles.iter().map(|_| "0x0").collect::<Vec<_>>()));
                    }
                    let raw = self.block_txs(bh, &h).await?;
                    let receipts = self.block_receipts(bh, &h).await?;
                    // (effective tip, gas used) per transaction, ordered by tip (as geth does).
                    let mut txs: Vec<(u128, u64)> = Vec::with_capacity(raw.len());
                    let mut prev = 0u64;
                    for (r, rec) in raw.iter().zip(receipts.iter()) {
                        let (tx, _) = Self::decode_any(r)?;
                        let tip = tx.effective_tip_per_gas(base).unwrap_or_default();
                        txs.push((tip, rec.cumulative_gas_used - prev));
                        prev = rec.cumulative_gas_used;
                    }
                    txs.sort_by_key(|t| t.0);
                    let total = h.gas_used; // geth's threshold basis: the header's gas used
                    let mut out = Vec::with_capacity(percentiles.len());
                    let (mut i, mut acc) = (0usize, txs.first().map_or(0, |t| t.1));
                    for p in percentiles {
                        let threshold = (total as f64 * p / 100.0) as u64;
                        while acc < threshold && i + 1 < txs.len() {
                            i += 1;
                            acc += txs[i].1;
                        }
                        out.push(format!("{:#x}", txs.get(i).map_or(0, |t| t.0)));
                    }
                    Ok(json!(out))
                })
                .buffered(8)
                .collect()
                .await;
            out["reward"] = Value::Array(rewards.into_iter().collect::<Result<Vec<_>, _>>()?);
        }
        Ok(out)
    }

    /// Transaction `i` of a block, or `null` if there is none.
    pub async fn transaction_at(&self, n: u64, bh: B256, h: &Header, i: usize) -> Result<Value, DataError> {
        if h.transactions_root == alloy_consensus::EMPTY_ROOT_HASH {
            return Ok(Value::Null);
        }
        let raw = self.block_txs(bh, h).await?;
        match raw.get(i) {
            Some(r) => Self::decode_any(r).map(|(tx, from)| Self::tx_json(&tx, from, Some((n, bh, h, i as u32)))),
            None => Ok(Value::Null),
        }
    }

    /// Transaction hashes of an indexed block (for `eth_getBlockBy*`).
    pub fn block_tx_hashes(&self, number: u64, hash: B256) -> Option<Arc<Vec<B256>>> {
        self.inner.read().txs.get(&number).filter(|(h, _)| *h == hash).map(|(_, v)| v.clone())
    }

    /// `eth_getLogs` over `[from, to]` (already resolved; `to` ≤ head). With `at`, the single
    /// block must be that block (by hash), or the query is refused: never another block at the
    /// same height.
    pub async fn logs(&self, filter: &Filter, from: u64, to: u64, at: Option<B256>) -> Result<Value, DataError> {
        if to < from {
            return Err(DataError::Invalid("fromBlock is after toBlock".into()));
        }
        if to - from + 1 > self.max_log_range {
            return Err(DataError::RangeTooLarge(self.max_log_range));
        }
        let scan = self.scan(std::slice::from_ref(filter), from, to, at, false).await?;
        Ok(Value::Array(scan.blocks.into_iter().flat_map(|b| b.logs).collect()))
    }

    /// Changes to `addresses` in each block of `[from, to]`, from the block's verified access list,
    /// change by change with its `block_access_index` (0: system calls before the transactions,
    /// `i + 1`: transaction `i`, the last: after them), not merged into final values. Blocks without
    /// changes to these addresses are left out. BALs come from the gossip cache or eth/71 peers.
    /// `headers` is a hash-linked range the caller captured, so these changes and anything else
    /// it reports for the blocks (logs, its cursor) are from the same chain.
    pub async fn bal_changes(&self, addresses: &[Address], headers: &[(u64, B256, Arc<Header>)]) -> Result<Vec<(u64, B256, Arc<Header>, Vec<Value>)>, DataError> {
        let want: HashSet<Address> = addresses.iter().copied().collect();
        let want = &want;
        let fetched: Vec<_> = futures::stream::iter(headers.iter().cloned())
            .map(|(n, bh, h)| async move {
                let bal = self.block_bal(bh, &h).await?;
                let mut changes = Vec::new();
                for acc in bal.iter().filter(|a| want.contains(&a.address)) {
                    let a = acc.address;
                    for c in acc.balance_changes() {
                        changes.push(json!({"index": c.block_access_index.0, "address": a, "kind": "balance", "value": c.post_balance}));
                    }
                    for c in acc.nonce_changes() {
                        changes.push(json!({"index": c.block_access_index.0, "address": a, "kind": "nonce", "value": c.new_nonce}));
                    }
                    for c in acc.code_changes() {
                        changes.push(json!({"index": c.block_access_index.0, "address": a, "kind": "code", "codeHash": keccak256(&c.new_code), "value": c.new_code}));
                    }
                    for sc in acc.storage_changes() {
                        for c in &sc.changes {
                            changes.push(json!({"index": c.block_access_index.0, "address": a, "kind": "storage", "slot": sc.slot, "value": c.new_value}));
                        }
                    }
                }
                changes.sort_by_key(|c| c["index"].as_u64().unwrap_or(0));
                Ok::<_, DataError>((n, bh, h, changes))
            })
            .buffered(8)
            .collect::<Vec<_>>()
            .await;
        let mut out = Vec::new();
        for r in fetched {
            let (n, bh, h, changes) = r?;
            if !changes.is_empty() {
                out.push((n, bh, h, changes));
            }
        }
        Ok(out)
    }

    /// The headers of `[from, to]` on one hash-linked chain (memory, else the post-fork store).
    pub(crate) async fn linked_headers(&self, from: u64, to: u64) -> Result<Vec<(u64, B256, Arc<Header>)>, DataError> {
        self.ensure_headers(from).await?;
        let disk: HashMap<u64, (B256, Arc<Header>)> = {
            let anchor = self.inner.read().headers.first_key_value().map(|(n, (_, hd))| (*n, hd.parent_hash));
            match (self.history.get(), anchor) {
                (Some(s), Some((mem_low, parent))) if from < mem_low => s
                    .linked_range(from, to.min(mem_low - 1), mem_low, parent)
                    .map_err(|e| DataError::Unavailable(e.to_string()))?
                    .ok_or_else(|| DataError::Unavailable(format!("headers below {mem_low} changed (reorg); retry")))?
                    .into_iter()
                    .map(|(hash, h)| (h.number, (hash, Arc::new(h))))
                    .collect(),
                _ => HashMap::new(),
            }
        };
        let g = self.inner.read();
        let mut out = Vec::new();
        let mut prev: Option<B256> = None;
        for n in from..=to {
            let Some((bh, h)) = g.headers.get(&n).cloned().or_else(|| disk.get(&n).cloned()) else {
                return Err(DataError::Unavailable(format!("no header for block {n}")));
            };
            if prev.is_some_and(|p| p != h.parent_hash) {
                return Err(DataError::Unavailable(format!("chain changed at block {n} during the scan; retry")));
            }
            prev = Some(bh);
            out.push((n, bh, h));
        }
        Ok(out)
    }

    /// The canonical hash at `n`, if skim holds that header: in memory, or on disk hash-linked to
    /// the oldest in-memory header.
    pub fn canonical_hash(&self, n: u64) -> Option<B256> {
        let g = self.inner.read();
        if let Some((h, _)) = g.headers.get(&n) {
            return Some(*h);
        }
        let (mem_low, parent) = g.headers.first_key_value().map(|(n, (_, hd))| (*n, hd.parent_hash))?;
        drop(g);
        if n >= mem_low {
            return None;
        }
        let run = self.history.get()?.linked_range(n, n, mem_low, parent).ok()??;
        run.first().map(|(h, _)| *h)
    }

    /// Logs of `[from, to]` matching any of `filters`, grouped by block (blocks without a match
    /// are left out), plus the hash of `to`: one consistent chain (headers in memory, or on disk
    /// hash-linked to it). With `at`, the single block must be that block. Shared by
    /// `eth_getLogs` and block streams.
    pub async fn scan(&self, filters: &[Filter], from: u64, to: u64, at: Option<B256>, persist: bool) -> Result<Scan, DataError> {
        let headers = self.linked_headers(from, to).await?;
        if let Some(want) = at
            && headers.iter().any(|(_, bh, _)| *bh != want)
        {
            return Err(DataError::Unavailable(format!("block {want} is no longer on the chain")));
        }
        self.scan_headers(filters, &headers, persist).await
    }

    /// [`Self::scan`] over a range of headers the caller captured (hash-linked, ascending), so
    /// that other data for the same blocks (e.g. BAL changes in a stream) comes from the very
    /// same chain. Stored matches are used whenever they cover the filters; `persist` also
    /// stores what this scan fetched (block streams; `eth_getLogs` only reads).
    pub async fn scan_headers(&self, filters: &[Filter], headers: &[(u64, B256, Arc<Header>)], persist: bool) -> Result<Scan, DataError> {
        if headers.is_empty() {
            return Err(DataError::Invalid("empty range".into()));
        }
        // Receipts older than every connected peer serves (history expiry): checked per block
        // below, after what's stored locally is taken from disk.
        let peers_from = match self.peers_serve_from() {
            (Some(p), n) if n > 0 => p,
            _ => 0,
        };
        let (candidates, scanned) = {
            let g = self.inner.read();
            let mut c = Vec::new();
            for (n, bh, h) in headers {
                let touched = g.touched.get(n).filter(|(th, _)| th == bh).map(|(_, t)| t.clone());
                let maybe = filters.iter().any(|f| {
                    f.matches_bloom(h.logs_bloom)
                        && (f.address.is_empty() || touched.as_ref().is_none_or(|t| f.address.iter().any(|a| t.contains(a))))
                });
                if maybe {
                    c.push((*n, *bh, h.clone()));
                }
            }
            (c, headers.len() as u64)
        };
        let mut expired = Vec::new();
        let t = Instant::now();
        let matches = |l: &alloy_primitives::Log| filters.iter().any(|f| f.matches(l));
        let mut blocks = Vec::new();
        let mut count = 0usize;
        // Per chunk of candidates. For a module's stream (`persist`) with `--history local`, each
        // block's stored entry answers if its receipts were already checked against every atom of
        // these filters; otherwise the receipts are fetched once and checked against the union of
        // what was covered before and these atoms, and the merged entry is stored. So a module's
        // filters can grow without losing what earlier scans stored. Receipts are dropped once
        // matched: a long scan holds at most one chunk of them in memory.
        let store = self.history.get().filter(|_| persist);
        let atoms = filter_atoms(filters);
        let atom_keys: Vec<(B256, Vec<u8>)> = atoms.iter().map(|(h, d, _)| (*h, d.clone())).collect();
        // Where stored matches are read from: the store when this scan persists; otherwise too
        // (eth_getLogs), if every atom of these filters has been stored for before.
        let (read, want): (Option<&crate::history::HeaderStore>, Vec<u32>) = match (store, self.history.get()) {
            (Some(s), _) => (Some(s), s.atom_ids(&atom_keys).map_err(|e| DataError::Unavailable(e.to_string()))?),
            (None, Some(s)) => match s.known_atom_ids(&atom_keys).ok().flatten() {
                Some(ids) => (Some(s), ids),
                None => (None, Vec::new()),
            },
            (None, None) => (None, Vec::new()),
        };
        // Atom id -> filter, for the unions (loaded as needed).
        let mut atom_filters: HashMap<u32, Filter> = want.iter().copied().zip(atoms.iter().map(|(_, _, f)| f.clone())).collect();
        // SKIM_SCAN_CHUNK overrides the chunk size (measurement only).
        static CHUNK: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let chunk_size = *CHUNK.get_or_init(|| std::env::var("SKIM_SCAN_CHUNK").ok().and_then(|v| v.parse().ok()).unwrap_or(SCAN_CHUNK));
        for chunk in candidates.chunks(chunk_size) {
            let hashes: Vec<B256> = chunk.iter().map(|(_, bh, _)| *bh).collect();
            let mut entries: HashMap<B256, crate::history::BlockEntry> = read.map(|s| s.get_block_logs(&hashes).unwrap_or_default()).unwrap_or_default();
            let covers = |e: &crate::history::BlockEntry| want.iter().all(|w| e.covered.contains(w));
            let mut need: Vec<(u64, B256, Arc<Header>)> = chunk.iter().filter(|(_, bh, _)| !entries.get(bh).is_some_and(covers)).cloned().collect();
            // Blocks another scan is fetching: wait for it, then read what it stored.
            let mut mine = InflightGuard { map: &self.inflight, hashes: Vec::new() };
            if let Some(s) = store {
                let mut waits = Vec::new();
                {
                    let mut f = self.inflight.lock();
                    for b in std::mem::take(&mut need) {
                        match f.get(&b.1) {
                            Some(n) => waits.push((b, n.clone())),
                            None => {
                                // A closed semaphore is the "done" signal: acquiring it then
                                // returns at once, even for a scan that starts waiting late.
                                f.insert(b.1, Arc::new(tokio::sync::Semaphore::new(0)));
                                // Owned by the guard from here: released even if this scan is
                                // cancelled while waiting for others below.
                                mine.hashes.push(b.1);
                                need.push(b);
                            }
                        }
                    }
                }
                for (b, n) in waits {
                    let _ = tokio::time::timeout(Duration::from_secs(60), n.acquire()).await;
                    match s.get_block_logs(&[b.1]).ok().and_then(|mut m| m.remove(&b.1)) {
                        Some(e) if covers(&e) => {
                            entries.insert(b.1, e);
                        }
                        Some(e) => {
                            entries.insert(b.1, e);
                            need.push(b);
                        }
                        None => need.push(b), // that fetch failed: fetch it here
                    }
                }
            }
            let _done = mine;
            // Older than any connected peer serves, and not covered locally: a stream reports these
            // as a gap; eth_getLogs refuses (what's stored was used above either way).
            if need.iter().any(|(n, _, _)| *n < peers_from) {
                if !persist {
                    return Err(DataError::Unavailable(format!("block {} is older than connected peers serve (earliest {peers_from}); receipts before it have expired", need[0].0)));
                }
                expired.extend(need.iter().filter(|(n, _, _)| *n < peers_from).map(|(n, _, _)| *n));
                need.retain(|(n, _, _)| *n >= peers_from);
            }
            // What each fetched block is checked against: these atoms plus what it covered before.
            let mut unions: HashMap<B256, Vec<u32>> = HashMap::new();
            if let Some(s) = store {
                let mut extra: Vec<u32> = Vec::new();
                for (_, bh, _) in &need {
                    let mut u: Vec<u32> = entries.get(bh).map(|e| e.covered.clone()).unwrap_or_default();
                    u.extend(&want);
                    u.sort_unstable();
                    u.dedup();
                    extra.extend(u.iter().filter(|id| !atom_filters.contains_key(id)));
                    unions.insert(*bh, u);
                }
                extra.sort_unstable();
                extra.dedup();
                for (id, def) in s.atom_defs(&extra).unwrap_or_default() {
                    if let Some(f) = atom_from_def(&def) {
                        atom_filters.insert(id, f);
                    }
                }
            }
            let batch = if need.len() > 4 {
                self.receipts_many(&need.iter().map(|(_, bh, h)| (*bh, h.clone())).collect::<Vec<_>>()).await
            } else {
                HashMap::new()
            };
            let (batch, unions, atom_filters) = (&batch, &unions, &atom_filters);
            let fetched: Vec<_> = futures::stream::iter(need.iter().cloned())
                .map(|(n, bh, h)| async move {
                    let r = match batch.get(&bh) {
                        Some(r) => r.clone(),
                        None => self.block_receipts(bh, &h).await?,
                    };
                    // Logs matching the union (for storage) or, without a store, the query; the
                    // receipts themselves are dropped here.
                    let union: Vec<&Filter> = unions.get(&bh).map(|u| u.iter().filter_map(|id| atom_filters.get(id)).collect()).unwrap_or_default();
                    let keep = |l: &alloy_primitives::Log| if union.is_empty() { matches(l) } else { union.iter().any(|f| f.matches(l)) };
                    let mut found = Vec::new();
                    let mut li = 0u32;
                    for (ti, rc) in r.iter().enumerate() {
                        for l in &rc.logs {
                            if keep(l) {
                                found.push((ti as u32, li, l.clone()));
                            }
                            li += 1;
                        }
                    }
                    drop(r);
                    let txs = if found.is_empty() { None } else { Some(self.scan_tx_hashes(n, bh, &h).await?) };
                    let logs = found
                        .into_iter()
                        .map(|(ti, li, log)| crate::history::MatchedLog { tx_index: ti, log_index: li, tx_hash: txs.as_ref().map_or(B256::ZERO, |t| t[ti as usize]), log })
                        .collect::<Vec<_>>();
                    Ok::<_, DataError>((bh, logs))
                })
                .buffered(8)
                .collect()
                .await;
            let mut to_store: Vec<B256> = Vec::new();
            for res in fetched {
                let (bh, logs) = res?;
                let covered = unions.get(&bh).cloned().unwrap_or_default();
                entries.insert(bh, crate::history::BlockEntry { covered, logs });
                to_store.push(bh);
            }
            for (n, bh, h) in chunk {
                let Some(e) = entries.get(bh) else { continue };
                let logs: Vec<Value> = e
                    .logs
                    .iter()
                    .filter(|x| matches(&x.log))
                    .map(|x| Self::log_json(&x.log, *bh, *n, h.timestamp, x.tx_hash, x.tx_index, x.log_index as u64))
                    .collect();
                if logs.is_empty() {
                    continue;
                }
                count += logs.len();
                blocks.push(ScanBlock { number: *n, hash: *bh, parent_hash: h.parent_hash, timestamp: h.timestamp, logs });
            }
            if let Some(s) = store {
                let stored: Vec<(B256, &crate::history::BlockEntry)> = to_store.iter().filter_map(|h| entries.get(h).map(|e| (*h, e))).collect();
                if let Err(e) = s.put_block_logs(&stored) {
                    tracing::warn!("{e}");
                }
            }
        }
        tracing::debug!(scanned, candidates = candidates.len(), logs = count, ms = t.elapsed().as_millis() as u64, "log scan");
        Ok(Scan { blocks, expired })
    }

    /// Block range of a filter, with tags resolved against `head`.
    pub fn range(filter: &Filter, head: u64) -> Result<(u64, u64), DataError> {
        use alloy_rpc_types_eth::BlockNumberOrTag as T;
        let tag = |t: Option<&T>| -> Result<u64, DataError> {
            match t {
                None | Some(T::Latest) | Some(T::Pending) => Ok(head),
                Some(t @ (T::Safe | T::Finalized)) => Err(DataError::Unavailable(format!("block tag {t} is not tracked yet"))),
                Some(T::Number(n)) => Ok((*n).min(head)),
                Some(T::Earliest) => Ok(0),
            }
        };
        match &filter.block_option {
            FilterBlockOption::Range { from_block, to_block } => Ok((tag(from_block.as_ref())?, tag(to_block.as_ref())?)),
            FilterBlockOption::AtBlockHash(_) => Err(DataError::Invalid("blockHash filters are resolved by the caller".into())),
        }
    }

    // ---- serving other peers ---------------------------------------------------------------

    /// A canonical header held in memory.
    pub fn cached_header(&self, id: skim_el::serve::BlockHashOrNumber) -> Option<Header> {
        let g = self.inner.read();
        let n = match id {
            skim_el::serve::BlockHashOrNumber::Number(n) => n,
            skim_el::serve::BlockHashOrNumber::Hash(h) => *g.by_hash.get(&h)?,
        };
        g.headers.get(&n).map(|(_, h)| (**h).clone())
    }

    /// A block body held in memory (transactions and withdrawals both verified).
    pub fn cached_body(&self, hash: B256) -> Option<skim_el::BlockBody> {
        let w = self.withdrawals.lock().get(&hash)?;
        let raw = self.bodies.lock().get(&hash)?;
        let transactions = raw.iter().map(|t| SignedTx::decode_2718(&mut t.as_ref()).ok()).collect::<Option<Vec<_>>>()?;
        Some(skim_el::BlockBody { transactions, ommers: vec![], withdrawals: Some(Withdrawals::new((*w).clone())) })
    }

    pub fn cached_receipts(&self, hash: B256) -> Option<Vec<Receipt>> {
        if let Some(r) = self.receipts.lock().get(&hash) {
            return Some((*r).clone());
        }
        let n = self.number_of(&hash)?;
        let empty = self.inner.read().headers.get(&n).is_some_and(|(_, h)| h.receipts_root == alloy_consensus::EMPTY_ROOT_HASH);
        empty.then(Vec::new)
    }

    pub fn cached_bal(&self, hash: B256) -> Option<Bytes> {
        self.bal_raw.lock().get(&hash)
    }

    /// Oldest block from which header, body and BAL are all held, without gaps, up to the newest.
    pub fn served_from(&self) -> Option<u64> {
        let hashes: Vec<(u64, B256)> = self.inner.read().headers.iter().rev().take(SERVE_BALS + 1).map(|(n, (h, _))| (*n, *h)).collect();
        let (bodies, wd, bals) = (self.bodies.lock(), self.withdrawals.lock(), self.bal_raw.lock());
        let mut from = None;
        for (i, (n, h)) in hashes.iter().enumerate() {
            if i > 0 && hashes[i - 1].0 != n + 1 || !(bodies.map.contains_key(h) && wd.map.contains_key(h) && bals.map.contains_key(h)) {
                break;
            }
            from = Some(*n);
        }
        from
    }

    pub fn number_of(&self, hash: &B256) -> Option<u64> {
        self.inner.read().by_hash.get(hash).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_wait_keeps_its_cpu_slot_until_the_work_ends() {
        // Cap 1. The first job runs and blocks; its async waiter is cancelled. A second job must
        // not start until the first one actually finishes.
        let pool = Arc::new(tokio::sync::Semaphore::new(1));
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let p1 = pool.clone();
        let first = tokio::spawn(async move { cpu_on(&p1, move || wait.recv().ok()).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        first.abort();
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (p2, s2) = (pool.clone(), started.clone());
        let second = tokio::spawn(async move { cpu_on(&p2, move || s2.store(true, std::sync::atomic::Ordering::SeqCst)).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!started.load(std::sync::atomic::Ordering::SeqCst), "second job started while the first still ran");
        release.send(()).unwrap();
        second.await.unwrap();
        assert!(started.load(std::sync::atomic::Ordering::SeqCst));
    }
}
