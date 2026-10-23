//! Which blocks a WebSocket session's `newHeads` and `logs` subscriptions must hear about when a
//! new head arrives. A session remembers the blocks it reported; on each new head it reports
//! the blocks that left the chain since (`logs` re-sends their logs with `removed: true`, as
//! geth does) and every block between its last report and the head (blocks a gap fill applied
//! at once, or events missed when the broadcast lagged). (Review: only the announced head was
//! reported, never a removal, and a lagged broadcast was skipped.)

use std::collections::BTreeMap;

use alloy_primitives::B256;

/// Blocks a session remembers, and the furthest back a step reports.
pub const KEEP: u64 = 64;

#[derive(Debug, Default)]
pub struct HeadTrack {
    seen: BTreeMap<u64, B256>,
}

/// What changed for a session since its last step.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Step {
    /// Reported blocks that are no longer on the chain, newest first.
    pub removed: Vec<(u64, B256)>,
    /// Blocks to report, oldest first, ending at the head.
    pub added: Vec<(u64, B256)>,
}

impl HeadTrack {
    /// Moves to `head`. `canon(n)` is the hash of block `n` on the selected chain, if known: a
    /// block whose hash isn't known (pruned) is assumed unchanged, never reported as removed.
    pub fn advance(&mut self, head: (u64, B256), canon: impl Fn(u64) -> Option<B256>) -> Step {
        let mut step = Step::default();
        while let Some((&n, &h)) = self.seen.last_key_value() {
            let still = match n.cmp(&head.0) {
                std::cmp::Ordering::Greater => false,
                std::cmp::Ordering::Equal => h == head.1,
                std::cmp::Ordering::Less => canon(n).is_none_or(|c| c == h),
            };
            if still {
                break;
            }
            step.removed.push((n, h));
            self.seen.remove(&n);
        }
        let from = self.seen.last_key_value().map_or(head.0, |(n, _)| n + 1).max(head.0.saturating_sub(KEEP - 1));
        for n in from..=head.0 {
            match if n == head.0 { Some(head.1) } else { canon(n) } {
                Some(h) => step.added.push((n, h)),
                // A block in between that isn't known: report only what follows it.
                None => step.added.clear(),
            }
        }
        for &(n, h) in &step.added {
            self.seen.insert(n, h);
        }
        while self.seen.len() as u64 > KEEP {
            self.seen.pop_first();
        }
        step
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u64, fork: u8) -> B256 {
        let mut x = [fork; 32];
        x[..8].copy_from_slice(&n.to_be_bytes());
        B256::from(x)
    }

    #[test]
    fn reports_every_block_up_to_the_head_and_what_a_reorg_removed() {
        let mut t = HeadTrack::default();
        let a = |n| Some(h(n, 0xa));
        // The first head: just it, nothing before the session existed.
        assert_eq!(t.advance((100, h(100, 0xa)), a), Step { removed: vec![], added: vec![(100, h(100, 0xa))] });
        // A gap fill applied 101..=103 at once and announced only 103.
        assert_eq!(t.advance((103, h(103, 0xa)), a).added, (101..=103).map(|n| (n, h(n, 0xa))).collect::<Vec<_>>());
        // A reorg from 102: B102, B103, B104. 103 and 102 were reported on A: removed, newest first.
        let b = |n: u64| Some(if n >= 102 { h(n, 0xb) } else { h(n, 0xa) });
        let s = t.advance((104, h(104, 0xb)), b);
        assert_eq!(s.removed, vec![(103, h(103, 0xa)), (102, h(102, 0xa))]);
        assert_eq!(s.added, (102..=104).map(|n| (n, h(n, 0xb))).collect::<Vec<_>>());
        // The same head again: nothing.
        assert_eq!(t.advance((104, h(104, 0xb)), b), Step::default());
        // A sibling of the head replaces it.
        let s = t.advance((104, h(104, 0xc)), b);
        assert_eq!((s.removed, s.added), (vec![(104, h(104, 0xb))], vec![(104, h(104, 0xc))]));
    }

    #[test]
    fn a_long_gap_reports_at_most_the_last_blocks_and_pruned_blocks_are_not_removals() {
        let mut t = HeadTrack::default();
        let a = |n| Some(h(n, 0xa));
        t.advance((100, h(100, 0xa)), a);
        let s = t.advance((1000, h(1000, 0xa)), a);
        assert_eq!(s.added.len() as u64, KEEP);
        assert_eq!(s.added.first().unwrap().0, 1000 - KEEP + 1);
        // Hashes the store no longer holds aren't evidence of a reorg.
        let s = t.advance((1001, h(1001, 0xa)), |n| (n == 1001).then(|| h(n, 0xa)));
        assert_eq!((s.removed, s.added), (vec![], vec![(1001, h(1001, 0xa))]));
    }
}
