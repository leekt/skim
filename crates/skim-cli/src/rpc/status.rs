//! `skim_status`, `skim_dashboard` and the Prometheus gauges that mirror live state.

use super::*;

impl Rpc {
    /// Refreshes the gauges and totals that mirror live state, right before a scrape.
    pub fn refresh_metrics(&self, top_addresses: usize) {
        use metrics::{counter, gauge};
        let sim = &self.sim;
        gauge!("skim_heap_live_bytes").set(crate::heap::live() as f64);
        for p in self.memory_parts() {
            gauge!("skim_memory_bytes", "part" => p.part).set(p.bytes as f64);
            gauge!("skim_memory_entries", "part" => p.part).set(p.entries as f64);
            gauge!("skim_memory_capacity", "part" => p.part).set(p.capacity as f64);
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        if let Some(h) = sim.store.head() {
            gauge!("skim_head_block").set(h.number as f64);
            gauge!("skim_head_age_seconds").set(now - h.timestamp as f64);
        }
        let peers = sim.sched.transport().net.peers();
        gauge!("skim_peers", "layer" => "el").set(peers.len() as f64);
        for (kind, n) in skim_el::requests::counts() {
            counter!("skim_peer_requests_total", "kind" => kind).absolute(n);
        }
        gauge!("skim_snap_peers").set(peers.iter().filter(|(_, p)| p.snap1 || p.snap2).count() as f64);
        for c in crate::dashboard::by_kind(peers.iter().map(|(_, p)| p.client.as_str())) {
            gauge!("skim_peer_clients", "layer" => "el", "client" => c["client"].as_str().unwrap_or("").to_string()).set(c["count"].as_f64().unwrap_or(0.0));
        }
        let fetch = sim.sched.peer_stats();
        for (id, p) in &peers {
            let Some(f) = fetch.get(id) else { continue };
            let labels = [("peer", format!("{:.10}", id.to_string())), ("client", crate::dashboard::client_kind(&p.client))];
            if f.ok > 0 {
                gauge!("skim_snap_peer_latency_ms", &labels).set(f.ewma_ms);
            }
            gauge!("skim_snap_peer_inflight", &labels).set(f.inflight as f64);
            counter!("skim_snap_peer_answers_total", &labels).absolute(f.ok);
            counter!("skim_snap_peer_errors_total", &labels).absolute(f.errors);
        }
        let book = sim.sched.transport().net.peer_book();
        for (client, snap, n) in &book.sessions {
            counter!("skim_el_sessions_total", "client" => client.clone(), "snap" => snap.to_string()).absolute(*n);
        }
        for (reason, n) in &book.closes {
            counter!("skim_el_session_closes_total", "reason" => reason.clone()).absolute(*n);
        }
        if let Some(t) = book.first_snap {
            gauge!("skim_el_first_snap_peer_seconds").set(t.as_secs_f64());
        }
        gauge!("skim_el_snap_peers_seen").set(book.snap_peers_seen as f64);
        gauge!("skim_el_snap_peers_held").set(book.snap_peers_held as f64);
        let st = sim.sched.stats();
        for (kind, v) in [
            ("jobs", st.jobs), ("requests", st.requests), ("ok", st.ok), ("errors", st.errors), ("failed", st.failed),
            ("hedges", st.hedges), ("root_unavailable", st.root_unavailable), ("disconnects", st.disconnects), ("dedup_hits", st.dedup_hits),
        ] {
            counter!("skim_snap_fetch_total", "kind" => kind).absolute(v);
        }
        if let Some(s) = &self.cl_stats {
            let s = s.lock().unwrap();
            gauge!("skim_peers", "layer" => "cl").set(s.peer_list.len() as f64);
            for c in crate::dashboard::by_kind(s.peer_list.iter().map(|p| p.agent.as_deref().unwrap_or(""))) {
                gauge!("skim_peer_clients", "layer" => "cl", "client" => c["client"].as_str().unwrap_or("").to_string()).set(c["count"].as_f64().unwrap_or(0.0));
            }
            for (kind, v) in [("envelopes_by_range", s.envelope_range_reqs), ("envelopes_by_root", s.envelope_root_reqs)] {
                counter!("skim_served_requests_total", "layer" => "cl", "kind" => kind).absolute(v);
            }
            counter!("skim_served_items_total", "layer" => "cl", "kind" => "envelopes").absolute(s.envelopes_served);
        }
        let c = sim.sched.transport().net.serve_counts();
        for (kind, reqs, items) in [
            ("headers", c.header_reqs, c.headers), ("bodies", c.body_reqs, c.bodies), ("receipts", c.receipt_reqs, c.receipts),
            ("bals", c.bal_reqs, c.bals), ("snap", c.snap_reqs, c.codes),
        ] {
            counter!("skim_served_requests_total", "layer" => "el", "kind" => kind).absolute(reqs);
            counter!("skim_served_items_total", "layer" => "el", "kind" => kind).absolute(items);
        }
        let z = sim.store.sizes();
        gauge!("skim_store_keys", "kind" => "subscribed").set(z.watched_keys as f64);
        gauge!("skim_store_keys", "kind" => "on_demand").set(z.temp_keys as f64);
        gauge!("skim_store_keys", "kind" => "versions").set(z.versions as f64);
        gauge!("skim_store_bytes", "kind" => "code").set(z.code_bytes as f64);
        gauge!("skim_store_bytes", "kind" => "recent_bals").set(z.recent_bal_bytes as f64);
        counter!("skim_store_admissions_refused_total").absolute(z.admissions_refused);
        counter!("skim_store_on_demand_evicted_total").absolute(z.on_demand_evicted);
        counter!("skim_store_resolved_from_bal_total").absolute(z.resolved_from_bal);
        counter!("skim_store_archive_base_writes_total").absolute(z.archive_base_writes);
        if let Some(subs) = &self.subs {
            for s in subs.status() {
                gauge!("skim_subscription_ready", "name" => s.name.clone()).set(s.ready as u8 as f64);
                gauge!("skim_subscription_missing_keys", "name" => s.name.clone()).set(s.missing_keys as f64);
            }
        }
        if top_addresses > 0 {
            let labels = self.subs.as_ref().map(|s| s.labels()).unwrap_or_default();
            for a in self.dash.addresses_json(&sim.store, &labels).into_iter().take(top_addresses) {
                // Only `address` labels the counters, so a change of subscription
                // never splits an address into two series.
                let addr = a["address"].as_str().unwrap_or("").to_string();
                counter!("skim_address_calls_total", "address" => addr.clone()).absolute(a["calls"].as_u64().unwrap_or(0));
                counter!("skim_address_fetched_keys_total", "address" => addr.clone()).absolute(a["fetched"].as_u64().unwrap_or(0));
                let subscription = match a["status"].as_str() {
                    Some("subscribed") => 2.0,
                    Some("partly subscribed") => 1.0,
                    _ => 0.0,
                };
                gauge!("skim_address_subscription", "address" => addr.clone()).set(subscription);
                gauge!("skim_address_info", "address" => addr.clone(), "subscription" => a["label"].as_str().unwrap_or("").to_string()).set(1.0);
                let slots = a["slots"].as_u64().unwrap_or(0);
                let unsubscribed = a["unsubscribed_slots"].as_u64().unwrap_or(0);
                gauge!("skim_address_slots", "address" => addr.clone(), "subscribed" => "true").set((slots - unsubscribed) as f64);
                gauge!("skim_address_slots", "address" => addr, "subscribed" => "false").set(unsubscribed as f64);
            }
        }
    }

    /// Retained data by structure (entries, estimated payload bytes, capacity): block caches,
    /// state, the call cache, peers and network bookkeeping. Explains part of RSS (payload, not
    /// allocator overhead or code pages); what it doesn't cover needs a heap profile.
    pub fn memory_parts(&self) -> Vec<crate::chaindata::MemPart> {
        use crate::chaindata::MemPart;
        let mut out = self.chain.as_ref().map(|c| c.memory_parts()).unwrap_or_default();
        out.push(self.call_cache.lock().part());
        let z = self.sim.store.sizes();
        out.push(MemPart { part: "state.recent_bal_writes", entries: z.recent_bals, bytes: z.recent_bal_bytes, capacity: z.recent_bals });
        out.push(MemPart { part: "state.versions", entries: z.versions, bytes: z.versions * 40 + z.keys * 96, capacity: z.keys });
        out.push(MemPart { part: "state.code", entries: z.code, bytes: z.code_bytes, capacity: z.code });
        let (addrs, methods) = self.dash.sizes();
        out.push(MemPart { part: "dashboard.addresses", entries: addrs, bytes: addrs * 160, capacity: addrs });
        out.push(MemPart { part: "dashboard.methods", entries: methods, bytes: methods * 1100, capacity: methods });
        let net = &self.sim.sched.transport().net;
        let peers = net.peers().len();
        out.push(MemPart { part: "el.peers", entries: peers, bytes: 0, capacity: peers });
        out.push(MemPart { part: "el.peer_book", entries: net.peer_book().records, bytes: net.peer_book().records * 96, capacity: skim_el::book::MAX_RECORDS });
        let (q, ql) = self.sim.sched.queue_lens();
        out.push(MemPart { part: "snap.queued_jobs", entries: q + ql, bytes: (q + ql) * 200, capacity: q + ql });
        if let Some(s) = &self.cl_stats {
            let s = s.lock().unwrap();
            out.push(MemPart { part: "cl.envelopes_served", entries: s.envelope_cache.0, bytes: s.envelope_cache.1, capacity: skim_cl::serve::SERVE_SLOTS });
            out.push(MemPart { part: "cl.peers", entries: s.connected, bytes: 0, capacity: s.connected });
            out.push(MemPart { part: "cl.seen_nodes", entries: s.seen_nodes, bytes: s.seen_nodes * 48, capacity: s.seen_nodes });
            out.push(MemPart { part: "cl.dial_records", entries: s.dial_records, bytes: s.dial_records * 56, capacity: s.dial_records });
        }
        out
    }

    /// `skim_dashboard`: peers, latency, RPC and address usage in one document.
    pub(super) fn dashboard_json(&self) -> Value {
        use crate::dashboard::{by_kind, client_kind, round2};
        let sim = &self.sim;
        let head = sim.store.head();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let peers = sim.sched.transport().net.peers();
        let fetch = sim.sched.peer_stats();
        let mut el: Vec<Value> = peers
            .iter()
            .map(|(id, p)| {
                let f = fetch.get(id);
                json!({
                    "id": format!("{:.10}", id.to_string()),
                    "client": client_kind(&p.client),
                    "version": p.client.split('/').nth(1).unwrap_or(""),
                    "eth": format!("eth/{}", p.eth_version),
                    "snap": if p.snap2 { "snap/2" } else if p.snap1 { "snap/1" } else { "none" },
                    "age_min": round2(p.connected_at.elapsed().as_secs_f64() / 60.0),
                    "snap_ok": f.map_or(0, |f| f.ok),
                    "snap_errors": f.map_or(0, |f| f.errors),
                    "ewma_ms": f.map_or(Value::Null, |f| json!(round2(f.ewma_ms))),
                    "min_ms": f.map_or(Value::Null, |f| json!(round2(f.min_ms))),
                    "inflight": f.map_or(0, |f| f.inflight),
                })
            })
            .collect();
        el.sort_by(|a, b| (a["snap"] == "none").cmp(&(b["snap"] == "none")).then(b["snap_ok"].as_u64().cmp(&a["snap_ok"].as_u64())));
        let (cl, cl_mesh) = match &self.cl_stats {
            Some(s) => {
                let s = s.lock().unwrap();
                let mut v: Vec<Value> = s
                    .peer_list
                    .iter()
                    .map(|p| {
                        let agent = p.agent.clone().unwrap_or_default();
                        json!({
                            "id": format!("{:.12}", p.id),
                            "client": client_kind(&agent),
                            "version": agent.split('/').nth(1).unwrap_or(""),
                            "direction": if p.outbound { "outbound" } else { "inbound" },
                            "age_min": round2(p.age_s / 60.0),
                            "gossip_first": p.gossip_first,
                            "head_slot": p.head_slot,
                        })
                    })
                    .collect();
                v.sort_by(|a, b| b["gossip_first"].as_u64().cmp(&a["gossip_first"].as_u64()));
                (v, json!(s.mesh))
            }
            None => (vec![], json!({})),
        };
        let blocks = self.dash.blocks();
        let mut arrivals: Vec<f64> = blocks.iter().map(|b| b.arrival_ms).filter(|x| x.is_finite()).collect();
        arrivals.sort_by(f64::total_cmp);
        let q = |p: f64| arrivals.get(((arrivals.len() as f64 - 1.0) * p).round() as usize).copied().map(round2);
        let labels = self.subs.as_ref().map(|s| s.labels()).unwrap_or_default();
        let addresses = self.dash.addresses_json(&sim.store, &labels);
        let unsubscribed = addresses.iter().filter(|a| a["status"] != "subscribed").count();
        let z = sim.store.sizes();
        let el_kinds: Vec<&str> = peers.iter().map(|(_, p)| p.client.as_str()).collect();
        json!({
            "updated": now,
            "summary": {
                "head": head.map(|h| h.number),
                "head_age_s": head.map(|h| round2(now - h.timestamp as f64)),
                "uptime_min": self.dash.uptime_s() / 60,
                "el_peers": peers.len(),
                "snap_peers": peers.iter().filter(|(_, p)| p.snap1 || p.snap2).count(),
                "cl_peers": cl.len(),
                "subscribed_keys": z.watched_keys,
                "on_demand_keys": z.temp_keys,
                "addresses_used": addresses.len(),
                "addresses_not_subscribed": unsubscribed,
                "arrival_p50_ms": q(0.5),
                "arrival_p90_ms": q(0.9),
            },
            "el": { "peers": el, "clients": by_kind(el_kinds.into_iter()) },
            "cl": { "peers": cl.clone(), "clients": by_kind(cl.iter().map(|p| p["client"].as_str().unwrap_or(""))), "mesh": cl_mesh },
            "blocks": {
                "numbers": blocks.iter().map(|b| b.number).collect::<Vec<_>>(),
                "arrival_ms": blocks.iter().map(|b| if b.arrival_ms.is_finite() { json!(round2(b.arrival_ms)) } else { Value::Null }).collect::<Vec<_>>(),
                "changed": blocks.iter().map(|b| b.changed).collect::<Vec<_>>(),
            },
            "rpc": self.dash.methods_json(),
            "addresses": addresses,
            "suggestion": self.dash.suggestion(&sim.store),
            "served": self.served_json(),
        })
    }

    /// Requests skim answered for other peers.
    pub(super) fn served_json(&self) -> Value {
        let c = self.sim.sched.transport().net.serve_counts();
        json!({
            "el_enode": self.sim.sched.transport().net.enode(),
            "cl_peer_id": self.cl_stats.as_ref().map(|s| s.lock().unwrap().peer_id.clone()),
            "el": {
                "headers": {"requests": c.header_reqs, "items": c.headers},
                "bodies": {"requests": c.body_reqs, "items": c.bodies},
                "receipts": {"requests": c.receipt_reqs, "items": c.receipts},
                "bals": {"requests": c.bal_reqs, "items": c.bals},
                "snap": {"requests": c.snap_reqs, "bytecodes": c.codes},
                "other_requests": c.other_reqs,
                "served_from": self.chain.as_ref().and_then(|c| c.served_from()),
            },
            "cl": self.cl_stats.as_ref().map(|s| {
                let s = s.lock().unwrap();
                json!({
                    "envelopes_by_range": {"requests": s.envelope_range_reqs},
                    "envelopes_by_root": {"requests": s.envelope_root_reqs},
                    "envelopes": s.envelopes_served,
                    "served_from_slot": s.envelopes_from_slot,
                })
            }),
        })
    }

    /// The block `latest` means for this request (the head, or the verified head).
    pub(super) fn latest(&self) -> Option<BlockRef> {
        if self.require_verified {
            self.light.as_ref().and_then(|l| l.status().verified_block).and_then(|v| crate::light::resolve(&self.sim.store, &v))
        } else {
            self.sim.store.head()
        }
    }
}
