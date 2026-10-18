//! What each EL peer turned out to be worth: SNAP support, how long sessions lasted, how often
//! it dropped us for being full. Ranks peers for the saved list (restart re-dials the best
//! first) and summarizes acquisition for metrics: how long the first SNAP peer took, which
//! clients connect, why sessions end.
//!
//! Measured on Sepolia: about half of all sessions are crawlers, reth peers don't offer SNAP,
//! and about half of the SNAP-capable peers drop us within a minute because they are full. A
//! peer that held a SNAP session before is the best bet next time; one that only ever dropped
//! us is not worth a saved slot.

use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    time::{Duration, Instant},
};

use reth_network_api::PeerId;

/// Peers remembered at most (each session with a new peer adds one; crawlers churn through many).
pub const MAX_RECORDS: usize = 4096;

/// A session that lasted this long counts as "held".
pub const HELD: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default)]
pub struct Record {
    pub client: String,
    pub snap: bool,
    /// Listen address (known for sessions we dialed).
    pub addr: Option<SocketAddr>,
    pub sessions: u32,
    pub longest: Duration,
    /// Sessions the peer ended because it was full (`TooManyPeers`).
    pub full: u32,
}

impl Record {
    /// Higher is better; <= 0 means not worth saving (crawlers, peers that only dropped us).
    pub fn score(&self) -> i32 {
        if self.client == "enrscout" {
            return 0;
        }
        let mut s = 1;
        if self.snap {
            s += 4;
        }
        if self.longest >= HELD {
            s += 3;
        }
        s - 2 * self.full.min(3) as i32
    }
}

#[derive(Debug, Default)]
pub struct PeerBook {
    started: Option<Instant>,
    records: HashMap<PeerId, Record>,
    sessions: BTreeMap<(String, bool), u64>,
    closes: BTreeMap<String, u64>,
    first_snap: Option<Duration>,
}

/// Acquisition so far (for metrics and status).
#[derive(Debug, Clone, Default)]
pub struct BookSummary {
    /// Sessions by (client, offers SNAP).
    pub sessions: Vec<(String, bool, u64)>,
    /// Session ends by reason.
    pub closes: Vec<(String, u64)>,
    /// Time from start to the first SNAP-capable session.
    pub first_snap: Option<Duration>,
    /// SNAP-capable peers seen, and how many of them held a session for [`HELD`].
    pub snap_peers_seen: usize,
    pub snap_peers_held: usize,
}

pub fn client_kind(client: &str) -> String {
    client.split('/').next().unwrap_or("").to_string()
}

impl PeerBook {
    pub fn start(&mut self, now: Instant) {
        self.started.get_or_insert(now);
    }

    pub fn on_session(&mut self, id: PeerId, client: &str, snap: bool, now: Instant) {
        let kind = client_kind(client);
        *self.sessions.entry((kind.clone(), snap)).or_default() += 1;
        if snap && kind != "enrscout" && self.first_snap.is_none() {
            self.first_snap = self.started.map(|s| now.saturating_duration_since(s));
        }
        if self.records.len() >= MAX_RECORDS && !self.records.contains_key(&id) {
            self.trim();
        }
        let r = self.records.entry(id).or_default();
        r.client = kind;
        r.snap |= snap;
        r.sessions += 1;
    }

    pub fn on_addr(&mut self, id: PeerId, addr: SocketAddr) {
        self.records.entry(id).or_default().addr = Some(addr);
    }

    /// `reason` as reth reports it, e.g. "TooManyPeers"; `None` when the session just ended.
    pub fn on_close(&mut self, id: PeerId, up: Duration, reason: Option<&str>) {
        let reason = reason.unwrap_or("none").to_string();
        *self.closes.entry(reason.clone()).or_default() += 1;
        let r = self.records.entry(id).or_default();
        r.longest = r.longest.max(up);
        if reason == "TooManyPeers" {
            r.full += 1;
        }
    }

    /// Bounds the history: peers not worth a saved slot (crawlers, ones that only dropped us) go
    /// first, then the lowest-ranked. Aggregate counts (sessions, closes) are kept as they are.
    fn trim(&mut self) {
        self.records.retain(|_, r| r.score() > 0);
        if self.records.len() >= MAX_RECORDS {
            let mut v: Vec<(PeerId, i32)> = self.records.iter().map(|(id, r)| (*id, r.score())).collect();
            v.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
            for (id, _) in v.into_iter().take(MAX_RECORDS / 4) {
                self.records.remove(&id);
            }
        }
    }

    /// Dialable peers worth saving, best first.
    pub fn ranked(&self) -> Vec<(PeerId, SocketAddr, i32)> {
        let mut v: Vec<(PeerId, SocketAddr, i32)> =
            self.records.iter().filter_map(|(id, r)| Some((*id, r.addr?, r.score()))).filter(|(_, _, s)| *s > 0).collect();
        v.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
        v
    }

    /// Peers seen this run that are not worth a saved slot (dropped from an older saved list).
    pub fn cooled(&self) -> Vec<PeerId> {
        self.records.iter().filter(|(_, r)| r.score() <= 0).map(|(id, _)| *id).collect()
    }

    pub fn summary(&self) -> BookSummary {
        let snap: Vec<&Record> = self.records.values().filter(|r| r.snap && r.client != "enrscout").collect();
        BookSummary {
            sessions: self.sessions.iter().map(|((c, s), n)| (c.clone(), *s, *n)).collect(),
            closes: self.closes.iter().map(|(r, n)| (r.clone(), *n)).collect(),
            first_snap: self.first_snap,
            snap_peers_seen: snap.len(),
            snap_peers_held: snap.iter().filter(|r| r.longest >= HELD).count(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(b: u8) -> PeerId {
        PeerId::repeat_byte(b)
    }

    #[test]
    fn held_snap_peers_rank_first_and_full_ones_cool_off() {
        let mut b = PeerBook::default();
        let t0 = Instant::now();
        b.start(t0);
        let addr: SocketAddr = "1.2.3.4:30303".parse().unwrap();
        // A Geth peer that held a session, a Nethermind peer that kept dropping us (full), a
        // reth peer (no SNAP), and a crawler.
        for _ in 0..3 {
            b.on_session(id(2), "Nethermind/v2", true, t0 + Duration::from_secs(5));
            b.on_close(id(2), Duration::from_secs(1), Some("TooManyPeers"));
        }
        b.on_session(id(1), "Geth/v1.17", true, t0 + Duration::from_secs(40));
        b.on_close(id(1), Duration::from_secs(300), None);
        b.on_session(id(3), "reth/v2.7", false, t0);
        b.on_session(id(4), "enrscout", true, t0);
        for i in 1..=4 {
            b.on_addr(id(i), addr);
        }
        let ranked: Vec<PeerId> = b.ranked().into_iter().map(|(p, _, _)| p).collect();
        assert_eq!(ranked, vec![id(1), id(3)], "held SNAP peer first; full and crawler peers cooled");
        assert!(b.cooled().contains(&id(2)) && b.cooled().contains(&id(4)));
        let s = b.summary();
        assert_eq!(s.first_snap, Some(Duration::from_secs(5)), "first SNAP-capable session, crawlers excluded");
        assert_eq!((s.snap_peers_seen, s.snap_peers_held), (2, 1));
        assert!(s.closes.contains(&("TooManyPeers".to_string(), 3)));
    }

    #[test]
    fn crawler_churn_doesnt_grow_the_book_or_evict_good_peers() {
        let mut b = PeerBook::default();
        let t0 = Instant::now();
        b.start(t0);
        b.on_session(id(1), "Geth/v1.17", true, t0);
        b.on_addr(id(1), "1.2.3.4:30303".parse().unwrap());
        b.on_close(id(1), Duration::from_secs(300), None);
        // Far more distinct crawlers than the book keeps.
        for i in 0..(MAX_RECORDS as u32 * 3) {
            let mut raw = [0u8; 64];
            raw[..4].copy_from_slice(&(i + 1000).to_be_bytes());
            b.on_session(PeerId::from(raw), "enrscout", false, t0);
        }
        assert!(b.records.len() <= MAX_RECORDS, "{} records", b.records.len());
        assert_eq!(b.ranked().first().map(|r| r.0), Some(id(1)), "the held SNAP peer survives");
        let crawler_sessions: u64 = b.summary().sessions.iter().filter(|(c, _, _)| c == "enrscout").map(|(_, _, n)| n).sum();
        assert_eq!(crawler_sessions, MAX_RECORDS as u64 * 3, "aggregate counts are kept");
    }
}
