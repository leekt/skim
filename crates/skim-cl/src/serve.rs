//! Recent payload envelopes kept to answer `ExecutionPayloadEnvelopesByRange` / `ByRoot`.
//!
//! Only envelopes the owner has verified (payload hash checked, block applied to the followed
//! chain) are added. A range answer contains only envelopes on one hash-linked chain ending at
//! the newest one, so an envelope from an abandoned fork is never served for its slot.

use std::{collections::BTreeMap, sync::Arc};

use alloy_primitives::B256;

use crate::rpc::MAX_REQUEST_PAYLOADS;

/// Slots kept (about 13 minutes on a 12 s slot; ~100–250 KB each).
pub const SERVE_SLOTS: usize = 64;

#[derive(Clone)]
pub struct Served {
    pub root: B256,
    pub block_hash: B256,
    pub parent_hash: B256,
    pub ssz: Arc<Vec<u8>>,
}

#[derive(Default)]
pub struct EnvelopeCache {
    by_slot: BTreeMap<u64, Served>,
}

impl EnvelopeCache {
    pub fn insert(&mut self, slot: u64, e: Served) {
        self.by_slot.insert(slot, e);
        while self.by_slot.len() > SERVE_SLOTS {
            self.by_slot.pop_first();
        }
    }

    /// Slots on the chain through the newest envelope (newest first walk, by parent hash).
    fn canonical(&self) -> Vec<(u64, &Served)> {
        let mut out = Vec::new();
        let mut want: Option<B256> = None;
        for (slot, e) in self.by_slot.iter().rev() {
            match want {
                Some(h) if e.block_hash != h => continue, // not on the chain: an abandoned fork
                _ => {}
            }
            out.push((*slot, e));
            want = Some(e.parent_hash);
        }
        out.reverse();
        out
    }

    /// `ExecutionPayloadEnvelopesByRange` request (`start_slot`, `count`), in slot order.
    pub fn range(&self, req: &[u8]) -> Vec<Arc<Vec<u8>>> {
        let Some((start, count)) = parse_range(req) else { return vec![] };
        let end = start.saturating_add(count.min(MAX_REQUEST_PAYLOADS));
        self.canonical().into_iter().filter(|(s, _)| (start..end).contains(s)).map(|(_, e)| e.ssz.clone()).collect()
    }

    /// `ExecutionPayloadEnvelopesByRoot` request (a list of beacon block roots).
    pub fn by_root(&self, req: &[u8]) -> Vec<Arc<Vec<u8>>> {
        if !req.len().is_multiple_of(32) {
            return vec![];
        }
        req.chunks_exact(32)
            .take(MAX_REQUEST_PAYLOADS as usize)
            .filter_map(|r| self.by_slot.values().find(|e| e.root.as_slice() == r).map(|e| e.ssz.clone()))
            .collect()
    }

    pub fn oldest_slot(&self) -> Option<u64> {
        self.by_slot.keys().next().copied()
    }
}

fn parse_range(b: &[u8]) -> Option<(u64, u64)> {
    if b.len() != 16 {
        return None;
    }
    Some((u64::from_le_bytes(b[..8].try_into().ok()?), u64::from_le_bytes(b[8..].try_into().ok()?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(n: u8, parent: u8) -> Served {
        Served {
            root: B256::repeat_byte(0x80 | n),
            block_hash: B256::repeat_byte(n),
            parent_hash: B256::repeat_byte(parent),
            ssz: Arc::new(vec![n]),
        }
    }

    fn req(start: u64, count: u64) -> Vec<u8> {
        [start.to_le_bytes(), count.to_le_bytes()].concat()
    }

    #[test]
    fn range_skips_an_abandoned_fork() {
        let mut c = EnvelopeCache::default();
        c.insert(10, env(1, 0));
        c.insert(11, env(2, 1));
        c.insert(12, env(3, 2)); // later orphaned
        c.insert(13, env(4, 2)); // the chain continues from slot 11
        c.insert(14, env(5, 4));
        let got: Vec<u8> = c.range(&req(10, 10)).iter().map(|s| s[0]).collect();
        assert_eq!(got, [1, 2, 4, 5]);
        let got: Vec<u8> = c.range(&req(12, 2)).iter().map(|s| s[0]).collect();
        assert_eq!(got, [4]);
        assert!(c.range(&req(0, 5)).is_empty());
        assert!(c.range(&[0u8; 15]).is_empty(), "malformed request");
    }

    #[test]
    fn by_root_and_bound() {
        let mut c = EnvelopeCache::default();
        for i in 0..(SERVE_SLOTS as u8 + 5) {
            c.insert(i as u64, env(i, i.wrapping_sub(1)));
        }
        assert_eq!(c.oldest_slot(), Some(5));
        let roots = [B256::repeat_byte(0x80 | 7), B256::repeat_byte(0xff), B256::repeat_byte(0x80 | 6)].concat();
        let got: Vec<u8> = c.by_root(&roots).iter().map(|s| s[0]).collect();
        assert_eq!(got, [7, 6]);
        assert!(c.by_root(&roots[..40]).is_empty());
    }
}
