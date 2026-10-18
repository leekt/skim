//! `--dashboard <addr>`: a read-only web page with skim's state, safe to put on a public
//! hostname. It serves only `/` (the page) and `/dashboard.json`, assembled from
//! `skim_status`, `skim_subscriptionStatus` and `skim_dashboard`: no other JSON-RPC method is
//! reachable through it. The node's enode (which carries its IP) is left out, and the document
//! is computed at most once per [`CACHE_FOR`], however many viewers poll it.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{Router, extract::State, response::Html, routing::get};
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::rpc::Rpc;

const CACHE_FOR: Duration = Duration::from_secs(2);
const PAGE: &str = include_str!("web_dashboard.html");

struct Web {
    rpc: Arc<Rpc>,
    cached: Mutex<Option<(Instant, Value)>>,
}

pub async fn serve(rpc: Arc<Rpc>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let web = Arc::new(Web { rpc, cached: Mutex::new(None) });
    let app = Router::new().route("/", get(|| async { Html(PAGE) })).route("/dashboard.json", get(document)).with_state(web);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "read-only dashboard listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn document(State(web): State<Arc<Web>>) -> axum::Json<Value> {
    if let Some((at, v)) = web.cached.lock().as_ref()
        && at.elapsed() < CACHE_FOR
    {
        return axum::Json(v.clone());
    }
    let latest = web.rpc.latest();
    let mut doc = json!({});
    for (key, method) in [("status", "skim_status"), ("subscriptions", "skim_subscriptionStatus"), ("dashboard", "skim_dashboard")] {
        doc[key] = web.rpc.dispatch(method, &json!([]), &latest).await.unwrap_or_else(|e| json!({"error": format!("{e:?}")}));
    }
    redact(&mut doc);
    *web.cached.lock() = Some((Instant::now(), doc.clone()));
    axum::Json(doc)
}

/// Drops what a public page shouldn't show: the node's enode (its IP and port).
fn redact(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.remove("el_enode");
            m.values_mut().for_each(redact);
        }
        Value::Array(a) => a.iter_mut().for_each(redact),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_enode_is_removed_wherever_it_appears() {
        let mut v = json!({"status": {"served": {"el_enode": "enode://x@1.2.3.4:30303", "cl_peer_id": "p"}}, "dashboard": {"served": [{"el_enode": "e"}]}});
        redact(&mut v);
        assert!(!v.to_string().contains("enode"), "{v}");
        assert_eq!(v["status"]["served"]["cl_peer_id"], "p");
    }
}
