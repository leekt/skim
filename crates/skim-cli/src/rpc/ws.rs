//! WebSocket sessions: JSON-RPC requests plus `eth_subscribe` pushes, delivered per
//! subscription ([`crate::ws_feed`]).

use super::*;

pub(super) async fn ws_upgrade(
    State(rpc): State<Arc<Rpc>>,
    axum::Extension(policy): axum::Extension<Arc<OriginPolicy>>,
    headers: axum::http::HeaderMap,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !policy.websocket_allowed(&headers) {
        return (axum::http::StatusCode::FORBIDDEN, "origin not allowed by --cors").into_response();
    }
    let caller = policy.caller(&headers);
    ws.on_upgrade(move |socket| ws_session(rpc, socket, caller))
}

/// One WebSocket connection: ordinary JSON-RPC requests (single or batch), plus
/// `eth_subscribe` / `eth_unsubscribe` for:
/// - `newHeads`: each block whose state is ready
/// - `logs` (with a filter): that block's matching logs
/// - `skim_subscriptions`: readiness and the latest `on_block` results after each block
async fn ws_session(rpc: Arc<Rpc>, socket: axum::extract::ws::WebSocket, caller: Caller) {
    use axum::extract::ws::Message;
    use futures::{SinkExt, StreamExt};
    let (mut tx, mut rx) = socket.split();
    let mut events = rpc.events.subscribe();
    let mut subs: std::collections::HashMap<String, Sub> = Default::default();
    let mut next_id = 1u64;
    loop {
        tokio::select! {
            msg = rx.next() => {
                let Some(Ok(msg)) = msg else { break };
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                let Ok(body) = serde_json::from_str::<Value>(&text) else {
                    let _ = tx.send(Message::Text(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}}).to_string().into())).await;
                    continue;
                };
                let epoch = rpc.reorg_epoch();
                let latest = rpc.latest();
                let mut one = |req: &Value| -> Option<Value> {
                    let method = req.get("method").and_then(Value::as_str)?;
                    let id = req.get("id").cloned().unwrap_or(Value::Null);
                    let params = req.get("params").cloned().unwrap_or(json!([]));
                    match method {
                        "eth_subscribe" => {
                            let kind = params.get(0).and_then(Value::as_str).unwrap_or("");
                            if subs.len() >= MAX_WS_SUBSCRIPTIONS {
                                return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32005, "message": format!("too many subscriptions on this connection (at most {MAX_WS_SUBSCRIPTIONS})")}}));
                            }
                            let sub = match kind {
                                "newHeads" => Sub::NewHeads { track: crate::ws_feed::HeadTrack::seeded(latest.map(|b| (b.number, b.hash))) },
                                "skim_subscriptions" => match &caller {
                                    Caller::Page(origin) => return Some(json!({"jsonrpc": "2.0", "id": id, "error": page_refused("skim_subscriptions", origin)})),
                                    Caller::Trusted => Sub::Skim,
                                },
                                "logs" => match serde_json::from_value::<alloy_rpc_types_eth::Filter>(params.get(1).cloned().unwrap_or(json!({}))) {
                                    // Logs from blocks after the one current now (or replacing it).
                                    Ok(f) => Sub::Logs {
                                        filter: Box::new(f),
                                        track: crate::ws_feed::HeadTrack::seeded(latest.map(|b| (b.number, b.hash))),
                                        delivered: Default::default(),
                                        failures: 0,
                                    },
                                    Err(e) => return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": format!("logs filter: {e}")}})),
                                },
                                other => return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": format!("unsupported subscription {other:?} (newHeads, logs, skim_subscriptions)")}})),
                            };
                            let sid = format!("{:#x}", next_id);
                            next_id += 1;
                            subs.insert(sid.clone(), sub);
                            Some(json!({"jsonrpc": "2.0", "id": id, "result": sid}))
                        }
                        "eth_unsubscribe" => {
                            let sid = params.get(0).and_then(Value::as_str).unwrap_or("");
                            Some(json!({"jsonrpc": "2.0", "id": id, "result": subs.remove(sid).is_some()}))
                        }
                        _ => None, // handled below (async)
                    }
                };
                let reply = match &body {
                    Value::Array(reqs) if reqs.len() > MAX_BATCH => batch_too_large(),
                    Value::Array(reqs) => {
                        // Subscription changes first (in order, they touch this session), then the
                        // other requests concurrently.
                        let mut out: Vec<Option<Value>> = reqs.iter().map(&mut one).collect();
                        let rest: Vec<&Value> = reqs.iter().zip(&out).filter(|(_, o)| o.is_none()).map(|(r, _)| r).collect();
                        let mut answered = rpc.batch(rest, &latest, epoch, caller.clone()).await.into_iter();
                        for o in out.iter_mut().filter(|o| o.is_none()) {
                            *o = answered.next();
                        }
                        Value::Array(out.into_iter().map(|o| o.unwrap_or(Value::Null)).collect())
                    }
                    req => match one(req) {
                        Some(v) => v,
                        None => rpc.one(req, &latest, epoch, caller.clone()).await,
                    },
                };
                if tx.send(Message::Text(reply.to_string().into())).await.is_err() {
                    break;
                }
            }
            ev = events.recv() => {
                let ev = match ev {
                    Ok(ev) => ev,
                    // Missed heads are caught up on the next one (`HeadTrack` reports every block
                    // since the last it reported); a missed subscriptions push is superseded.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                };
                let sent = match ev {
                    WsEvent::NewHead(b) => {
                        // With `--require-verified`, pushes follow the verified head too, like every
                        // read. (Review: they followed optimistic heads.)
                        let head = if rpc.require_verified { rpc.latest() } else { Some(b) };
                        match head {
                            Some(head) => push_heads(&rpc, &mut subs, (head.number, head.hash), &mut tx).await,
                            None => true,
                        }
                    }
                    WsEvent::Subscriptions(n) => match rpc.subs.as_ref() {
                        Some(s) => {
                            let result = json!({"block": format!("{n:#x}"), "subscriptions": s.status()});
                            let mut ok = true;
                            for (sid, _) in subs.iter().filter(|(_, s)| matches!(s, Sub::Skim)) {
                                ok &= send_push(&mut tx, sid, result.clone()).await;
                            }
                            ok
                        }
                        None => true,
                    },
                };
                if !sent {
                    return;
                }
            }
        }
    }
}

type WsSink = futures::stream::SplitSink<axum::extract::ws::WebSocket, axum::extract::ws::Message>;

async fn send_push(tx: &mut WsSink, sid: &str, result: Value) -> bool {
    use futures::SinkExt;
    let msg = json!({"jsonrpc": "2.0", "method": "eth_subscription", "params": {"subscription": sid, "result": result}});
    tx.send(axum::extract::ws::Message::Text(msg.to_string().into())).await.is_ok()
}

/// Blocks in a row a `logs` subscription may fail to deliver (receipts not fetched) before it
/// ends with an error, rather than stall or skip silently.
const MAX_LOG_DELIVERY_FAILURES: u32 = 8;

/// Moves every head-following subscription to `head`: `newHeads` gets each added block's header;
/// `logs` gets the logs it reported for removed blocks again with `removed: true` (newest
/// first), then the logs of each added block. A subscription's position advances per block
/// delivered. A `logs` subscription that can't be told everything (a gap too long to report, or
/// receipts that keep failing) ends with an error notification instead of skipping silently.
/// False once the connection is gone.
async fn push_heads(rpc: &Rpc, subs: &mut std::collections::HashMap<String, Sub>, head: (u64, B256), tx: &mut WsSink) -> bool {
    let canon = |n: u64| rpc.sim.store.block(n).map(|x| x.hash);
    let mut ended = Vec::new();
    for (sid, sub) in subs.iter_mut() {
        match sub {
            Sub::NewHeads { track } => {
                // Heads more than `KEEP` behind are skipped, as geth skips them for a slow reader.
                let step = track.step(head, canon);
                if let Some((_, to)) = step.skipped
                    && let Some(h) = canon(to)
                {
                    track.skip_to(to, h);
                }
                for &(n, hash) in &step.added {
                    let Some(b) = rpc.sim.store.block(n).filter(|b| b.hash == hash) else { break };
                    let Some(h) = rpc.sim.header_arc(&b) else { break };
                    let Ok(v) = serde_json::to_value(alloy_rpc_types_eth::Header::new((*h).clone())) else { break };
                    if !send_push(tx, sid, v).await {
                        return false;
                    }
                    track.mark(n, hash);
                }
            }
            Sub::Logs { filter, track, delivered, failures } => {
                let Some(c) = &rpc.chain else { continue };
                let step = track.step(head, canon);
                for &(n, hash) in &step.removed {
                    let Some((h, logs)) = delivered.remove(&n) else { continue };
                    if h != hash {
                        continue;
                    }
                    for mut log in logs.into_iter().rev() {
                        log["removed"] = json!(true);
                        if !send_push(tx, sid, log).await {
                            return false;
                        }
                    }
                }
                if let Some((from, to)) = step.skipped {
                    ended.push((sid.clone(), format!("logs of blocks {from}..={to} can't be delivered (more than {} blocks behind); subscribe again", crate::ws_feed::KEEP)));
                    continue;
                }
                for &(n, hash) in &step.added {
                    match c.logs(filter, n, n, Some(hash)).await {
                        Ok(Value::Array(logs)) => {
                            // Logs: one notification per log, as clients expect.
                            for log in &logs {
                                if !send_push(tx, sid, log.clone()).await {
                                    return false;
                                }
                            }
                            delivered.insert(n, (hash, logs));
                            track.mark(n, hash);
                            *failures = 0;
                        }
                        // Not delivered: the position stays, and the next head tries it again.
                        // (Review: the block was skipped for good.)
                        other => {
                            *failures += 1;
                            #[cfg(test)]
                            c.run_test_hook("ws_delivery_failed");
                            if *failures >= MAX_LOG_DELIVERY_FAILURES {
                                let why = match other {
                                    Err(e) => RpcError::from(e).message,
                                    Ok(_) => "unexpected result".into(),
                                };
                                ended.push((sid.clone(), format!("logs of block {n} couldn't be fetched ({why}); subscribe again")));
                            }
                            break;
                        }
                    }
                }
                while delivered.len() as u64 > crate::ws_feed::KEEP {
                    delivered.pop_first();
                }
            }
            Sub::Skim => {}
        }
    }
    for (sid, why) in ended {
        subs.remove(&sid);
        let msg = json!({"jsonrpc": "2.0", "method": "eth_subscription", "params": {"subscription": sid, "error": {"code": -32002, "message": why}}});
        use futures::SinkExt;
        if tx.send(axum::extract::ws::Message::Text(msg.to_string().into())).await.is_err() {
            return false;
        }
    }
    true
}

enum Sub {
    NewHeads { track: crate::ws_feed::HeadTrack },
    /// `track`: the blocks delivered, from the head when it subscribed; `delivered`: the logs
    /// pushed per block, to withdraw if the block leaves the chain; `failures`: deliveries failed
    /// in a row.
    Logs { filter: Box<alloy_rpc_types_eth::Filter>, track: crate::ws_feed::HeadTrack, delivered: std::collections::BTreeMap<u64, (B256, Vec<Value>)>, failures: u32 },
    Skim,
}
