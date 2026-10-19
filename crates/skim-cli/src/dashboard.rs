//! Usage and latency history behind `skim_dashboard` and the Prometheus metrics: which contract
//! addresses requests read (pinned by a subscription or not), RPC latency per method, and
//! block arrival.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Instant,
};

use alloy_primitives::{Address, U256};
use parking_lot::Mutex;
use serde_json::{Value, json};
use skim_bal::StateKey;
use skim_sim::db::Access;
use skim_state::Store;


/// Distinct slots remembered per address (counts keep going past this).
const SLOTS_PER_ADDRESS: usize = 4096;
/// Addresses tracked; beyond this, the least-called ones are dropped.
const MAX_ADDRESSES: usize = 5000;
const BLOCKS_KEPT: usize = 120;
const LATENCIES_KEPT: usize = 512;

#[derive(Default)]
struct AddrUsage {
    /// Calls whose execution read this address (cache hits included).
    calls: u64,
    slots: HashSet<U256>,
    slot_reads: u64,
    /// Keys fetched from the network for it (accounts and slots).
    fetched: u64,
    last_block: u64,
}

#[derive(Default)]
struct MethodStat {
    count: u64,
    errors: u64,
    recent_ms: VecDeque<f64>,
}

#[derive(Clone, Copy)]
pub struct BlockStat {
    pub number: u64,
    pub arrival_ms: f64,
    pub changed: usize,
}

pub struct Dashboard {
    started: Instant,
    usage: Mutex<HashMap<Address, AddrUsage>>,
    methods: Mutex<HashMap<String, MethodStat>>,
    blocks: Mutex<VecDeque<BlockStat>>,
}

impl Default for Dashboard {
    fn default() -> Self {
        Self { started: Instant::now(), usage: Default::default(), methods: Default::default(), blocks: Default::default() }
    }
}

impl Dashboard {
    /// One executed call: every address it read, and what had to be fetched.
    pub fn record_call(&self, block: u64, access: &Access, fetched_accounts: &[Address], fetched_storage: &[(Address, U256)]) {
        let mut u = self.usage.lock();
        let mut addrs: HashSet<Address> = access.accounts.iter().copied().filter(|a| !is_system(a)).collect();
        addrs.extend(access.storage.iter().map(|(a, _)| *a));
        for a in &addrs {
            let e = u.entry(*a).or_default();
            e.calls += 1;
            e.last_block = block;
        }
        for (a, s) in &access.storage {
            let e = u.entry(*a).or_default();
            e.slot_reads += 1;
            if e.slots.len() < SLOTS_PER_ADDRESS {
                e.slots.insert(*s);
            }
        }
        for a in fetched_accounts.iter().filter(|a| !is_system(a)) {
            u.entry(*a).or_default().fetched += 1;
        }
        for (a, _) in fetched_storage {
            u.entry(*a).or_default().fetched += 1;
        }
        prune(&mut u);
    }

    /// A call answered from the result cache: the same addresses were read again.
    pub fn record_hit(&self, block: u64, addrs: &[Address]) {
        let mut u = self.usage.lock();
        for a in addrs.iter().filter(|a| !is_system(a)) {
            let e = u.entry(*a).or_default();
            e.calls += 1;
            e.last_block = block;
        }
        prune(&mut u);
    }

    /// Addresses and methods tracked (for memory attribution).
    pub fn sizes(&self) -> (usize, usize) {
        (self.usage.lock().len(), self.methods.lock().len())
    }

    pub fn record_method(&self, method: &str, ms: f64, ok: bool) {
        let mut m = self.methods.lock();
        let e = m.entry(method.to_string()).or_default();
        e.count += 1;
        e.errors += !ok as u64;
        e.recent_ms.push_back(ms);
        if e.recent_ms.len() > LATENCIES_KEPT {
            e.recent_ms.pop_front();
        }
    }

    pub fn record_block(&self, b: BlockStat) {
        let mut q = self.blocks.lock();
        q.push_back(b);
        if q.len() > BLOCKS_KEPT {
            q.pop_front();
        }
    }

    pub fn uptime_s(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// Addresses by calls, with how much of what was read is pinned by a subscription.
    /// `labels`: subscription names by address.
    pub fn addresses_json(&self, store: &Store, labels: &HashMap<Address, String>) -> Vec<Value> {
        let u = self.usage.lock();
        let mut rows: Vec<(&Address, &AddrUsage)> = u.iter().collect();
        rows.sort_by(|a, b| b.1.calls.cmp(&a.1.calls).then(b.1.fetched.cmp(&a.1.fetched)));
        rows.into_iter()
            .take(200)
            .map(|(a, e)| {
                let account_pinned = store.is_subscribed(&StateKey::Balance(*a));
                let pinned_slots = e.slots.iter().filter(|s| store.is_subscribed(&StateKey::Storage(*a, **s))).count();
                let unpinned = e.slots.len() - pinned_slots;
                let status = match (account_pinned || pinned_slots > 0, unpinned) {
                    (true, 0) => "pinned",
                    (true, _) => "partly pinned",
                    (false, _) => "on demand",
                };
                json!({
                    "address": a.to_checksum(None),
                    "label": labels.get(a).cloned().unwrap_or_default(),
                    "status": status,
                    "calls": e.calls,
                    "slots": e.slots.len(),
                    "unpinned_slots": unpinned,
                    "slot_reads": e.slot_reads,
                    "fetched": e.fetched,
                    "last_block": e.last_block,
                })
            })
            .collect()
    }

    /// A `skim.yaml` subscription block for the most-used addresses that aren't fully pinned.
    pub fn suggestion(&self, store: &Store) -> String {
        let u = self.usage.lock();
        let mut rows: Vec<(&Address, &AddrUsage)> = u
            .iter()
            .filter(|(a, e)| {
                e.calls >= 3
                    && (!store.is_subscribed(&StateKey::Balance(**a))
                        || e.slots.iter().any(|s| !store.is_subscribed(&StateKey::Storage(**a, *s))))
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.1.calls));
        let mut y = String::from("state:\n  subscriptions:\n");
        for (a, e) in rows.iter().take(10) {
            let mut slots: Vec<U256> = e.slots.iter().filter(|s| !store.is_subscribed(&StateKey::Storage(**a, **s))).copied().collect();
            slots.sort();
            y.push_str(&format!("    - name: used-{}\n      address: \"{}\"\n      account: true\n", a.to_string()[2..8].to_lowercase(), a.to_checksum(None)));
            if slots.len() > 64 {
                y.push_str(&format!("      storage: all                 # {} distinct slots read\n", slots.len()));
            } else if !slots.is_empty() {
                let list: Vec<String> = slots.iter().map(|s| format!("\"{s:#x}\"")).collect();
                y.push_str(&format!("      storage: [{}]\n", list.join(", ")));
            }
        }
        if rows.is_empty() { String::new() } else { y }
    }

    pub fn methods_json(&self) -> Vec<Value> {
        let m = self.methods.lock();
        let mut rows: Vec<Value> = m
            .iter()
            .map(|(k, e)| {
                let mut v: Vec<f64> = e.recent_ms.iter().copied().collect();
                v.sort_by(f64::total_cmp);
                let q = |p: f64| v.get(((v.len() as f64 - 1.0) * p).round() as usize).copied().unwrap_or(0.0);
                json!({"method": k, "count": e.count, "errors": e.errors, "p50_ms": round2(q(0.5)), "p95_ms": round2(q(0.95)), "max_ms": round2(q(1.0))})
            })
            .collect();
        rows.sort_by(|a, b| b["count"].as_u64().cmp(&a["count"].as_u64()));
        rows
    }

    pub fn blocks(&self) -> Vec<BlockStat> {
        self.blocks.lock().iter().copied().collect()
    }
}

/// The zero address (the default caller) and precompiles: read by most calls, not "used contracts".
fn is_system(a: &Address) -> bool {
    a.0[..18].iter().all(|b| *b == 0) && u16::from_be_bytes([a.0[18], a.0[19]]) <= 0x100
}

pub fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Client family from an EL client string or a CL agent ("Geth/v1.17…" → "Geth").
pub fn client_kind(s: &str) -> String {
    let k = s.split('/').next().unwrap_or("").trim();
    if k.is_empty() { "unknown".into() } else { k.to_string() }
}

/// Counts by client kind, largest first.
pub fn by_kind<'a>(kinds: impl Iterator<Item = &'a str>) -> Vec<Value> {
    let mut m: HashMap<String, u64> = HashMap::new();
    for k in kinds {
        *m.entry(client_kind(k)).or_default() += 1;
    }
    let mut v: Vec<(String, u64)> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v.into_iter().map(|(k, n)| json!({"client": k, "count": n})).collect()
}

/// Keeps the per-address table bounded: past `MAX_ADDRESSES`, the least-called tenth goes.
fn prune(u: &mut HashMap<Address, AddrUsage>) {
    if u.len() > MAX_ADDRESSES {
        let mut calls: Vec<u64> = u.values().map(|e| e.calls).collect();
        calls.sort_unstable();
        let cut = calls[calls.len() - MAX_ADDRESSES * 9 / 10];
        u.retain(|_, e| e.calls > cut);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_counts_calls_slots_and_fetches() {
        let d = Dashboard::default();
        let (a, b) = (Address::repeat_byte(1), Address::repeat_byte(2));
        let access = Access { accounts: vec![a], storage: vec![(a, U256::from(1)), (a, U256::from(2)), (b, U256::from(7))], ..Default::default() };
        d.record_call(10, &access, &[], &[(b, U256::from(7))]);
        d.record_hit(11, &[a, b]);
        let u = d.usage.lock();
        assert_eq!((u[&a].calls, u[&a].slots.len(), u[&a].fetched, u[&a].last_block), (2, 2, 0, 11));
        assert_eq!((u[&b].calls, u[&b].slots.len(), u[&b].fetched), (2, 1, 1));
    }

    #[test]
    fn cache_hits_on_many_addresses_stay_bounded() {
        let d = Dashboard::default();
        for i in 0..(MAX_ADDRESSES as u64 * 2) {
            let mut raw = [0u8; 20];
            raw[..8].copy_from_slice(&(i + 1).to_be_bytes());
            d.record_hit(1, &[Address::from(raw)]);
        }
        assert!(d.usage.lock().len() <= MAX_ADDRESSES);
    }

    #[test]
    fn method_percentiles_and_kinds() {
        let d = Dashboard::default();
        for i in 1..=100 {
            d.record_method("eth_call", i as f64, i % 10 != 0);
        }
        let m = &d.methods_json()[0];
        assert_eq!((m["count"].as_u64(), m["errors"].as_u64()), (Some(100), Some(10)));
        assert_eq!(m["p50_ms"].as_f64(), Some(51.0));
        assert_eq!(m["max_ms"].as_f64(), Some(100.0));
        let k = by_kind(["Geth/v1", "Geth/v2", "Nethermind/x", ""].into_iter());
        assert_eq!(k[0], json!({"client": "Geth", "count": 2}));
        assert_eq!(k.len(), 3);
    }
}
