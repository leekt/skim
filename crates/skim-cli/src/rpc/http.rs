//! The HTTP server: JSON-RPC over POST, the WebSocket upgrade, CORS and which callers may
//! use `skim_*` methods.

use super::*;

async fn handle(State(rpc): State<Arc<Rpc>>, axum::Extension(policy): axum::Extension<Arc<OriginPolicy>>, headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Json<Value> {
    let caller = policy.caller(&headers);
    // The epoch before `latest`: a reorg between the two shows up as a changed epoch.
    let epoch = rpc.reorg_epoch();
    let latest = rpc.latest();
    match body {
        Value::Array(reqs) if reqs.len() > MAX_BATCH => Json(batch_too_large()),
        Value::Array(reqs) => {
            let mut out = Vec::with_capacity(reqs.len());
            out.extend(rpc.batch(reqs.iter().collect(), &latest, epoch, caller.clone()).await);
            Json(Value::Array(out))
        }
        req => Json(rpc.one(&req, &latest, epoch, caller).await),
    }
}

/// Who is calling, for what a browser page may see. `skim_*` methods expose what this node is
/// used for (subscription names and addresses, the addresses calls read, peer ids): a page
/// gets them only from an origin `--cors` names exactly, never through `*`. Everything else
/// follows `--cors` as before. A request without an `Origin` header (curl, a server, viem in
/// Node) is not a page. (Review R6: with the default `--cors *`, any web page the user opened
/// could read `skim_status` and `skim_dashboard` from a local node.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// No `Origin`, or an origin `--cors` names exactly.
    Trusted,
    /// A browser page from another origin; `skim_*` methods are refused.
    Page(Arc<str>),
}

pub(super) fn page_refused(method: &str, origin: &str) -> Value {
    json!({"code": -32004, "message": format!("{method} is not available to web pages ({origin}): skim_* methods reveal what this node watches; allow this origin with --cors {origin}")})
}

/// `--cors` as a policy: the origin browser pages may call from (`*` for any), or none.
#[derive(Debug, Clone, Default)]
pub struct OriginPolicy {
    pub cors: Option<String>,
}

impl OriginPolicy {
    pub fn caller(&self, headers: &axum::http::HeaderMap) -> Caller {
        match headers.get(axum::http::header::ORIGIN).and_then(|o| o.to_str().ok()) {
            None => Caller::Trusted,
            Some(o) if self.cors.as_deref() == Some(o) => Caller::Trusted,
            Some(o) => Caller::Page(Arc::from(o)),
        }
    }

    /// Whether a page from `origin` may open a WebSocket: browsers don't apply CORS to
    /// WebSocket upgrades, so the server checks the `Origin` itself, against `--cors`.
    pub fn websocket_allowed(&self, headers: &axum::http::HeaderMap) -> bool {
        match headers.get(axum::http::header::ORIGIN).and_then(|o| o.to_str().ok()) {
            None => true,
            Some(o) => matches!(self.cors.as_deref(), Some("*")) || self.cors.as_deref() == Some(o),
        }
    }
}

/// Requests per batch (HTTP or WebSocket), as geth's default. (Review: unlimited; only the 2 MB
/// body limit applied, ~30k requests.)
pub(super) const MAX_BATCH: usize = 1000;
/// Subscriptions per WebSocket connection. (Review: unlimited; every block ran a log scan for
/// each `logs` subscription.)
pub(super) const MAX_WS_SUBSCRIPTIONS: usize = 128;

pub(super) fn batch_too_large() -> Value {
    json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32600, "message": format!("batch too large: at most {MAX_BATCH} requests")}})
}

/// `cors`: the `Access-Control-Allow-Origin` to send (browser dapps calling skim directly), or
/// `None` to send none.
pub async fn serve(rpc: Arc<Rpc>, listener: tokio::net::TcpListener, cors: Option<String>) -> anyhow::Result<()> {
    use axum::http::{HeaderValue, header};
    // HTTP POST and WebSocket on the same address; OPTIONS answers CORS preflights.
    let policy = Arc::new(OriginPolicy { cors: cors.clone() });
    let mut app = Router::new()
        .route("/", post(handle).get(ws_upgrade).options(|| async { axum::http::StatusCode::NO_CONTENT }))
        .with_state(rpc)
        .layer(axum::Extension(policy));
    if let Some(origin) = cors {
        let origin = HeaderValue::from_str(&origin)?;
        app = app.layer(axum::middleware::map_response(move |mut r: axum::response::Response| {
            let origin = origin.clone();
            async move {
                let h = r.headers_mut();
                h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
                h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("POST, GET, OPTIONS"));
                h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("content-type"));
                h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("86400"));
                r
            }
        }));
    }
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "JSON-RPC listening (HTTP and WebSocket)");
    axum::serve(listener, app).await?;
    Ok(())
}
