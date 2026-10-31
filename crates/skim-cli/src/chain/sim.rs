//! A deterministic network simulation for [`Chain::place`]: peer-timing bugs in chain placement
//! only showed under realistic timing (a forged far-ahead envelope becoming the head after an
//! immediate re-anchor; corroboration racing the import of the block just gossiped), so this
//! drives `place` through whole scenarios on a virtual clock.
//!
//! - **Chain:** a canonical chain with honest reorgs: an orphan branch gossiped on time, the
//!   canonical blocks of the same heights gossiped late, then the canonical chain overtakes.
//! - **EL peers:** each imports every honest block after its own delay (100 ms .. 1 s after
//!   gossip); its chain is the highest block it has imported (ties: the one imported first). They
//!   answer [`Chain::peers_know`] by hash from what they've imported, and headers by number from
//!   their chain, BALs by hash ([`ElFetch`]). A lying peer also "has" a forged chain.
//! - **Consensus peers:** an [`EnvelopeSource`](crate::persist::EnvelopeSource) serving its own
//!   (delayed) view of the chain by slot.
//! - **Gossip to the follower:** honest blocks with a small delay, some dropped (gaps), some
//!   late; adversarial envelopes (far ahead, a real parent at an invented height, siblings,
//!   replayed orphans, the tip of a lying peer's forged chain).
//!
//! The clock moves only between events (every simulated answer is immediate), so runs are
//! deterministic per seed. After each scenario: the head is the canonical tip, every stored
//! block is canonical, no honest block dropped a canonical block, no single adversarial
//! envelope dropped more than one (the one-block reach of a sibling), and none re-anchored.

use super::tests::{GENESIS, SLOT0, envelope};
use super::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::time::Duration;

const SLOT_MS: u64 = 12_000;
/// The first block of every scenario (the follower's starting head unless it starts elsewhere).
const BASE: u64 = 100;

/// When block `number`'s slot starts, in ms (blocks sit in slot `SLOT0 + number`).
fn slot_ms(number: u64) -> u64 {
    (GENESIS + (SLOT0 + number) * 12) * 1000
}

/// splitmix64: deterministic and dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }
    fn pct(&mut self, p: u64) -> bool {
        self.next() % 100 < p
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Canonical,
    /// Honest, but reorged out (or replayed late).
    Orphan,
    Forged,
}

struct Block {
    header: Header,
    hash: B256,
    env: Vec<u8>,
    kind: Kind,
}

struct SimPeer {
    id: skim_el::Peer,
    /// Block hash -> when this peer imported it (ms).
    import: HashMap<B256, u64>,
}

/// Everything the follower's peers know, fixed up front; [`Net::now`] decides what's visible.
struct Net {
    now: AtomicU64,
    blocks: Vec<Block>,
    by_hash: HashMap<B256, usize>,
    el: Vec<SimPeer>,
    cl: SimPeer,
}

impl Net {
    /// The peer's chain at the current time: number -> block index.
    fn view(&self, p: &SimPeer) -> BTreeMap<u64, usize> {
        let now = self.now.load(SeqCst);
        let head = p
            .import
            .iter()
            .filter(|(_, t)| **t <= now)
            .map(|(h, t)| (self.by_hash[h], *t))
            .max_by_key(|(i, t)| (self.blocks[*i].header.number, std::cmp::Reverse(*t)));
        let mut out = BTreeMap::new();
        let mut cur = head.map(|(i, _)| i);
        while let Some(i) = cur {
            out.insert(self.blocks[i].header.number, i);
            cur = self.by_hash.get(&self.blocks[i].header.parent_hash).copied().filter(|j| p.import.get(&self.blocks[*j].hash).is_some_and(|t| *t <= now));
        }
        out
    }

    fn has(&self, p: &SimPeer, hash: &B256) -> bool {
        p.import.get(hash).is_some_and(|t| *t <= self.now.load(SeqCst))
    }
}

struct SimEl(Arc<Net>);

impl ElFetch for SimEl {
    fn bal_peers(&self) -> Vec<skim_el::Peer> {
        self.0.el.iter().map(|p| p.id).collect()
    }
    fn fetch_headers(&self, peer: skim_el::Peer, start: u64, limit: u64) -> futures::future::BoxFuture<'_, Option<Vec<Header>>> {
        let net = &self.0;
        let p = net.el.iter().find(|p| p.id == peer).expect("known peer");
        let view = net.view(p);
        let hs = (start..start + limit).map_while(|n| view.get(&n).map(|i| net.blocks[*i].header.clone())).collect();
        Box::pin(async move { Some(hs) })
    }
    fn fetch_bals(&self, peer: skim_el::Peer, hashes: Vec<B256>) -> futures::future::BoxFuture<'_, Option<Vec<Option<alloy_primitives::Bytes>>>> {
        let net = &self.0;
        let p = net.el.iter().find(|p| p.id == peer).expect("known peer");
        // Every simulated block has the empty BAL.
        let out = hashes.iter().map(|h| net.has(p, h).then(|| alloy_primitives::Bytes::from_static(&[0xc0]))).collect();
        Box::pin(async move { Some(out) })
    }
}

struct SimCl(Arc<Net>);

impl crate::persist::EnvelopeSource for SimCl {
    fn envelopes_by_range_from(
        &self,
        start_slot: u64,
        count: u64,
        _timeout: Duration,
    ) -> futures::future::BoxFuture<'_, (Option<String>, anyhow::Result<Vec<Vec<u8>>>)> {
        let net = &self.0;
        let view = net.view(&net.cl);
        let lo = start_slot.saturating_sub(SLOT0);
        let hi = (start_slot + count).saturating_sub(SLOT0);
        let out = view.range(lo..hi).map(|(_, i)| net.blocks[*i].env.clone()).collect();
        Box::pin(async move { (Some("sim".into()), Ok(out)) })
    }
}

fn sim_peers_know(net: Arc<Net>) -> PeersKnow {
    Arc::new(move |hash, number| {
        let c = match net.by_hash.get(&hash) {
            Some(i) if net.blocks[*i].header.number == number => net.el.iter().filter(|p| net.has(p, &hash)).count(),
            _ => 0,
        };
        Box::pin(async move { c })
    })
}

#[derive(Clone, Copy)]
struct Cfg {
    seed: u64,
    /// Canonical blocks after [`BASE`].
    blocks: u64,
    honest_peers: usize,
    /// A peer that also serves a forged chain (and vouches for it).
    liar: bool,
    reorg_pct: u64,
    /// Gossip the follower misses (gaps), or gets a few slots late.
    drop_pct: u64,
    late_pct: u64,
    /// Adversarial envelopes per slot.
    attack_pct: u64,
    /// The follower's starting head: a forged block far ahead instead of `BASE`.
    start_forged: Option<StartForged>,
    /// Blocks the follower misses after `BASE` (it was offline).
    offline: u64,
    /// Gossip reaches the follower within 20 ms (before any peer imports it, 100 ms+), and an
    /// orphan sibling of each block's parent is replayed 30 ms behind it.
    orphan_race: bool,
}

#[derive(Clone, Copy)]
enum StartForged {
    /// Parent and height both made up.
    Unrelated,
    /// A real parent (`BASE`), a made-up height.
    RealParent,
    /// A forged chain the lying peer serves (it vouches for the head's parent).
    LiarChain,
}

impl Cfg {
    fn new(seed: u64) -> Self {
        Cfg { seed, blocks: 30, honest_peers: 4, liar: false, reorg_pct: 0, drop_pct: 0, late_pct: 0, attack_pct: 0, start_forged: None, offline: 0, orphan_race: false }
    }
}

struct Event {
    at: u64,
    block: usize,
    what: &'static str,
}

/// Builds the blocks, every peer's import times, and the follower's gossip, all from the seed.
struct Builder {
    cfg: Cfg,
    rng: Rng,
    blocks: Vec<Block>,
    by_hash: HashMap<B256, usize>,
    el: Vec<SimPeer>,
    liar: Option<SimPeer>,
    cl: SimPeer,
    events: Vec<Event>,
    salt: u8,
}

impl Builder {
    fn new(cfg: Cfg) -> Self {
        let peer = |i: u8| SimPeer { id: skim_el::Peer::repeat_byte(i), import: HashMap::new() };
        Builder {
            rng: Rng(cfg.seed.wrapping_mul(0x2545_f491_4f6c_dd1d) ^ 0x5eed),
            blocks: Vec::new(),
            by_hash: HashMap::new(),
            el: (1..=cfg.honest_peers as u8).map(peer).collect(),
            liar: cfg.liar.then(|| peer(0xee)),
            cl: peer(0xc1),
            events: Vec::new(),
            salt: 0x20,
            cfg,
        }
    }

    fn next_salt(&mut self) -> u8 {
        self.salt = if self.salt == 0xff { 0x20 } else { self.salt + 1 };
        self.salt
    }

    /// Adds a block gossiped at `gossip`: honest peers (and the consensus peer) import honest
    /// blocks after their delays; the liar "has" its forged blocks from the start.
    fn add(&mut self, parent: B256, number: u64, salt: u8, kind: Kind, gossip: u64, liar_has: bool) -> Option<usize> {
        let (header, env) = envelope(parent, number, salt);
        let hash = header.hash_slow();
        if self.by_hash.contains_key(&hash) {
            return None;
        }
        let i = self.blocks.len();
        if kind != Kind::Forged {
            for p in self.el.iter_mut().chain(self.liar.iter_mut()).chain(std::iter::once(&mut self.cl)) {
                let d = self.rng.range(100, 1000);
                p.import.insert(hash, gossip + d);
            }
        } else if liar_has && let Some(l) = &mut self.liar {
            l.import.insert(hash, 0);
        }
        self.blocks.push(Block { header, hash, env, kind });
        self.by_hash.insert(hash, i);
        Some(i)
    }

    fn deliver(&mut self, at: u64, block: usize, what: &'static str) {
        self.events.push(Event { at, block, what });
    }

    /// Honest gossip to the follower: usually on time, sometimes missed or late.
    fn gossip(&mut self, at: u64, block: usize, honest_tail: bool) {
        let at = at + self.rng.range(0, if self.cfg.orphan_race { 20 } else { 300 });
        if honest_tail {
            // Delivered on time: the run settles.
        } else if self.blocks[block].header.number <= BASE + self.cfg.offline || self.rng.pct(self.cfg.drop_pct) {
            return;
        } else if self.rng.pct(self.cfg.late_pct) {
            let late = at + self.rng.range(SLOT_MS, 3 * SLOT_MS);
            return self.deliver(late, block, "late");
        }
        self.deliver(at, block, "honest");
    }

    fn build(mut self) -> (Arc<Net>, Vec<Event>, HashSet<B256>, Header, Header) {
        let cfg = self.cfg;
        // History peers had long before the follower's start, so reconciling can look back.
        let mut canon = Vec::new();
        let mut p = B256::repeat_byte(0x99);
        for n in BASE - 16..=BASE {
            let c = self.add(p, n, 0xa, Kind::Canonical, slot_ms(n) + 2000, false).unwrap();
            p = self.blocks[c].hash;
            canon.push(c);
        }
        let base = *canon.last().unwrap();
        let tail_from = BASE + cfg.blocks - 2; // the last blocks: honest traffic only, so it settles
        let mut n = BASE + 1;
        let mut orphans: Vec<usize> = Vec::new();
        while n <= BASE + cfg.blocks {
            let tip = *canon.last().unwrap();
            let tip_hash = self.blocks[tip].hash;
            let honest_tail = n >= tail_from;
            // An honest reorg: an orphan branch first, the canonical blocks of the same heights late.
            let depth = if !honest_tail && self.rng.pct(cfg.reorg_pct) { self.rng.range(1, 3).min(tail_from - n) } else { 0 };
            if depth > 0 {
                let mut p = tip_hash;
                let salt = self.next_salt();
                for k in 0..depth {
                    let at = slot_ms(n + k) + 2000;
                    let o = self.add(p, n + k, salt, Kind::Orphan, at, false).unwrap();
                    p = self.blocks[o].hash;
                    orphans.push(o);
                    self.gossip(at, o, false);
                }
                // The canonical blocks of those heights, in a burst mid-slot of the last one.
                let burst = slot_ms(n + depth - 1) + 6000;
                for k in 0..depth {
                    let c = self.add(self.blocks[*canon.last().unwrap()].hash, n + k, 0xa, Kind::Canonical, burst + k * 10, false).unwrap();
                    canon.push(c);
                    self.gossip(burst + k * 10, c, false);
                }
                n += depth;
                continue;
            }
            let at = slot_ms(n) + 2000;
            let c = self.add(tip_hash, n, 0xa, Kind::Canonical, at, false).unwrap();
            canon.push(c);
            self.gossip(at, c, honest_tail);
            if cfg.orphan_race && !honest_tail && canon.len() >= 3 {
                let p = canon[canon.len() - 2];
                let (pp, pn) = (self.blocks[p].header.parent_hash, self.blocks[p].header.number);
                let salt = self.next_salt();
                if let Some(o) = self.add(pp, pn, salt, Kind::Orphan, slot_ms(pn) + 2500, false) {
                    self.deliver(at + 30, o, "replayed orphan (race)");
                }
            }
            if !honest_tail && n > BASE + cfg.offline && self.rng.pct(cfg.attack_pct) {
                self.attack(n, &canon, &orphans);
            }
            n += 1;
        }
        let canonical: HashSet<B256> = canon.iter().map(|i| self.blocks[*i].hash).collect();
        let start = match cfg.start_forged {
            None => self.blocks[base].header.clone(),
            Some(how) => {
                let parent = match how {
                    StartForged::Unrelated => B256::repeat_byte(0x66),
                    StartForged::RealParent => self.blocks[base].hash,
                    StartForged::LiarChain => {
                        let p = self.add(B256::repeat_byte(0x66), BASE + 4999, 0xf, Kind::Forged, 0, true).unwrap();
                        self.blocks[p].hash
                    }
                };
                let f = self.add(parent, BASE + 5000, 0xf, Kind::Forged, 0, true).unwrap();
                self.blocks[f].header.clone()
            }
        };
        let tip = self.blocks[*canon.last().unwrap()].header.clone();
        let mut el = self.el;
        el.extend(self.liar);
        let net = Arc::new(Net { now: AtomicU64::new(0), blocks: self.blocks, by_hash: self.by_hash, el, cl: self.cl });
        self.events.sort_by_key(|e| e.at); // stable: same-time events keep their order
        (net, self.events, canonical, start, tip)
    }

    /// One adversarial envelope during slot `n` (canonical up to `canon.last()`, number `n`).
    fn attack(&mut self, n: u64, canon: &[usize], orphans: &[usize]) {
        let at = slot_ms(n) + self.rng.range(0, SLOT_MS - 1);
        let salt = self.next_salt();
        let pick = |rng: &mut Rng, back: u64| canon[canon.len() - 1 - rng.range(0, back.min(canon.len() as u64 - 1)) as usize];
        match self.rng.range(0, if self.liar.is_some() { 4 } else { 3 }) {
            0 => {
                // Far ahead, made-up parent.
                let parent = B256::repeat_byte(self.rng.range(1, 255) as u8);
                let far = n + self.rng.range(2, 6000);
                if let Some(f) = self.add(parent, far, salt, Kind::Forged, at, false) {
                    self.deliver(at, f, "forged far ahead");
                }
            }
            1 => {
                // A real parent at an invented height.
                let p = pick(&mut self.rng, 6);
                let pn = self.blocks[p].header.number;
                let number = pn + 1 + self.rng.range(1, 2000);
                if let Some(f) = self.add(self.blocks[p].hash, number, salt, Kind::Forged, at, false) {
                    self.deliver(at, f, "forged invented height");
                }
            }
            2 => {
                // A sibling of a recent canonical block (a real parent at the real height).
                let c = pick(&mut self.rng, 3);
                let (parent, number) = (self.blocks[c].header.parent_hash, self.blocks[c].header.number);
                if let Some(f) = self.add(parent, number, salt, Kind::Forged, at, false) {
                    self.deliver(at, f, "forged sibling");
                }
            }
            3 => {
                // A replayed orphan, or a fresh one under an older canonical block.
                if !orphans.is_empty() && self.rng.pct(50) {
                    let o = orphans[self.rng.range(0, orphans.len() as u64 - 1) as usize];
                    self.deliver(at, o, "replayed orphan");
                } else {
                    let c = pick(&mut self.rng, 8);
                    let (parent, number) = (self.blocks[c].header.parent_hash, self.blocks[c].header.number);
                    // Honest: peers saw it in its slot (before any of this run's queries).
                    if let Some(o) = self.add(parent, number, salt, Kind::Orphan, slot_ms(number) + 2500, false) {
                        self.deliver(at, o, "replayed orphan");
                    }
                }
            }
            _ => {
                // A lying peer's forged chain off a recent canonical block; its tip by gossip.
                let mut p = self.blocks[pick(&mut self.rng, 4)].hash;
                let from = self.blocks[self.by_hash[&p]].header.number + 1;
                let len = self.rng.range(1, 40);
                for k in 0..len {
                    match self.add(p, from + k, salt, Kind::Forged, 0, true) {
                        Some(f) => p = self.blocks[f].hash,
                        None => return,
                    }
                }
                // The tip itself: also served by the liar (it vouches for its own chain).
                if let Some(f) = self.add(p, from + len, salt, Kind::Forged, at, true) {
                    self.deliver(at, f, "forged tip of a liar's chain");
                }
            }
        }
    }
}

/// Runs one scenario and checks the invariants; panics with the event trace on a violation.
async fn run(cfg: Cfg) -> Vec<String> {
    let (net, events, canonical, start, tip) = Builder::new(cfg).build();
    let store = Store::new(64);
    store.apply_block(block_ref(&start), Default::default()).unwrap();
    // Every rollback: (to, canonical blocks it dropped).
    let rollbacks: Arc<Mutex<Vec<(u64, usize)>>> = Default::default();
    let (rb, st, canon) = (rollbacks.clone(), store.clone(), canonical.clone());
    let chain = Chain {
        store: store.clone(),
        el: Arc::new(SimEl(net.clone())),
        on_header: Box::new(|_| {}),
        on_applied: Box::new(|_| {}),
        on_rollback: Box::new(move |to| {
            // Called before the store rolls back: it still holds what's dropped.
            let head = st.head().map_or(0, |h| h.number);
            let dropped = (to + 1..=head).filter(|n| st.block(*n).is_some_and(|b| canon.contains(&b.hash))).count();
            rb.lock().unwrap().push((to, dropped));
        }),
        lookback: 8,
        cl: Some(Arc::new(SimCl(net.clone()))),
        peers_know: sim_peers_know(net.clone()),
    };
    let mut trace = Vec::new();
    let mut violations = Vec::new();
    for e in &events {
        net.now.store(e.at, SeqCst);
        let b = &net.blocks[e.block];
        let before = rollbacks.lock().unwrap().len();
        let prev_head = store.head().unwrap();
        let placed = chain.place(b.header.clone(), Default::default()).await;
        let head = store.head().unwrap();
        let head_kind = net.by_hash.get(&head.hash).map(|i| net.blocks[*i].kind);
        let short = match &placed {
            Placed::Applied { reorged_to, filled, .. } => format!("applied (reorged_to {reorged_to:?}, filled {filled})"),
            Placed::Duplicate => "duplicate".into(),
            Placed::Deferred(why) => format!("deferred: {why}"),
            Placed::Reanchored(_) => "re-anchored".into(),
        };
        let dropped: Vec<(u64, usize)> = rollbacks.lock().unwrap()[before..].to_vec();
        let reanchored = matches!(placed, Placed::Reanchored(_));
        trace.push(format!(
            "t={:>6}ms {:<28} #{} {:?} {:x}.. -> {short}; rollbacks {dropped:?}; head #{} {:?}",
            e.at - slot_ms(BASE),
            e.what,
            b.header.number,
            b.kind,
            b.hash.0[0],
            head.number,
            head_kind
        ));
        let lost: usize = dropped.iter().map(|d| d.1).sum();
        match b.kind {
            // A fresh start forgets everything; only when the gap is over the replay limit or
            // the head was on no peer's chain.
            Kind::Canonical if reanchored => {
                if prev_head.number + MAX_RECONCILE >= b.header.number && canonical.contains(&prev_head.hash) {
                    violations.push(format!("canonical #{} re-anchored from canonical #{}", b.header.number, prev_head.number));
                }
            }
            // Honest canonical blocks only ever drop non-canonical ones.
            Kind::Canonical if lost > 0 => violations.push(format!("canonical #{} dropped {lost} canonical blocks", b.header.number)),
            // One envelope: at most the one-block reach of a sibling, and never a fresh start.
            Kind::Orphan | Kind::Forged if lost > 1 => violations.push(format!("{} #{} dropped {lost} canonical blocks", e.what, b.header.number)),
            Kind::Orphan | Kind::Forged if reanchored && cfg.start_forged.is_none() => {
                violations.push(format!("{} #{} re-anchored the follower", e.what, b.header.number))
            }
            _ => {}
        }
    }
    let head = store.head().unwrap();
    if head.hash != tip.hash_slow() {
        violations.push(format!("ended at #{} {:x}.., not the canonical tip #{}", head.number, head.hash.0[0], tip.number));
    }
    for n in head.number.saturating_sub(63)..=head.number {
        if let Some(b) = store.block(n)
            && !canonical.contains(&b.hash)
        {
            violations.push(format!("stored block #{n} is not canonical"));
            break;
        }
    }
    if !violations.is_empty() {
        panic!("seed {}: {violations:#?}\ntrace:\n{}", cfg.seed, trace.join("\n"));
    }
    trace
}

#[tokio::test]
async fn honest_traffic_with_reorgs_gaps_and_late_blocks() {
    let mut lines = Vec::new();
    for seed in 0..50 {
        lines.extend(run(Cfg { reorg_pct: 15, drop_pct: 10, late_pct: 10, ..Cfg::new(seed) }).await);
    }
    // The scenarios exercise what they're meant to: reorgs, gap fills, deferrals.
    let count = |pat: &str| lines.iter().filter(|l| l.contains(pat)).count();
    assert!(count("reorged_to Some") > 20 && count("deferred") > 20, "{} reorgs, {} deferrals", count("reorged_to Some"), count("deferred"));
    assert!(lines.iter().any(|l| l.contains("filled") && !l.contains("filled 0")), "no gap was filled");
}

#[tokio::test]
async fn forged_and_replayed_envelopes_among_honest_traffic() {
    for seed in 0..50 {
        run(Cfg { reorg_pct: 10, drop_pct: 5, late_pct: 5, attack_pct: 40, ..Cfg::new(seed) }).await;
    }
}

#[tokio::test]
async fn a_lying_el_peer_with_forged_envelopes() {
    for seed in 0..50 {
        run(Cfg { liar: true, reorg_pct: 10, drop_pct: 5, late_pct: 5, attack_pct: 40, ..Cfg::new(seed) }).await;
    }
}

#[tokio::test]
async fn only_consensus_peers_fill_gaps_and_reorgs() {
    for seed in 0..30 {
        run(Cfg { honest_peers: 0, reorg_pct: 10, drop_pct: 10, late_pct: 5, attack_pct: 30, ..Cfg::new(seed) }).await;
    }
}

#[tokio::test]
async fn after_a_long_offline_gap_the_follower_re_anchors() {
    // Offline for more than the replay limit; then honest gossip with forged envelopes mixed in.
    for seed in 0..3 {
        let trace = run(Cfg { blocks: MAX_RECONCILE + 40, offline: MAX_RECONCILE + 20, attack_pct: 30, ..Cfg::new(seed) }).await;
        assert!(trace.iter().any(|l| l.contains("re-anchored")), "seed {seed}: {trace:#?}");
    }
}

#[tokio::test]
async fn started_on_a_forged_head_the_follower_returns_to_the_chain() {
    for seed in 0..20 {
        for how in [StartForged::Unrelated, StartForged::RealParent] {
            run(Cfg { start_forged: Some(how), reorg_pct: 10, drop_pct: 5, attack_pct: 20, ..Cfg::new(seed) }).await;
        }
    }
    // With a lying peer, and one that vouches for the forged head.
    for seed in 0..20 {
        for how in [StartForged::Unrelated, StartForged::LiarChain] {
            run(Cfg { start_forged: Some(how), liar: true, reorg_pct: 10, attack_pct: 20, ..Cfg::new(seed) }).await;
        }
    }
}

#[tokio::test]
async fn a_replayed_orphan_racing_the_import_of_a_fresh_head() {
    // Third review, with timing: every block reaches the follower before any peer imports it,
    // and a sibling of the head's parent (which peers saw in its slot) is replayed right behind
    // it. Asking peers about the fresh head itself made it look made up.
    for seed in 0..30 {
        run(Cfg { orphan_race: true, reorg_pct: 10, ..Cfg::new(seed) }).await;
    }
}
