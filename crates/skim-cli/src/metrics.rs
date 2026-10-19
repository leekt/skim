//! Prometheus metrics at `/metrics` on `--metrics <addr>` (off unless set). Scraped by the
//! Prometheus + Grafana stack in `metrics/`. reth's own network metrics are exported too.
//!
//! Gauges that mirror live state (peers, store sizes, served counts, address usage) are
//! refreshed on every scrape; latencies are histograms recorded as they happen.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use metrics::{Unit, describe_counter, describe_gauge, describe_histogram};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use metrics_util::MetricKindMask;

use crate::rpc::Rpc;

/// Installs the global recorder. Series not updated for `idle` (peers that left, addresses that
/// dropped out of the top N) are removed.
pub fn install(idle: Duration) -> anyhow::Result<PrometheusHandle> {
    let handle = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full("skim_rpc_request_duration_seconds".into()),
            &[0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0],
        )?
        .set_buckets_for_metric(
            Matcher::Full("skim_block_arrival_seconds".into()),
            &[0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 2.5, 3.0, 4.0, 6.0, 12.0],
        )?
        .idle_timeout(MetricKindMask::GAUGE | MetricKindMask::COUNTER, Some(idle))
        .install_recorder()?;
    describe();
    metrics_process::Collector::default().describe();
    Ok(handle)
}

fn describe() {
    describe_gauge!("skim_head_block", "Latest block whose state is ready");
    describe_gauge!("skim_head_age_seconds", Unit::Seconds, "Time since the head block's slot started");
    describe_histogram!("skim_block_arrival_seconds", Unit::Seconds, "Payload arrival after slot start, per applied block");
    describe_gauge!("skim_block_changed_keys", "Tracked keys the last block changed");
    describe_histogram!("skim_rpc_request_duration_seconds", Unit::Seconds, "JSON-RPC request latency by method");
    describe_counter!("skim_rpc_requests_total", "JSON-RPC requests by method and outcome");
    describe_gauge!("skim_peers", "Connected peers by layer (el, cl)");
    describe_gauge!("skim_snap_peers", "Connected EL peers that serve SNAP state");
    describe_gauge!("skim_memory_bytes", "Estimated payload bytes retained, by structure (part): payload only, no allocator overhead");
    describe_gauge!("skim_memory_entries", "Entries retained, by structure (part)");
    describe_gauge!("skim_memory_capacity", "Allocated slots (collection capacity), by structure (part)");
    describe_gauge!("skim_peer_clients", "Connected peers by layer and client");
    describe_gauge!("skim_snap_peer_latency_ms", "SNAP answer latency per peer (EWMA)");
    describe_gauge!("skim_snap_peer_inflight", "SNAP requests in flight per peer");
    describe_counter!("skim_snap_peer_answers_total", "SNAP answers per peer");
    describe_counter!("skim_snap_peer_errors_total", "SNAP errors per peer");
    describe_counter!("skim_snap_fetch_total", "SNAP fetch scheduler events by kind");
    describe_counter!("skim_el_sessions_total", "EL sessions by client and whether the peer offers SNAP");
    describe_counter!("skim_el_session_closes_total", "EL session ends by reason (TooManyPeers = the peer was full)");
    describe_gauge!("skim_el_first_snap_peer_seconds", Unit::Seconds, "Time from start to the first SNAP-capable session");
    describe_gauge!("skim_el_snap_peers_seen", "SNAP-capable peers seen this run");
    describe_gauge!("skim_el_snap_peers_held", "SNAP-capable peers that held a session for at least 60 s");
    describe_gauge!("skim_store_keys", "Store keys by kind (subscribed, on_demand)");
    describe_gauge!("skim_store_bytes", "Store sizes in bytes by kind (code, recent_bals)");
    describe_counter!("skim_store_admissions_refused_total", "Keys refused by max_subscribed_keys");
    describe_counter!("skim_store_on_demand_evicted_total", "On-demand keys evicted by max_on_demand_keys");
    describe_counter!("skim_store_resolved_from_bal_total", "Reads of untracked keys answered from a recent block's BAL write instead of a fetch");
    describe_gauge!("skim_subscription_ready", "1 when a subscription's state is complete at the head");
    describe_gauge!("skim_subscription_missing_keys", "Keys a subscription still misses");
    describe_counter!("skim_served_requests_total", "Requests skim answered for other peers");
    describe_counter!("skim_served_items_total", "Items skim sent to other peers");
    describe_counter!("skim_address_calls_total", "eth_call executions that read this address (top N)");
    describe_counter!("skim_address_fetched_keys_total", "Keys fetched from the network for this address (top N)");
    describe_gauge!("skim_address_slots", "Distinct slots read at this address, by pinning (top N)");
    describe_gauge!("skim_address_pinning", "2 = pinned by a subscription, 1 = partly pinned, 0 = on demand (top N)");
    describe_gauge!("skim_address_info", "Always 1; `subscription` names the subscription that pins the address, if any (top N)");
}

/// Serves `/metrics` until the process exits.
pub async fn serve(addr: SocketAddr, handle: PrometheusHandle, rpc: Arc<Rpc>, top_addresses: usize) -> anyhow::Result<()> {
    let process = metrics_process::Collector::default();
    let app = axum::Router::new().route(
        "/metrics",
        axum::routing::get(move || {
            let (rpc, handle, process) = (rpc.clone(), handle.clone(), process.clone());
            async move {
                process.collect();
                rpc.refresh_metrics(top_addresses);
                handle.run_upkeep();
                handle.render()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "Prometheus metrics at /metrics");
    axum::serve(listener, app).await?;
    Ok(())
}
