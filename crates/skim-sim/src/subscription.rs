//! Subscriptions (PROJECT.md §7.5): the state an application declares is kept current and
//! **ready**; everything else is on demand.
//!
//! - **Account:** balance, nonce and code hash.
//! - **Storage list:** explicit slots.
//! - **Learned:** whatever the calls in `calls` read (their access sets, re-learned every block),
//!   also used by protocol predictors.
//! - **Simulation:** run fixed call inputs every block. Their access sets stay subscribed, so the
//!   calls stay warm; with `on_block` the latest results are kept for readers.
//!
//! **Ready** means the head's BAL is applied and every required key of the subscription is known
//! at the head. Readiness is reported per subscription and never assumed.

use std::collections::{BTreeMap, HashSet};

use alloy_primitives::{Address, Bytes, U256};
use parking_lot::RwLock;
use revm::{context::TxEnv, primitives::TxKind};
use serde::{Deserialize, Serialize};
use skim_bal::StateKey;
use skim_state::{BlockRef, Retention, StateValue};

use crate::{Mode, Simulator, prefetch};

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum StorageSpec {
    Slots(Vec<U256>),
    /// `"all"`: the whole storage trie, downloaded over SNAP and verified against the account's
    /// storage root, then kept current from every block's BAL.
    All(String),
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct CallSpec {
    pub to: Address,
    pub data: Bytes,
    #[serde(default)]
    pub from: Option<Address>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SubscriptionSpec {
    pub name: String,
    #[serde(default)]
    pub address: Option<Address>,
    #[serde(default)]
    pub account: bool,
    #[serde(default)]
    pub storage: Option<StorageSpec>,
    /// Simulation inputs whose access sets are learned and kept warm.
    #[serde(default)]
    pub calls: Vec<CallSpec>,
    /// Re-run `calls` every block and keep the latest results.
    #[serde(default)]
    pub on_block: bool,
    /// `{ mode: archive }` keeps every version of this subscription's keys on disk (M4).
    #[serde(default)]
    pub retention: Option<RetentionSpec>,
    /// `storage: all`: refuse contracts with more slots than this (default 50,000).
    #[serde(default)]
    pub max_slots: Option<Limit>,
}

/// A count limit in config: a number, or `unlimited`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit(pub usize);

impl Limit {
    pub const UNLIMITED: Limit = Limit(usize::MAX);
}

impl<'de> Deserialize<'de> for Limit {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            N(usize),
            S(String),
        }
        match Raw::deserialize(d)? {
            Raw::N(n) => Ok(Limit(n)),
            Raw::S(s) if s == "unlimited" => Ok(Limit::UNLIMITED),
            Raw::S(s) => s.parse().map(Limit).map_err(|_| serde::de::Error::custom(format!("expected a number or \"unlimited\", got {s:?}"))),
        }
    }
}

impl Serialize for Limit {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.0 == usize::MAX { s.serialize_str("unlimited") } else { s.serialize_u64(self.0 as u64) }
    }
}

pub const DEFAULT_MAX_SLOTS: usize = 50_000;

impl SubscriptionSpec {
    fn storage_all(&self) -> bool {
        matches!(&self.storage, Some(StorageSpec::All(_)))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RetentionSpec {
    pub mode: String,
}

impl SubscriptionSpec {
    pub fn archive(&self) -> bool {
        self.retention.as_ref().is_some_and(|r| r.mode == "archive")
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Status {
    pub name: String,
    pub ready: bool,
    /// Block the status refers to.
    pub block: Option<u64>,
    pub required_keys: usize,
    pub missing_keys: usize,
    pub truncated: bool,
    /// Every call of the subscription ran without error and without unresolved misses at `block`.
    pub calls_ok: bool,
    pub error: Option<String>,
    /// Latest results of `calls` (hex output, or an error), when `on_block` is set.
    pub results: Vec<String>,
}

pub struct Subscriptions {
    specs: Vec<SubscriptionSpec>,
    max_keys: usize,
    /// Keys each subscription declares explicitly (account / storage).
    declared: BTreeMap<String, HashSet<StateKey>>,
    /// Required keys per subscription (declared + admitted learned keys).
    required: RwLock<BTreeMap<String, HashSet<StateKey>>>,
    status: RwLock<BTreeMap<String, Status>>,
    /// `storage: all` subscriptions that verifiably exceed a limit (grew past `max_slots`, or
    /// the store dropped them at `max_subscribed_keys`): not downloaded again; the error stays
    /// until restart. Only verified facts land here.
    too_large: RwLock<BTreeMap<String, String>>,
    /// `storage: all` downloads to retry later: (block to retry at, attempts so far). Set when
    /// peers only gave unverified oversized answers, or the budget refused the install.
    retry_at: RwLock<BTreeMap<String, (u64, u32)>>,
}

/// Backoff for `storage: all` retries: 32 blocks, doubling, at most ~7 hours of blocks.
fn retry_after(attempts: u32) -> u64 {
    (32u64 << attempts.min(6)).min(2048)
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("subscription {0}: storage must be a list of slots or \"all\"")]
    StorageAll(String),
    #[error("subscription {0}: storage \"all\" can't be archived yet (the archive is keyed by slot number)")]
    StorageAllArchive(String),
    #[error("subscription {0}: storage/account needs an address")]
    NoAddress(String),
    #[error("{0} declared keys exceed limits.max_subscribed_keys = {1}")]
    TooManyKeys(usize, usize),
}

/// Code hashes among `keys` (known, non-empty) whose bytecode skim doesn't hold.
fn missing_code(sim: &Simulator, snap: &skim_state::Snapshot, keys: &[StateKey]) -> Vec<alloy_primitives::B256> {
    let mut out: Vec<alloy_primitives::B256> = keys
        .iter()
        .filter(|k| matches!(k, StateKey::CodeHash(_)))
        .filter_map(|k| match snap.get(k) {
            Ok(StateValue::Known(h)) => Some(alloy_primitives::B256::from(h)),
            _ => None,
        })
        .filter(|h| *h != revm::primitives::KECCAK_EMPTY && !h.is_zero() && sim.store.code(h).is_none())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Readiness: the head is current, every required key is known, every call ran cleanly at
/// this block, and the key limit cut nothing. An empty key set from a failed run is not ready.
fn is_ready(head_ok: bool, missing: usize, calls_ok: bool, truncated: bool) -> bool {
    head_ok && missing == 0 && calls_ok && !truncated
}

use crate::account_keys;

impl Subscriptions {
    pub fn new(specs: Vec<SubscriptionSpec>, max_keys: usize) -> Result<Self, ConfigError> {
        let mut required = BTreeMap::new();
        for s in &specs {
            let mut keys = HashSet::new();
            if (s.account || s.storage.is_some()) && s.address.is_none() {
                return Err(ConfigError::NoAddress(s.name.clone()));
            }
            if let Some(a) = s.address {
                if s.account {
                    keys.extend(account_keys(a));
                }
                match &s.storage {
                    Some(StorageSpec::All(v)) if v != "all" => return Err(ConfigError::StorageAll(s.name.clone())),
                    Some(StorageSpec::All(_)) if s.archive() => return Err(ConfigError::StorageAllArchive(s.name.clone())),
                    Some(StorageSpec::All(_)) => keys.extend(account_keys(a)), // code and balance too
                    Some(StorageSpec::Slots(v)) => keys.extend(v.iter().map(|slot| StateKey::Storage(a, *slot))),
                    None => {}
                }
            }
            required.insert(s.name.clone(), keys);
        }
        let total: usize = required.values().map(HashSet::len).sum();
        if total > max_keys {
            return Err(ConfigError::TooManyKeys(total, max_keys));
        }
        let status = specs.iter().map(|s| (s.name.clone(), Status { name: s.name.clone(), ..Default::default() })).collect();
        Ok(Self {
            specs,
            max_keys,
            declared: required.clone(),
            required: RwLock::new(required),
            status: RwLock::new(status),
            too_large: Default::default(),
            retry_at: Default::default(),
        })
    }

    pub fn total_keys(&self) -> usize {
        self.required.read().values().map(HashSet::len).sum()
    }

    /// Subscription name per address it pins (declared addresses first, then addresses of
    /// keys learned from its calls).
    pub fn labels(&self) -> std::collections::HashMap<Address, String> {
        let mut m = std::collections::HashMap::new();
        for sp in &self.specs {
            if let Some(a) = sp.address {
                m.entry(a).or_insert_with(|| sp.name.clone());
            }
        }
        for (name, keys) in self.required.read().iter() {
            for k in keys {
                m.entry(match *k {
                    StateKey::Balance(a) | StateKey::Nonce(a) | StateKey::CodeHash(a) | StateKey::Storage(a, _) => a,
                }).or_insert_with(|| name.clone());
            }
        }
        m
    }

    pub fn status(&self) -> Vec<Status> {
        self.status.read().values().cloned().collect()
    }

    /// Per block: subscribe declared keys (fetching unknown ones), run/learn the calls,
    /// then compute readiness at `head`.
    ///
    /// Limits are enforced where keys enter the store: calls run with on-demand retention, and
    /// only keys admitted under `max_keys` become subscribed. Keys a call no longer reads are
    /// unsubscribed (except archived ones, whose history must stay continuous).
    pub async fn maintain(&self, sim: &Simulator, head: &BlockRef) {
        sim.store.set_max_subscribed(self.max_keys);
        // 1. Declared keys: subscribe (or archive) and fetch whatever is unknown.
        let declared: Vec<StateKey> = self.declared.values().flat_map(|k| k.iter().copied()).collect();
        let archived_specs: HashSet<&str> = self.specs.iter().filter(|s| s.archive()).map(|s| s.name.as_str()).collect();
        for (name, keys) in &self.declared {
            for k in keys {
                if archived_specs.contains(name.as_str()) {
                    sim.store.archive_key_checked(*k);
                } else {
                    sim.store.watch_key(*k);
                }
            }
        }
        if let Ok(snap) = sim.store.snapshot(head) {
            let (accts, slots) = crate::unknown_split(&snap, &declared);
            if !accts.is_empty() || !slots.is_empty() {
                let at = if sim.miss_at_parent { sim.store.block(head.number.saturating_sub(1)).unwrap_or(*head) } else { *head };
                prefetch::resolve_retained(&sim.store, &sim.sched, &at, &accts, &slots, &[], skim_snap::Priority::High, Retention::Subscribed)
                    .await;
            }
        }

        // 1b. `storage: all`: download each contract's whole storage once (verified against its
        //     storage root), then BALs keep it current. A reorg below the download drops it.
        for spec in self.specs.iter().filter(|s| s.storage_all()) {
            let Some(a) = spec.address else { continue };
            let max = spec.max_slots.map_or(DEFAULT_MAX_SLOTS, |l| l.0);
            if let Some(e) = self.too_large.read().get(&spec.name) {
                self.status.write().entry(spec.name.clone()).or_default().error = Some(e.clone());
                continue;
            }
            // The store drops a whole-storage contract whose growth hits the global budget.
            if let Some(e) = sim.store.take_complete_drop(&a) {
                tracing::warn!(subscription = %spec.name, address = %a, "{e}");
                self.too_large.write().insert(spec.name.clone(), e.clone());
                self.status.write().entry(spec.name.clone()).or_default().error = Some(e);
                continue;
            }
            sim.store.watch_address(a);
            if let Some(n) = sim.store.complete_storage_len(&a) {
                // The limit holds for the contract's whole life: slots first written after the
                // (verified) download count too, and those writes come from verified BALs. Past
                // it, stop tracking the contract (not just its new slots).
                if n > max {
                    sim.store.drop_complete_storage(&a);
                    let e = format!("storage grew to {n} slots, past max_slots {max}; raise max_slots (or set it to unlimited)");
                    tracing::warn!(subscription = %spec.name, address = %a, "{e}");
                    self.too_large.write().insert(spec.name.clone(), e.clone());
                    self.status.write().entry(spec.name.clone()).or_default().error = Some(e);
                }
                continue;
            }
            if self.retry_at.read().get(&spec.name).is_some_and(|(at, _)| head.number < *at) {
                continue; // backing off; the last error stays in the status
            }
            let at = if sim.miss_at_parent { sim.store.block(head.number.saturating_sub(1)).unwrap_or(*head) } else { *head };
            let tr = sim.sched.transport();
            let mut err = Some("no peer served the storage yet".to_string());
            let mut back_off = false;
            let mut oversized_reports = Vec::new();
            for peer in tr.net.peers().into_iter().filter(|(_, p)| p.snap1 || p.snap2).map(|(id, _)| id).take(4) {
                match tr.storage_all(peer, at.state_root, a, max).await {
                    Ok(skim_snap::el::StorageAll::Complete { slots, .. }) => {
                        let n = slots.len();
                        err = match sim.store.install_complete_storage(&at, a, slots) {
                            Ok(()) => {
                                tracing::info!(subscription = %spec.name, address = %a, slots = n, block = at.number, "storage: all downloaded and verified");
                                None
                            }
                            Err(e) => {
                                // e.g. the subscribed-key budget: retry later, not every block.
                                back_off = true;
                                Some(e.to_string())
                            }
                        };
                        break;
                    }
                    Ok(skim_snap::el::StorageAll::TooLarge(n)) => {
                        // The leaves passed the budget before the storage root could be checked,
                        // so this is a peer's claim, not a verified size: try the other peers.
                        oversized_reports.push(n);
                        continue;
                    }
                    Ok(skim_snap::el::StorageAll::Unavailable) | Err(_) => continue,
                }
            }
            if err.is_some() && !oversized_reports.is_empty() {
                back_off = true;
                err = Some(format!(
                    "{} peer(s) sent more than {max} slots ({}+, unverified: the download stops at max_slots before the storage root can be checked); raise max_slots if the contract really is that large",
                    oversized_reports.len(),
                    oversized_reports.iter().min().unwrap()
                ));
            }
            {
                let mut r = self.retry_at.write();
                if back_off {
                    let attempts = r.get(&spec.name).map_or(0, |(_, a)| a + 1);
                    r.insert(spec.name.clone(), (head.number + retry_after(attempts), attempts));
                } else if err.is_none() {
                    r.remove(&spec.name);
                }
            }
            if let Some(e) = err {
                self.status.write().entry(spec.name.clone()).or_default().error = Some(e);
            }
        }

        // 2. Calls: run at this block (fetched state is on demand until admitted).
        for spec in self.specs.iter().filter(|s| !s.calls.is_empty()) {
            let mut results = Vec::new();
            let mut learned = HashSet::new();
            let mut first_error = None;
            for c in &spec.calls {
                let Some(header) = sim.header(head) else {
                    first_error.get_or_insert_with(|| format!("no header for block {}", head.number));
                    break;
                };
                let tx = TxEnv::builder()
                    .caller(c.from.unwrap_or_default())
                    .kind(TxKind::Call(c.to))
                    .data(c.data.clone())
                    .gas_limit(header.gas_limit)
                    .gas_price(0)
                    .build()
                    .unwrap_or_default();
                match sim.call(head, tx, Mode::Resolve { max_rounds: 8 }).await {
                    Ok(o) => {
                        for a in &o.access.accounts {
                            learned.extend(account_keys(*a));
                        }
                        learned.extend(o.access.storage.iter().map(|(a, s)| StateKey::Storage(*a, *s)));
                        results.push(match &o.result {
                            revm::context::result::ExecutionResult::Success { output, .. } => format!("{}", output.data()),
                            other => format!("error: {other:?}"),
                        });
                    }
                    Err(e) => {
                        first_error.get_or_insert_with(|| e.to_string());
                        results.push(format!("error: {e}"));
                    }
                }
            }
            // Admit learned keys under the global limit. A failed run keeps the previous set:
            // its access set is incomplete, and the subscription is reported not ready.
            let archive = spec.archive();
            let mut truncated = false;
            if first_error.is_none() {
                let declared = self.declared.get(&spec.name).cloned().unwrap_or_default();
                let mut admitted: HashSet<StateKey> = declared.clone();
                let mut sorted: Vec<StateKey> = learned.into_iter().collect();
                sorted.sort();
                for k in sorted {
                    let ok = if archive { sim.store.archive_key_checked(k) } else { sim.store.watch_key(k) };
                    if ok {
                        admitted.insert(k);
                    } else {
                        truncated = true;
                    }
                }
                let old = self.required.write().insert(spec.name.clone(), admitted.clone()).unwrap_or_default();
                if !archive {
                    let req = self.required.read();
                    for k in old.difference(&admitted) {
                        if !req.values().any(|ks| ks.contains(k)) {
                            sim.store.unwatch_key(k);
                        }
                    }
                }
            }
            let mut st = self.status.write();
            let s = st.entry(spec.name.clone()).or_default();
            s.truncated = truncated;
            s.calls_ok = first_error.is_none();
            s.error = first_error;
            if spec.on_block {
                s.results = results;
            }
        }

        // 2b. Bytecode of subscribed accounts: a code hash alone can't run a call. Fetch what is
        //     missing (calls fetch their own as they run; declared `account: true` keys don't).
        if let Ok(snap) = sim.store.snapshot(head) {
            let all: Vec<StateKey> = self.required.read().values().flat_map(|k| k.iter().copied()).collect();
            let hashes = missing_code(sim, &snap, &all);
            if !hashes.is_empty() {
                let at = if sim.miss_at_parent { sim.store.block(head.number.saturating_sub(1)).unwrap_or(*head) } else { *head };
                prefetch::resolve_retained(&sim.store, &sim.sched, &at, &[], &[], &hashes, skim_snap::Priority::High, Retention::Subscribed).await;
            }
        }

        // 3. Readiness at the head: required keys known (with their bytecode), every call ran
        //    cleanly at this block, and nothing was cut by the key limit.
        let Ok(snap) = sim.store.snapshot(head) else { return };
        let head_ok = sim.store.head().is_some_and(|h| h.hash == head.hash);
        let req = self.required.read();
        let mut st = self.status.write();
        for spec in &self.specs {
            let keys = req.get(&spec.name).cloned().unwrap_or_default();
            let keys_vec: Vec<StateKey> = keys.iter().copied().collect();
            let missing = keys.iter().filter(|k| !matches!(snap.get(k), Ok(StateValue::Known(_)))).count() + missing_code(sim, &snap, &keys_vec).len();
            let s = st.entry(spec.name.clone()).or_default();
            s.name = spec.name.clone();
            s.block = Some(head.number);
            s.required_keys = keys.len();
            s.missing_keys = missing;
            let mut calls_ok = spec.calls.is_empty() || s.calls_ok;
            if spec.storage_all() {
                // The whole storage must be known at (or before) this block.
                let have = spec.address.and_then(|a| sim.store.complete_storage_block(&a));
                calls_ok &= have.is_some_and(|b| b <= head.number);
                if have.is_some() && spec.calls.is_empty() {
                    s.error = None;
                }
            }
            s.ready = is_ready(head_ok, missing, calls_ok, s.truncated);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unverified_oversized_answers_back_off_instead_of_disabling() {
        // Retries are spaced out but never stop: 32, 64, ... blocks, capped at 2048.
        assert_eq!(retry_after(0), 32);
        assert_eq!(retry_after(1), 64);
        assert_eq!(retry_after(6), 2048);
        assert_eq!(retry_after(40), 2048);
    }

    #[test]
    fn failed_or_truncated_simulations_are_never_ready() {
        // Review finding: a call that failed learned no keys, so "0 missing" read as ready.
        assert!(is_ready(true, 0, true, false));
        assert!(!is_ready(true, 0, false, false)); // the call failed: empty key set is not ready
        assert!(!is_ready(true, 0, true, true)); // learned set cut by the limit
        assert!(!is_ready(false, 0, true, false)); // not at the head
        assert!(!is_ready(true, 1, true, false));
    }

    #[test]
    fn limits_accept_numbers_and_unlimited() {
        let y = "- name: big\n  address: \"0x0000000000000000000000000000000000000001\"\n  storage: all\n  max_slots: unlimited\n";
        let specs: Vec<SubscriptionSpec> = serde_yaml_ng::from_str(y).unwrap();
        assert_eq!(specs[0].max_slots, Some(Limit::UNLIMITED));
        let y2 = y.replace("unlimited", "1234");
        let specs: Vec<SubscriptionSpec> = serde_yaml_ng::from_str(&y2).unwrap();
        assert_eq!(specs[0].max_slots, Some(Limit(1234)));
        assert!(serde_yaml_ng::from_str::<Vec<SubscriptionSpec>>(&y.replace("unlimited", "lots")).is_err());
    }

    #[test]
    fn declared_keys_over_the_limit_are_rejected_at_startup() {
        let y = "- name: big\n  address: \"0x0000000000000000000000000000000000000001\"\n  account: true\n  storage: [\"0x1\", \"0x2\"]\n";
        let specs: Vec<SubscriptionSpec> = serde_yaml_ng::from_str(y).unwrap();
        assert!(matches!(Subscriptions::new(specs.clone(), 4), Err(ConfigError::TooManyKeys(5, 4))));
        assert!(Subscriptions::new(specs, 5).is_ok());
    }

    #[test]
    fn parses_yaml_and_rejects_unsupported() {
        let y = r#"
- name: weth
  address: "0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14"
  account: true
  storage: ["0x3", "0x4"]
- name: quoter
  calls:
    - to: "0xEd1f6473345F45b75F8179591dd5bA1888cf2FB3"
      data: "0xc6a5026a"
  on_block: true
"#;
        let specs: Vec<SubscriptionSpec> = serde_yaml_ng::from_str(y).unwrap();
        let subs = Subscriptions::new(specs, 1000).unwrap();
        assert_eq!(subs.total_keys(), 5); // 3 account keys + 2 slots; calls learn later
        let all: Vec<SubscriptionSpec> = serde_yaml_ng::from_str("- name: x\n  address: \"0x0000000000000000000000000000000000000001\"\n  storage: all\n").unwrap();
        assert!(Subscriptions::new(all, 10).is_ok());
        let bad: Vec<SubscriptionSpec> = serde_yaml_ng::from_str("- name: x\n  address: \"0x0000000000000000000000000000000000000001\"\n  storage: everything\n").unwrap();
        assert!(matches!(Subscriptions::new(bad, 10), Err(ConfigError::StorageAll(_))));
        let archived: Vec<SubscriptionSpec> = serde_yaml_ng::from_str("- name: x\n  address: \"0x0000000000000000000000000000000000000001\"\n  storage: all\n  retention: { mode: archive }\n").unwrap();
        assert!(matches!(Subscriptions::new(archived, 10), Err(ConfigError::StorageAllArchive(_))));
        let noaddr: Vec<SubscriptionSpec> = serde_yaml_ng::from_str("- name: y\n  account: true\n").unwrap();
        assert!(matches!(Subscriptions::new(noaddr, 10), Err(ConfigError::NoAddress(_))));
    }
}
