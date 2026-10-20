//! `skim run --mempool-probe` (Sepolia measurement): how big is the validation state of the
//! public mempool, and how fast does it churn? The question behind a light mempool node
//! (VOPS-style validation state; docs/guide/future-considerations.md).
//!
//! Pending transactions come from peers' announcements (fetched with `GetPooledTransactions`)
//! and broadcasts. For each sender, the keys that decide includability (nonce, balance, code
//! hash: an EIP-7702 delegation) are read once, verified, then kept current from each block's
//! BAL without fetching. Each block: inclusions, transactions overtaken by their sender's nonce,
//! underfunded ones. A transaction expires after 30 minutes; an account is released with its last
//! pending transaction. Once a minute, one log line with the totals.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_primitives::{Address, B256, U256};
use parking_lot::Mutex;
use skim_bal::StateKey;
use skim_el::{ElNet, MempoolEvent, PendingTx, Peer};
use skim_sim::Simulator;

const EXPIRE: Duration = Duration::from_secs(30 * 60);
const FETCH_BATCH: usize = 256;

struct Entry {
    tx: PendingTx,
    first_seen: Instant,
    underfunded: bool,
}

#[derive(Default)]
struct Account {
    nonce: Option<u64>,
    balance: Option<U256>,
    delegated: bool,
    pending: usize,
}

#[derive(Default)]
struct Stats {
    announced: u64,
    tx_requests: u64,
    txs_received: u64,
    account_reads: u64,
    account_read_ms: Vec<f64>,
    read_failures: u64,
    bal_updates: u64,
    blocks: u64,
    included: u64,
    inclusion_s: Vec<f64>,
    overtaken: u64,
    underfunded: u64,
    expired: u64,
    stale_on_arrival: u64,
}

#[derive(Default)]
struct State {
    pending: HashMap<B256, Entry>,
    accounts: HashMap<Address, Account>,
    requested: HashSet<B256>,
    stats: Stats,
}

pub struct MempoolProbe {
    st: Mutex<State>,
    net: ElNet,
    sim: Arc<Simulator>,
}

impl MempoolProbe {
    pub fn start(net: ElNet, sim: Arc<Simulator>, mut rx: tokio::sync::mpsc::Receiver<MempoolEvent>) -> Arc<Self> {
        let p = Arc::new(Self { st: Default::default(), net, sim });
        let me = p.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                match ev {
                    MempoolEvent::Announced { peer, hashes } => me.announced(peer, hashes),
                    MempoolEvent::Transactions { txs, .. } => {
                        me.st.lock().stats.txs_received += txs.len() as u64;
                        for t in txs {
                            me.add(t);
                        }
                    }
                }
            }
        });
        let me = p.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                me.report();
            }
        });
        p
    }

    fn announced(self: &Arc<Self>, peer: Peer, hashes: Vec<B256>) {
        let new: Vec<B256> = {
            let mut st = self.st.lock();
            st.stats.announced += hashes.len() as u64;
            if st.requested.len() > 500_000 {
                st.requested.clear(); // bounded; a re-request is harmless
            }
            hashes.into_iter().filter(|h| !st.pending.contains_key(h) && st.requested.insert(*h)).collect()
        };
        for chunk in new.chunks(FETCH_BATCH) {
            let (me, chunk) = (self.clone(), chunk.to_vec());
            tokio::spawn(async move {
                me.st.lock().stats.tx_requests += 1;
                if let (Ok(txs), _) = me.net.pooled_transactions(peer, chunk).await {
                    me.st.lock().stats.txs_received += txs.len() as u64;
                    for t in txs {
                        me.add(t);
                    }
                }
            });
        }
    }

    fn add(self: &Arc<Self>, tx: PendingTx) {
        let fetch = {
            let mut st = self.st.lock();
            if st.pending.contains_key(&tx.hash) {
                return;
            }
            let a = st.accounts.entry(tx.sender).or_default();
            a.pending += 1;
            let fetch = a.pending == 1 && a.nonce.is_none();
            let stale = a.nonce.is_some_and(|n| tx.nonce < n);
            let sender = tx.sender;
            st.pending.insert(tx.hash, Entry { tx, first_seen: Instant::now(), underfunded: false });
            if stale {
                st.stats.stale_on_arrival += 1;
            }
            fetch.then_some(sender)
        };
        if let Some(a) = fetch {
            let me = self.clone();
            tokio::spawn(async move { me.read_account(a).await });
        }
    }

    /// The sender's validation keys at the head: on demand, verified (SNAP with proofs or a BAL).
    async fn read_account(&self, a: Address) {
        let Some(head) = self.sim.store.head() else { return };
        let t = Instant::now();
        let (n, b, c) = tokio::join!(
            self.sim.read(&head, StateKey::Nonce(a)),
            self.sim.read(&head, StateKey::Balance(a)),
            self.sim.read(&head, StateKey::CodeHash(a)),
        );
        let ms = t.elapsed().as_secs_f64() * 1e3;
        let mut st = self.st.lock();
        st.stats.account_reads += 1;
        match (n, b, c) {
            (Ok(n), Ok(b), Ok(c)) => {
                st.stats.account_read_ms.push(ms);
                if let Some(acc) = st.accounts.get_mut(&a) {
                    acc.nonce.get_or_insert(n.saturating_to());
                    acc.balance.get_or_insert(b);
                    // An EOA's code hash is the empty-code hash; anything else here is a 7702 delegation.
                    acc.delegated = B256::from(c) != alloy_primitives::KECCAK256_EMPTY && c != U256::ZERO;
                }
            }
            _ => st.stats.read_failures += 1,
        }
    }

    /// A block skim applied: its transactions' hashes and its BAL's final writes.
    pub fn on_block(&self, tx_hashes: &[B256], writes: &[(StateKey, U256)]) {
        let mut st = self.st.lock();
        st.stats.blocks += 1;
        let now = Instant::now();
        for h in tx_hashes {
            if let Some(e) = st.pending.remove(h) {
                st.stats.included += 1;
                st.stats.inclusion_s.push(now.duration_since(e.first_seen).as_secs_f64());
                if let Some(a) = st.accounts.get_mut(&e.tx.sender) {
                    a.pending -= 1;
                }
            }
        }
        let mut touched = HashSet::new();
        for (k, v) in writes {
            let (a, field) = match k {
                StateKey::Nonce(a) => (a, 0),
                StateKey::Balance(a) => (a, 1),
                StateKey::CodeHash(a) => (a, 2),
                StateKey::Storage(..) => continue,
            };
            let Some(acc) = st.accounts.get_mut(a) else { continue };
            match field {
                0 => acc.nonce = Some(v.saturating_to()),
                1 => acc.balance = Some(*v),
                _ => acc.delegated = B256::from(*v) != alloy_primitives::KECCAK256_EMPTY && *v != U256::ZERO,
            }
            touched.insert(*a);
        }
        st.stats.bal_updates += touched.len() as u64;
        // Re-check the touched senders' pending transactions; expire old ones.
        let mut gone = Vec::new();
        let mut newly_underfunded = 0;
        let State { pending, accounts, .. } = &mut *st;
        for (h, e) in pending.iter_mut() {
            if now.duration_since(e.first_seen) > EXPIRE {
                gone.push((*h, false));
                continue;
            }
            if !touched.contains(&e.tx.sender) {
                continue;
            }
            let acc = &accounts[&e.tx.sender];
            if acc.nonce.is_some_and(|n| e.tx.nonce < n) {
                gone.push((*h, true));
            } else if !e.underfunded && acc.balance.is_some_and(|b| b < e.tx.max_cost) {
                e.underfunded = true;
                newly_underfunded += 1;
            }
        }
        st.stats.underfunded += newly_underfunded;
        for (h, overtaken) in gone {
            if let Some(e) = st.pending.remove(&h) {
                if overtaken {
                    st.stats.overtaken += 1;
                } else {
                    st.stats.expired += 1;
                }
                if let Some(a) = st.accounts.get_mut(&e.tx.sender) {
                    a.pending -= 1;
                }
            }
        }
        st.accounts.retain(|_, a| a.pending > 0);
    }

    fn report(&self) {
        let mut st = self.st.lock();
        let pct = |v: &mut Vec<f64>, p: f64| {
            if v.is_empty() {
                return f64::NAN;
            }
            v.sort_by(f64::total_cmp);
            v[((p / 100.0) * (v.len() - 1) as f64) as usize]
        };
        let (pending, accounts) = (st.pending.len(), st.accounts.len());
        let delegated = st.accounts.values().filter(|a| a.delegated).count();
        let known = st.accounts.values().filter(|a| a.nonce.is_some()).count();
        // Per account: address, nonce, balance, a flag (~61 bytes of state); per transaction: what
        // a validity check needs (hash, sender, nonce, cost; ~92 bytes).
        let state_bytes = accounts * 61;
        let s = &mut st.stats;
        let (r50, r95) = (pct(&mut s.account_read_ms, 50.0), pct(&mut s.account_read_ms, 95.0));
        let (i50, i95) = (pct(&mut s.inclusion_s, 50.0), pct(&mut s.inclusion_s, 95.0));
        tracing::info!(
            pending, accounts, known, delegated, state_bytes, tx_bytes = pending * 92,
            announced = s.announced, tx_requests = s.tx_requests, txs_received = s.txs_received,
            account_reads = s.account_reads, read_failures = s.read_failures, read_ms_p50 = r50, read_ms_p95 = r95,
            blocks = s.blocks, bal_updates = s.bal_updates, included = s.included, inclusion_s_p50 = i50, inclusion_s_p95 = i95,
            overtaken = s.overtaken, underfunded = s.underfunded, expired = s.expired, stale_on_arrival = s.stale_on_arrival,
            "mempool probe"
        );
        // Keep the latency samples bounded.
        for v in [&mut s.account_read_ms, &mut s.inclusion_s] {
            if v.len() > 100_000 {
                v.drain(..50_000);
            }
        }
    }
}
