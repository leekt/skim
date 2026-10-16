//! State Access Predictor (PROJECT.md §7.2): one place that turns a simulation input into the
//! state keys it will read, before running it.
//!
//! ```text
//! input ──► predict() ─┬─ learned access sets (exact input, else frequency-weighted core)
//!                      └─ protocol-aware sources (Uniswap V3: tick range from amount and
//!                         liquidity → bitmap words + initialized ticks)
//!        ──► fetch unknown keys in one wave (parent root) ──► MVCC ──► revm
//!        ──► on a miss: expand() (V3: the whole bitmap-word range up to the missed word)
//!        ──► record() the access set
//! ```

use std::collections::{HashMap, HashSet};

use alloy_primitives::{Address, U256};
use skim_bal::StateKey;
use skim_state::{Snapshot, StateValue};

use crate::{
    CallInput as Call,
    learned::{AccessKey, Learned},
    prefetch::{self, V3Pool},
};

const MAX_WORDS: i32 = 64;
const QUOTE_EXACT_INPUT_SINGLE: [u8; 4] = [0xc6, 0xa5, 0x02, 0x6a];

#[derive(Debug, Clone, Copy)]
pub struct V3Meta {
    pub pool: V3Pool,
    pub fee: u32,
    pub token0: Address,
    pub token1: Address,
}

pub struct Predictor {
    pub learned: Learned,
    pub v3: Vec<V3Meta>,
    /// Bitmap words always covered around the current tick.
    pub base_words: i32,
    /// Per pool, the furthest word offsets (below, above the current word) any prediction has
    /// needed. The background refresh keeps this window warm, so repeat reaches stay off the
    /// simulation path.
    pub reach: std::sync::Mutex<HashMap<Address, (i32, i32)>>,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Prediction {
    pub learned: usize,
    pub protocol: usize,
    /// Bitmap words the V3 source predicted for this input (0 if not a V3 quote).
    pub v3_words: i32,
}

use crate::account_keys;

fn u256_word(data: &[u8], i: usize) -> Option<U256> {
    data.get(4 + 32 * i..4 + 32 * (i + 1)).map(U256::from_be_slice)
}

/// Ticks a swap of `amount_in` can reach: `(current_tick, target_tick)` from the
/// constant-liquidity formula. The caller widens it, since liquidity can drop after a crossing.
pub fn v3_target_tick(slot0: U256, liquidity: U256, amount_in: U256, zero_for_one: bool) -> (i32, Option<i32>) {
    let (tick, _) = prefetch::decode_slot0(slot0);
    let sqrt_x96: U256 = slot0 & ((U256::from(1u8) << 160usize) - U256::from(1u8));
    let sqrt_p = f64::from(sqrt_x96) / 2f64.powi(96);
    let l = f64::from(liquidity & U256::from(u128::MAX));
    let x = f64::from(amount_in);
    if l <= 0.0 || sqrt_p <= 0.0 {
        return (tick, None);
    }
    let sqrt_new = if zero_for_one { 1.0 / (1.0 / sqrt_p + x / l) } else { sqrt_p + x / l };
    let t = 2.0 * sqrt_new.ln() / 1.0001f64.ln();
    (tick, t.is_finite().then(|| t.clamp(-887272.0, 887272.0) as i32))
}

impl Predictor {
    fn v3_for_quote(&self, call: &Call) -> Option<(V3Meta, bool, U256)> {
        if call.data.len() < 4 + 32 * 5 || call.data[..4] != QUOTE_EXACT_INPUT_SINGLE {
            return None;
        }
        let tin = Address::from_word(u256_word(&call.data, 0)?.into());
        let tout = Address::from_word(u256_word(&call.data, 1)?.into());
        let amount = u256_word(&call.data, 2)?;
        let fee: u32 = u256_word(&call.data, 3)?.to();
        let m = self.v3.iter().find(|m| {
            m.fee == fee && ((m.token0 == tin && m.token1 == tout) || (m.token0 == tout && m.token1 == tin))
        })?;
        Some((*m, tin == m.token0, amount))
    }

    /// V3 keys for a quote: bitmap words from the current word to the predicted target word
    /// (+50% and 1 word of margin), the initialized ticks in known words, and pool scalars.
    fn v3_keys(&self, m: &V3Meta, snap: &Snapshot, zero_for_one: bool, amount: U256) -> (Vec<StateKey>, i32) {
        let a = m.pool.address;
        let get = |slot: U256| snap.get(&StateKey::Storage(a, slot)).ok();
        let (Some(StateValue::Known(s0)), Some(StateValue::Known(liq))) = (get(U256::ZERO), get(U256::from(4u8))) else {
            return (prefetch::predict(snap, &m.pool, self.base_words), 0);
        };
        let (tick, target) = v3_target_tick(s0, liq, amount, zero_for_one);
        let w0 = prefetch::word_of(tick, m.pool.spacing);
        let dist = match target {
            Some(t) => (prefetch::word_of(t, m.pool.spacing) - w0).abs(),
            None => MAX_WORDS,
        };
        let reach = ((dist as f64 * 1.5).ceil() as i32 + 1).clamp(self.base_words, MAX_WORDS);
        let (lo, hi) = if zero_for_one { (w0 - reach, w0 + 1) } else { (w0 - 1, w0 + reach) };
        {
            let mut r = self.reach.lock().unwrap();
            let e = r.entry(a).or_insert((-self.base_words, self.base_words));
            e.0 = e.0.min(lo - w0);
            e.1 = e.1.max(hi - w0);
        }
        let mut keys = prefetch::predict(snap, &m.pool, self.base_words);
        // Nearest words first: if a wave is capped, the keys a swap reads first are fetched first.
        let mut words: Vec<i32> = (lo..=hi).collect();
        words.sort_by_key(|w| (w - w0).abs());
        for w in words {
            let wk = prefetch::bitmap_slot(w);
            keys.push(StateKey::Storage(a, wk));
            if let Some(StateValue::Known(bits)) = get(wk) {
                for i in 0..256usize {
                    if bits.bit(i) {
                        let base = prefetch::tick_base_slot(((w << 8) + i as i32) * m.pool.spacing);
                        keys.extend((0..4u64).map(|o| StateKey::Storage(a, base + U256::from(o))));
                    }
                }
            }
        }
        (keys, hi - lo + 1)
    }

    /// Keys this input will likely read.
    pub fn predict(&self, call: &Call, snap: &Snapshot) -> (Vec<StateKey>, Prediction) {
        let mut keys = Vec::new();
        let mut p = Prediction::default();
        if let Some((_, learned)) = self.learned.predict(call.to, &call.data) {
            for k in learned {
                match k {
                    AccessKey::Account(a) => keys.extend(account_keys(a)),
                    AccessKey::Storage(a, s) => keys.push(StateKey::Storage(a, s)),
                }
            }
            p.learned = keys.len();
        }
        if let Some((m, z, amount)) = self.v3_for_quote(call) {
            let (v3, words) = self.v3_keys(&m, snap, z, amount);
            p.protocol = v3.len();
            p.v3_words = words;
            keys.extend(v3);
        }
        let mut seen = HashSet::new();
        keys.retain(|k| seen.insert(*k));
        (keys, p)
    }

    /// Widens a miss: a missed V3 bitmap word pulls in every word between the current word and
    /// it, so the next round doesn't discover them one by one.
    pub fn expand(&self, snap: &Snapshot, misses: &[(Address, U256)]) -> Vec<StateKey> {
        let mut out = Vec::new();
        for m in &self.v3 {
            let a = m.pool.address;
            let mine: HashSet<U256> = misses.iter().filter(|(x, _)| *x == a).map(|(_, s)| *s).collect();
            if mine.is_empty() {
                continue;
            }
            let Ok(StateValue::Known(s0)) = snap.get(&StateKey::Storage(a, U256::ZERO)) else { continue };
            let w0 = prefetch::word_of(prefetch::decode_slot0(s0).0, m.pool.spacing);
            let words: HashMap<U256, i32> =
                ((w0 - 4 * MAX_WORDS)..=(w0 + 4 * MAX_WORDS)).map(|w| (prefetch::bitmap_slot(w), w)).collect();
            for s in &mine {
                if let Some(&w) = words.get(s) {
                    let (lo, hi) = if w < w0 { (w, w0) } else { (w0, w) };
                    out.extend((lo..=hi).map(|x| StateKey::Storage(a, prefetch::bitmap_slot(x))));
                }
            }
        }
        out
    }

    /// Background window for a pool: the observed reach, at least ±`base_words`.
    pub fn window(&self, pool: &Address) -> (i32, i32) {
        self.reach.lock().unwrap().get(pool).copied().unwrap_or((-self.base_words, self.base_words))
    }

    pub fn record(&mut self, call: &Call, accounts: &[Address], storage: &[(Address, U256)]) {
        let keys = accounts
            .iter()
            .map(|a| AccessKey::Account(*a))
            .chain(storage.iter().map(|(a, s)| AccessKey::Storage(*a, *s)))
            .collect();
        self.learned.record(call.to, &call.data, keys);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot0(sqrt_x96: U256, tick: i32) -> U256 {
        sqrt_x96 | (U256::from((tick as u32) & 0xff_ffff) << 160usize)
    }

    #[test]
    fn target_tick_moves_the_right_way_and_scales_with_amount() {
        // Price 1.0 (tick 0), L = 1e18.
        let s0 = slot0(U256::from(1u8) << 96usize, 0);
        let l = U256::from(10u128.pow(18));
        let (t, small) = v3_target_tick(s0, l, U256::from(10u128.pow(15)), true);
        let (_, big) = v3_target_tick(s0, l, U256::from(10u128.pow(18)), true);
        let (_, up) = v3_target_tick(s0, l, U256::from(10u128.pow(18)), false);
        assert_eq!(t, 0);
        let (small, big, up) = (small.unwrap(), big.unwrap(), up.unwrap());
        assert!(small < 0 && big < small, "{small} {big}");
        // x = L at price 1: 1/sqrtP' = 2 -> P' = 1/4 -> tick ~ -13863
        assert!((big + 13863).abs() < 5, "{big}");
        assert!(up > 0);
        assert_eq!(v3_target_tick(s0, U256::ZERO, U256::from(1u8), true).1, None);
    }
}
