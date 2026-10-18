//! Miss resolution with a time breakdown, and Uniswap V3 tick prefetch (PROJECT.md §7.2).
//!
//! **Misses** are fetched at the *parent* block's root by default. At gossip time, SNAP peers
//! often haven't executed the new block yet. The store then replays the new block's indexed BAL
//! writes onto the fetched base (`Store::insert_base`), so the result is exact for the new block.
//!
//! **V3 prefetch** predicts the state a swap can touch from the pool's own state:
//! - `slot0` gives the current tick and observation index
//! - the tick-bitmap words within ±W of the current word are fetched
//! - every initialized tick in those words is fetched (4 slots each)
//! - so are the current observation, `feeGrowthGlobal*`, `protocolFees` and `liquidity`
//!
//! The refresh runs per block, off the simulation path, and fetches only keys that are unknown.

use alloy_primitives::{Address, B256, Bytes, I256, U256, keccak256};
use revm::primitives::KECCAK_EMPTY;
use skim_bal::StateKey;
use skim_snap::{FetchMeta, Key, Priority, Scheduler, Value, el::ElTransport};
use skim_state::{BlockRef, Retention, Snapshot, StateValue, Store};
#[cfg(test)]
use std::sync::Arc;

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Resolve {
    pub keys: usize,
    pub wall_ms: f64,
    pub queue_ms_max: f64,
    pub service_ms_max: f64,
    pub attempts_max: u32,
    pub unavailable: u32,
    pub hedged: u32,
    /// A second, dependent round for code of newly seen accounts.
    pub code_round: bool,
    pub failed: usize,
}

fn absorb(r: &mut Resolve, m: &FetchMeta) {
    r.queue_ms_max = r.queue_ms_max.max(m.queue_ms);
    r.service_ms_max = r.service_ms_max.max(m.total_ms - m.queue_ms);
    r.attempts_max = r.attempts_max.max(m.attempts);
    r.unavailable += m.unavailable;
    r.hedged += m.hedged as u32;
}

/// Fetches `accounts`/`storage`/`code` at `at.state_root` and commits them as bases at `at`.
pub async fn resolve(
    store: &Store,
    sched: &Scheduler<ElTransport>,
    at: &BlockRef,
    accounts: &[Address],
    storage: &[(Address, U256)],
    code: &[B256],
) -> Resolve {
    resolve_with(store, sched, at, accounts, storage, code, Priority::High).await
}

/// [`resolve_with`] that commits values with an explicit retention (on-demand TTL vs subscribed).
pub async fn resolve_retained(
    store: &Store,
    sched: &Scheduler<ElTransport>,
    at: &BlockRef,
    accounts: &[Address],
    storage: &[(Address, U256)],
    code: &[B256],
    prio: Priority,
    retention: Retention,
) -> Resolve {
    resolve_inner(store, sched, at, accounts, storage, code, prio, retention).await
}

/// [`resolve`] at a given scheduler priority (background prefetch uses `Low`).
pub async fn resolve_with(
    store: &Store,
    sched: &Scheduler<ElTransport>,
    at: &BlockRef,
    accounts: &[Address],
    storage: &[(Address, U256)],
    code: &[B256],
    prio: Priority,
) -> Resolve {
    resolve_inner(store, sched, at, accounts, storage, code, prio, Retention::Subscribed).await
}

/// Fetches the keys and installs each verified value as soon as it arrives (completion order),
/// so an account's bytecode request starts when that account is in, not after the whole wave.
/// Returns once every key, and every bytecode it led to, has an answer.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve_inner<T: skim_snap::Transport>(
    store: &Store,
    sched: &Scheduler<T>,
    at: &BlockRef,
    accounts: &[Address],
    storage: &[(Address, U256)],
    code: &[B256],
    prio: Priority,
    retention: Retention,
) -> Resolve {
    use futures::{FutureExt, StreamExt};
    let t = std::time::Instant::now();
    let root = at.state_root;
    let mut keys: Vec<Key> = accounts.iter().map(|&address| Key::Account { root, address }).collect();
    keys.extend(storage.iter().map(|&(address, slot)| Key::Storage { root, address, slot }));
    keys.extend(code.iter().map(|&h| Key::Code(h)));
    let mut r = Resolve { keys: keys.len(), ..Default::default() };
    // Code hashes requested in this resolve (given, or chained from an account): once each.
    let mut codes: std::collections::HashSet<B256> = code.iter().copied().collect();
    let mut pending = futures::stream::FuturesUnordered::new();
    for (k, f) in keys.iter().copied().zip(sched.submit(&keys, prio)) {
        pending.push(async move { (k, f.await) }.boxed());
    }
    while let Some((k, (res, meta))) = pending.next().await {
        absorb(&mut r, &meta);
        match (k, res) {
            (Key::Account { address, .. }, Ok(Value::Account(a))) => {
                let (bal, nonce, ch) = match a {
                    Some(a) => (a.balance, U256::from(a.nonce), a.code_hash),
                    None => (U256::ZERO, U256::ZERO, KECCAK_EMPTY),
                };
                let _ = store.insert_bases_with(
                    at,
                    &[(StateKey::Balance(address), bal), (StateKey::Nonce(address), nonce), (StateKey::CodeHash(address), U256::from_be_bytes(ch.0))],
                    retention,
                );
                if ch != KECCAK_EMPTY && store.code(&ch).is_none() && codes.insert(ch) {
                    r.code_round = true;
                    let k = Key::Code(ch);
                    let f = sched.submit(&[k], prio).pop().expect("one key");
                    pending.push(async move { (k, f.await) }.boxed());
                }
            }
            (Key::Storage { address, slot, .. }, Ok(Value::Storage(v))) => {
                let _ = store.insert_base_with(at, StateKey::Storage(address, slot), v, retention);
            }
            (Key::Code(_), Ok(Value::Code(c))) => {
                store.insert_code(c);
            }
            _ => r.failed += 1,
        }
    }
    r.wall_ms = t.elapsed().as_secs_f64() * 1e3;
    r
}

// ---------- Uniswap V3 ----------

const SLOT0: u64 = 0;
const TICKS_SLOT: u64 = 5;
const BITMAP_SLOT: u64 = 6;
const OBSERVATIONS_SLOT: u64 = 8;

#[derive(Debug, Clone, Copy)]
pub struct V3Pool {
    pub address: Address,
    pub spacing: i32,
}

fn mapping_slot(key: I256, slot: u64) -> U256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(&key.to_be_bytes::<32>());
    buf[32..].copy_from_slice(&U256::from(slot).to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(buf).0)
}

pub fn bitmap_slot(word: i32) -> U256 {
    mapping_slot(I256::try_from(word).unwrap(), BITMAP_SLOT)
}

pub fn tick_base_slot(tick: i32) -> U256 {
    mapping_slot(I256::try_from(tick).unwrap(), TICKS_SLOT)
}

/// `(tick, observationIndex)` from a packed `slot0`.
pub fn decode_slot0(v: U256) -> (i32, u16) {
    let raw: u32 = ((v >> 160usize) & U256::from(0xff_ffffu32)).to::<u32>();
    let tick = ((raw << 8) as i32) >> 8; // sign-extend int24
    let obs: u16 = ((v >> 184usize) & U256::from(0xffffu32)).to::<u16>();
    (tick, obs)
}

pub fn word_of(tick: i32, spacing: i32) -> i32 {
    tick.div_euclid(spacing) >> 8
}

/// Keys a swap from the current state can touch within ±`words` bitmap words.
/// Initialized ticks are known only once their bitmap words are, so this is called again after
/// each refresh round until nothing new appears.
pub fn predict(snap: &Snapshot, pool: &V3Pool, words: i32) -> Vec<StateKey> {
    predict_range(snap, pool, -words, words)
}

/// Like [`predict`] with an asymmetric word window `[w0 + lo, w0 + hi]`.
pub fn predict_range(snap: &Snapshot, pool: &V3Pool, lo: i32, hi: i32) -> Vec<StateKey> {
    let a = pool.address;
    let get = |slot: U256| snap.get(&StateKey::Storage(a, slot)).ok();
    let mut keys: Vec<StateKey> = [0u64, 1, 2, 3, 4].iter().map(|s| StateKey::Storage(a, U256::from(*s))).collect();
    keys.extend([StateKey::Balance(a), StateKey::Nonce(a), StateKey::CodeHash(a)]);
    let Some(StateValue::Known(s0)) = get(U256::from(SLOT0)) else { return keys };
    let (tick, obs) = decode_slot0(s0);
    keys.push(StateKey::Storage(a, U256::from(OBSERVATIONS_SLOT + obs as u64)));
    let w0 = word_of(tick, pool.spacing);
    for w in (w0 + lo)..=(w0 + hi) {
        let wk = bitmap_slot(w);
        keys.push(StateKey::Storage(a, wk));
        if let Some(StateValue::Known(bits)) = get(wk) {
            for i in 0..256 {
                if bits.bit(i) {
                    let t = ((w << 8) + i as i32) * pool.spacing;
                    let base = tick_base_slot(t);
                    for o in 0..4u64 {
                        keys.push(StateKey::Storage(a, base + U256::from(o)));
                    }
                }
            }
        }
    }
    keys
}

/// Fetches whatever `predict` says is unknown, repeating while new keys appear (bitmap words
/// first, then the ticks they reveal). Returns the rounds' breakdowns.
///
/// With `Retention::Ttl(n)` the window is a hot set, not a subscription: every key in the
/// current window gets its TTL extended on each pass, and keys the price moved away from
/// expire `n` blocks after they last were in the window.
#[allow(clippy::too_many_arguments)]
pub async fn refresh_pool(
    store: &std::sync::Arc<Store>,
    sched: &Scheduler<ElTransport>,
    head: &BlockRef,
    at: &BlockRef,
    pool: &V3Pool,
    (lo, hi): (i32, i32),
    budget: usize,
    retention: Retention,
) -> (Vec<Resolve>, Vec<StateKey>) {
    let mut rounds = Vec::new();
    let mut fetched = Vec::new();
    let mut spent = 0usize;
    for _ in 0..3 {
        if spent >= budget {
            break;
        }
        let Ok(snap) = store.snapshot(head) else { break };
        // Closest to the current word first, so a budget cut keeps the most likely keys.
        let w0 = match snap.get(&StateKey::Storage(pool.address, U256::ZERO)) {
            Ok(StateValue::Known(s0)) => word_of(decode_slot0(s0).0, pool.spacing),
            _ => 0,
        };
        let mut ordered: Vec<(i32, StateKey)> = Vec::new();
        for d in 0..=lo.abs().max(hi) {
            for w in [w0 - d, w0 + d] {
                if w < w0 + lo || w > w0 + hi || (d == 0 && w != w0) {
                    continue;
                }
                for k in predict_range(&snap, pool, w - w0, w - w0) {
                    ordered.push((d, k));
                }
            }
        }
        if let Retention::Ttl(n) = retention {
            let window: Vec<StateKey> = ordered.iter().map(|(_, k)| *k).collect();
            store.touch_ttl(&window, n);
        }
        let mut seen = std::collections::HashSet::new();
        let unknown: Vec<StateKey> = ordered
            .into_iter()
            .map(|(_, k)| k)
            .filter(|k| seen.insert(*k) && matches!(snap.get(k), Ok(StateValue::Unknown)))
            .take(budget - spent)
            .collect();
        if unknown.is_empty() {
            break;
        }
        spent += unknown.len();
        fetched.extend(unknown.iter().copied());
        let mut accounts = Vec::new();
        let mut storage = Vec::new();
        for k in unknown {
            match k {
                StateKey::Storage(a, s) => storage.push((a, s)),
                StateKey::Balance(a) | StateKey::Nonce(a) | StateKey::CodeHash(a) => {
                    if !accounts.contains(&a) {
                        accounts.push(a);
                    }
                }
            }
        }
        rounds.push(resolve_inner(store, sched, at, &accounts, &storage, &[], Priority::Low, retention).await);
    }
    (rounds, fetched)
}

/// `tickSpacing()` decoded from a call's output (int24 ABI-encoded).
pub fn decode_spacing(out: &Bytes) -> Option<i32> {
    (out.len() == 32).then(|| {
        let v = U256::from_be_slice(out);
        let raw = (v & U256::from(0xff_ffffu32)).to::<u32>();
        ((raw << 8) as i32) >> 8
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot0_and_words() {
        // tick = -201000 (0xfcEED8 as int24), observationIndex = 7
        let tick: i32 = -201000;
        let raw = (tick as u32) & 0xff_ffff;
        let v = (U256::from(raw) << 160) | (U256::from(7u32) << 184) | U256::from(12345u64);
        assert_eq!(decode_slot0(v), (-201000, 7));
        assert_eq!(word_of(-201000, 60), (-3350i32) >> 8); // -3350 / 256 floors to -14
        assert_eq!(word_of(-201000, 60), -14);
        assert_eq!(word_of(59, 60), 0);
        assert_eq!(word_of(-1, 60), -1);
    }

    #[test]
    fn mapping_slots_match_solidity_abi_encoding() {
        // keccak256(abi.encode(int16(-1), uint256(6))): key is sign-extended to 32 bytes.
        let mut buf = [0xffu8; 64];
        buf[32..].copy_from_slice(&U256::from(6u64).to_be_bytes::<32>());
        assert_eq!(bitmap_slot(-1), U256::from_be_bytes(keccak256(buf).0));
    }

    // ---- completion-order resolution (no network: a mock transport) ----

    use skim_snap::{AccountAnswer, AccountState, Config, PeerId, SlotAnswer, Transport, TransportError};
    use std::time::Duration;

    const CODE: &[u8] = b"pipelined-code";
    const SLOW_SLOT_MS: u64 = 400;
    const CODE_MS: u64 = 250;

    /// One peer: accounts answer at once (with bytecode to fetch), storage takes `SLOW_SLOT_MS`,
    /// bytecode `CODE_MS`. Counts account requests.
    #[derive(Clone, Default)]
    struct Mock {
        account_requests: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Transport for Mock {
        fn snap_peers(&self) -> Vec<PeerId> {
            vec![PeerId::repeat_byte(1)]
        }
        async fn account(&self, _: PeerId, _: B256, _: Address) -> Result<AccountAnswer, TransportError> {
            self.account_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok(AccountAnswer::Found(Some(AccountState { nonce: 1, balance: U256::from(9), storage_root: B256::ZERO, code_hash: keccak256(CODE) })))
        }
        async fn storage(&self, _: PeerId, _: B256, _: Address, slot: U256) -> Result<SlotAnswer, TransportError> {
            tokio::time::sleep(Duration::from_millis(SLOW_SLOT_MS)).await;
            Ok(SlotAnswer::Found(slot + U256::from(1)))
        }
        async fn codes(&self, _: PeerId, hashes: Vec<B256>) -> Result<Vec<Bytes>, TransportError> {
            tokio::time::sleep(Duration::from_millis(CODE_MS)).await;
            Ok(hashes.iter().filter(|h| **h == keccak256(CODE)).map(|_| Bytes::from_static(CODE)).collect())
        }
    }

    #[tokio::test]
    async fn bytecode_is_fetched_as_soon_as_its_account_arrives() {
        // Account (fast) -> its bytecode (250 ms), alongside an unrelated slow slot (400 ms).
        // Waiting for the whole wave before chaining the bytecode takes 400 + 250 ms; in
        // completion order the bytecode overlaps the slot, so ~400 ms.
        let store = Store::new(64);
        let at = BlockRef { number: 1, hash: B256::repeat_byte(1), parent_hash: B256::ZERO, state_root: B256::repeat_byte(7), timestamp: 12 };
        store.apply_block(at, Default::default()).unwrap();
        let mock = Mock::default();
        let sched = Scheduler::new(mock.clone(), Config { hedge_after: Duration::from_secs(10), deadline: Duration::from_secs(5), ..Default::default() });
        let (a, s) = (Address::repeat_byte(0xaa), Address::repeat_byte(0xbb));
        let r = resolve_inner(&store, &sched, &at, &[a], &[(s, U256::from(3))], &[], Priority::High, Retention::Subscribed).await;
        assert_eq!(r.failed, 0);
        assert!(r.code_round);
        assert!(store.code(&keccak256(CODE)).is_some(), "the chained bytecode is installed before returning");
        let snap = store.snapshot(&at).unwrap();
        assert_eq!(snap.get(&StateKey::Storage(s, U256::from(3))).unwrap(), StateValue::Known(U256::from(4)));
        assert_eq!(snap.get(&StateKey::Balance(a)).unwrap(), StateValue::Known(U256::from(9)));
        assert!(
            r.wall_ms < (SLOW_SLOT_MS + CODE_MS) as f64 - 100.0,
            "bytecode waited for the unrelated slot: {:.0} ms",
            r.wall_ms
        );
        assert_eq!(mock.account_requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
