//! Which blocks a WebSocket subscription must hear about when a new head arrives. Each
//! subscription remembers the blocks it reported, starting from the head when it subscribed. On
//! each new head it reports the blocks that left the chain since (`logs` re-sends their logs
//! with `removed: true`, as geth does) and every block between its last report and the head
//! (blocks a gap fill applied at once, or heads missed when the broadcast lagged). A block counts
//! as reported only once it was delivered ([`HeadTrack::mark`]): one that failed is offered again
//! on the next head. (Review: only the announced head was reported, never a removal; then the
//! position advanced before delivery, and an unseeded tracker skipped blocks after subscribing.)

use std::collections::BTreeMap;

use alloy_primitives::B256;

/// Blocks a subscription remembers, and the furthest back a step reports.
pub const KEEP: u64 = 64;

#[derive(Debug, Default)]
pub struct HeadTrack {
    seen: BTreeMap<u64, B256>,
}

/// What changed for a subscription since its last delivered block.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Step {
    /// Reported blocks that are no longer on the chain, newest first.
    pub removed: Vec<(u64, B256)>,
    /// Blocks to report, oldest first, ending at the head.
    pub added: Vec<(u64, B256)>,
    /// Blocks after the last reported one that can't be reported any more (more than [`KEEP`]
    /// behind, or not known): a gap the subscriber must be told about.
    pub skipped: Option<(u64, u64)>,
}

impl HeadTrack {
    /// A subscription made when `head` was current: blocks after it are reported, and `head`
    /// itself is reported again if a reorg replaces it.
    pub fn seeded(head: Option<(u64, B256)>) -> Self {
        Self { seen: head.into_iter().collect() }
    }

    /// What to report for `head`. `canon(n)` is the hash of block `n` on the selected chain, if
    /// known: a block whose hash isn't known (pruned) is assumed unchanged, never reported as
    /// removed. Removed blocks are forgotten here; added ones only once [`Self::mark`]ed.
    pub fn step(&mut self, head: (u64, B256), canon: impl Fn(u64) -> Option<B256>) -> Step {
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
        let next = self.seen.last_key_value().map_or(head.0, |(n, _)| n + 1);
        let from = next.max(head.0.saturating_sub(KEEP - 1));
        for n in from..=head.0 {
            match if n == head.0 { Some(head.1) } else { canon(n) } {
                Some(h) => step.added.push((n, h)),
                // A block in between that isn't known: report only what follows it.
                None => step.added.clear(),
            }
        }
        let first = step.added.first().map_or(head.0 + 1, |(n, _)| *n);
        if !self.seen.is_empty() && next < first {
            step.skipped = Some((next, first - 1));
        }
        step
    }

    /// `n` (hash `h`) was delivered.
    pub fn mark(&mut self, n: u64, h: B256) {
        self.seen.insert(n, h);
        while self.seen.len() as u64 > KEEP {
            self.seen.pop_first();
        }
    }

    /// Treats a reported gap as passed: the subscriber was told.
    pub fn skip_to(&mut self, n: u64, h: B256) {
        self.seen.clear();
        self.seen.insert(n, h);
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

    fn deliver(t: &mut HeadTrack, s: &Step) {
        for &(n, x) in &s.added {
            t.mark(n, x);
        }
    }

    #[test]
    fn reports_every_block_since_subscribing_and_what_a_reorg_removed() {
        let a = |n| Some(h(n, 0xa));
        // Subscribed at 100; the next announcement is 104: 101..=104, not just 104.
        let mut t = HeadTrack::seeded(Some((100, h(100, 0xa))));
        let s = t.step((104, h(104, 0xa)), a);
        assert_eq!(s.added, (101..=104).map(|n| (n, h(n, 0xa))).collect::<Vec<_>>());
        deliver(&mut t, &s);
        // A reorg from 103: B103, B104, B105. 104 and 103 were reported on A: removed, newest first.
        let b = |n: u64| Some(if n >= 103 { h(n, 0xb) } else { h(n, 0xa) });
        let s = t.step((105, h(105, 0xb)), b);
        assert_eq!(s.removed, vec![(104, h(104, 0xa)), (103, h(103, 0xa))]);
        assert_eq!(s.added, (103..=105).map(|n| (n, h(n, 0xb))).collect::<Vec<_>>());
        deliver(&mut t, &s);
        assert_eq!(t.step((105, h(105, 0xb)), b), Step::default(), "the same head again: nothing");
        // The block it subscribed at, replaced: the replacement is reported (it's new).
        let mut t = HeadTrack::seeded(Some((100, h(100, 0xa))));
        let s = t.step((100, h(100, 0xc)), |_| None);
        assert_eq!((s.removed, s.added), (vec![(100, h(100, 0xa))], vec![(100, h(100, 0xc))]));
    }

    #[test]
    fn an_undelivered_block_is_offered_again() {
        let a = |n| Some(h(n, 0xa));
        let mut t = HeadTrack::seeded(Some((100, h(100, 0xa))));
        let s = t.step((102, h(102, 0xa)), a);
        t.mark(101, h(101, 0xa)); // 102 failed (receipts not fetched)
        assert_eq!(s.added.len(), 2);
        let s = t.step((103, h(103, 0xa)), a);
        assert_eq!(s.added, vec![(102, h(102, 0xa)), (103, h(103, 0xa))]);
    }

    #[test]
    fn a_gap_too_long_to_report_is_said_so_and_pruned_blocks_are_not_removals() {
        let a = |n| Some(h(n, 0xa));
        let mut t = HeadTrack::seeded(Some((100, h(100, 0xa))));
        let s = t.step((1000, h(1000, 0xa)), a);
        assert_eq!(s.added.len() as u64, KEEP);
        assert_eq!(s.skipped, Some((101, 1000 - KEEP)));
        t.skip_to(1000 - KEEP, h(1000 - KEEP, 0xa));
        deliver(&mut t, &s);
        // Hashes the store no longer holds aren't evidence of a reorg.
        let s = t.step((1001, h(1001, 0xa)), |n| (n == 1001).then(|| h(n, 0xa)));
        assert_eq!((s.removed, s.added, s.skipped), (vec![], vec![(1001, h(1001, 0xa))], None));
        // Unseeded (nothing was current when it subscribed): the first head only, no gap.
        let s = HeadTrack::default().step((5, h(5, 0xa)), a);
        assert_eq!((s.added.len(), s.skipped), (1, None));
    }
}
