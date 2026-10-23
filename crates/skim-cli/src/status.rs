//! `skim status`: asks a running skim (its JSON-RPC) how it's doing and says, in plain words,
//! what works, what doesn't yet, and what to do about it. Exits non-zero if a check fails, so
//! scripts and health checks can use it.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

#[derive(clap::Args)]
pub struct Args {
    /// The skim to ask (its `--rpc` address).
    #[arg(long, default_value = "http://127.0.0.1:8545")]
    rpc: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

/// One line of the report: what, how it is, and what to do if it isn't fine.
#[derive(Debug)]
pub struct Check {
    pub what: &'static str,
    pub level: Level,
    pub detail: String,
    pub hint: Option<&'static str>,
}

/// The checks, from `skim_status`, `skim_subscriptionStatus` and the head's timestamp.
pub fn assess(status: &Value, subs: &Value, head_ts: Option<u64>, now: u64) -> Vec<Check> {
    let mut out = Vec::new();
    let n = |v: &Value| v.as_u64();

    let lag = head_ts.map(|t| now.saturating_sub(t));
    let fork = n(&status["network"]["glamsterdam_at"]).filter(|t| *t > now);
    out.push(match (n(&status["head"]), lag) {
        _ if fork.is_some() => {
            let left = fork.unwrap() - now;
            Check {
                what: "chain",
                level: Level::Warn,
                detail: format!("waiting for Glamsterdam on {}: in {}d {}h", status["network"]["name"].as_str().unwrap_or("this network"), left / 86400, left % 86400 / 3600),
                hint: Some("skim follows a network from its Glamsterdam fork on (no BALs before it); it picks the chain up at activation"),
            }
        }
        (Some(h), Some(l)) if l <= 60 => Check { what: "chain", level: Level::Ok, detail: format!("following: head {h}, {l} s old"), hint: None },
        (Some(h), Some(l)) => Check {
            what: "chain",
            level: if l > 600 { Level::Fail } else { Level::Warn },
            detail: format!("head {h} is {l} s old"),
            hint: Some("no recent blocks from consensus gossip: check outbound connectivity (TCP and UDP on --cl-port, QUIC on --cl-port + 1)"),
        },
        _ => Check { what: "chain", level: Level::Warn, detail: "no block applied yet".into(), hint: Some("starting: the first block usually arrives within a minute") },
    });

    let forks = &status["network"]["forks"];
    let when = |at: u64| if at > now { format!("in {}d {}h", (at - now) / 86400, (at - now) % 86400 / 3600) } else { "active".into() };
    if let Some(at) = n(&forks["stops_at"]["at"]) {
        let fork = forks["stops_at"]["fork"].as_str().unwrap_or("a fork");
        out.push(Check {
            what: "forks",
            level: if now >= at { Level::Fail } else { Level::Warn },
            detail: if now >= at { format!("stopped at {fork}: skim doesn't implement it, no block after it is applied") } else { format!("{fork} {}: skim doesn't implement it and stops there", when(at)) },
            hint: Some(if fork.starts_with("hegota") {
                "Hegota: run a skim release that supports it, or --experimental-hegota (assumptions in docs/guide/hegota.md)"
            } else {
                "run a skim release that implements this fork"
            }),
        });
    } else if let Some(at) = n(&forks["hegota"]["at"]).filter(|_| forks["hegota"]["support"] == "experimental") {
        out.push(Check {
            what: "forks",
            level: if now >= at { Level::Warn } else { Level::Ok },
            detail: format!("Hegota {}: followed with experimental support", when(at)),
            hint: (now >= at).then_some("answers after Hegota rest on assumptions its final spec may change (docs/guide/hegota.md)"),
        });
    }

    let el = n(&status["el_peers"]).unwrap_or(0);
    let snap = n(&status["snap_peers"]).unwrap_or(0);
    out.push(match (el, snap) {
        (0, _) => Check {
            what: "execution peers",
            level: Level::Fail,
            detail: "none connected".into(),
            hint: Some("state reads wait for a SNAP peer (--state-wait): check outbound TCP and UDP on --el-port; a fresh start can take a few minutes"),
        },
        (e, 0) => Check {
            what: "execution peers",
            level: Level::Warn,
            detail: format!("{e} connected, none serving state (SNAP)"),
            hint: Some("reads of state skim doesn't hold wait until a SNAP-serving peer connects"),
        },
        (e, s) => Check { what: "execution peers", level: Level::Ok, detail: format!("{e} connected, {s} serving state"), hint: None },
    });

    let lc = &status["light_client"];
    let verified = n(&lc["verified_block"]["number"]);
    out.push(match verified {
        Some(v) => {
            let behind = n(&status["head"]).map(|h| h.saturating_sub(v)).unwrap_or(0);
            let fin = n(&lc["finalized_block"]["number"]).map_or("not yet".to_string(), |f| f.to_string());
            Check {
                what: "light client",
                level: if behind > 64 { Level::Warn } else { Level::Ok },
                detail: format!("signed head {v} ({behind} behind), finalized {fin}"),
                hint: (behind > 64).then_some("signed updates are lagging: `safe` and --require-verified answers are behind"),
            }
        }
        None => Check {
            what: "light client",
            level: Level::Warn,
            detail: lc["last_error"].as_str().map_or("not verified yet".into(), |e| format!("not verified yet ({})", e.chars().take(80).collect::<String>())),
            hint: Some("`latest` still answers (optimistic); `safe`, `finalized` and --require-verified wait for a verified update"),
        },
    });

    let subs = subs.as_array().cloned().unwrap_or_default();
    let ready = subs.iter().filter(|s| s["ready"].as_bool() == Some(true)).count();
    out.push(if subs.is_empty() {
        Check { what: "state", level: Level::Ok, detail: "no subscriptions: everything is read on demand".into(), hint: None }
    } else if ready == subs.len() {
        Check { what: "state", level: Level::Ok, detail: format!("{ready}/{} subscriptions ready", subs.len()), hint: None }
    } else {
        let waiting: Vec<&str> = subs.iter().filter(|s| s["ready"].as_bool() != Some(true)).filter_map(|s| s["name"].as_str()).take(5).collect();
        Check {
            what: "state",
            level: Level::Warn,
            detail: format!("{ready}/{} subscriptions ready; waiting: {}", subs.len(), waiting.join(", ")),
            hint: Some("subscribed state is fetched from SNAP peers; `skim_subscriptionStatus` lists the missing keys"),
        }
    });

    let h = &status["history"];
    out.push(Check {
        what: "history",
        level: Level::Ok,
        detail: match h["mode"].as_str() {
            Some("local") => format!("local (post-fork headers on disk); logs from block {}", h["logs_from"]),
            _ => format!("head-only; logs from block {} (about the last day)", h["logs_from"]),
        },
        hint: None,
    });
    out
}

pub async fn run(args: Args) -> anyhow::Result<()> {
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build()?;
    let call = |method: &'static str, params: Value| {
        let (client, url) = (client.clone(), args.rpc.clone());
        async move {
            let v: Value = client.post(&url).json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})).send().await?.json().await?;
            anyhow::Ok(v["result"].clone())
        }
    };
    let status = match call("skim_status", json!([])).await {
        Ok(s) if !s.is_null() => s,
        Ok(_) => anyhow::bail!("{} answered, but not as skim (no skim_status)", args.rpc),
        Err(e) => {
            println!("skim at {}: not reachable ({e})", args.rpc);
            println!("  start one with `skim` (a Sepolia node, no config needed), or pass --rpc");
            std::process::exit(2);
        }
    };
    let subs = call("skim_subscriptionStatus", json!([])).await.unwrap_or(Value::Null);
    let version = call("web3_clientVersion", json!([])).await.unwrap_or(Value::Null);
    let chain = call("eth_chainId", json!([])).await.ok().and_then(|v| v.as_str().and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()));
    let head_ts = call("eth_getBlockByNumber", json!(["latest", false]))
        .await
        .ok()
        .and_then(|b| b["timestamp"].as_str().and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()));
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

    println!("skim at {}: {} (chain {})", args.rpc, version.as_str().unwrap_or("?"), chain.map_or("?".into(), |c| c.to_string()));
    let checks = assess(&status, &subs, head_ts, now);
    for c in &checks {
        let mark = match c.level {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        };
        println!("  [{mark}] {:<16} {}", c.what, c.detail);
        if let Some(h) = c.hint {
            println!("         {:<16} -> {h}", "");
        }
    }
    if checks.iter().any(|c| c.level == Level::Fail) {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_healthy_zero_config_node_is_all_ok() {
        let status = json!({"head": 100, "el_peers": 5, "snap_peers": 3, "light_client": {"verified_block": {"number": 99}, "finalized_block": {"number": 64}}, "history": {"mode": "off", "logs_from": 50}});
        let checks = assess(&status, &json!([]), Some(1_000), 1_008);
        assert!(checks.iter().all(|c| c.level == Level::Ok), "{checks:?}");
    }

    #[test]
    fn missing_peers_and_a_stale_head_say_what_to_do() {
        let status = json!({"head": 100, "el_peers": 0, "snap_peers": 0, "light_client": {"last_error": "bootstrap: no CL peers yet"}, "history": {"mode": "off"}});
        let subs = json!([{"name": "pool", "ready": false}, {"name": "token", "ready": true}]);
        let checks = assess(&status, &subs, Some(1_000), 1_000 + 900);
        let by = |w: &str| checks.iter().find(|c| c.what == w).unwrap();
        assert_eq!(by("chain").level, Level::Fail);
        assert_eq!(by("execution peers").level, Level::Fail);
        assert!(by("execution peers").hint.unwrap().contains("--el-port"));
        assert_eq!(by("light client").level, Level::Warn);
        assert!(by("state").detail.contains("waiting: pool"));
    }

    #[test]
    fn before_glamsterdam_the_chain_check_says_when() {
        let status = json!({"head": null, "el_peers": 2, "snap_peers": 2, "light_client": {}, "history": {"mode": "off"}, "network": {"name": "hoodi", "glamsterdam_at": 1_000 + 6 * 86400 + 3 * 3600}});
        let c = assess(&status, &json!([]), None, 1_000);
        assert_eq!(c[0].detail, "waiting for Glamsterdam on hoodi: in 6d 3h");
    }

    #[test]
    fn a_fork_skim_stops_at_is_a_warning_then_a_failure() {
        let st = |forks: Value| json!({"head": 100, "el_peers": 5, "snap_peers": 3, "light_client": {}, "history": {"mode": "off"}, "network": {"name": "devnet", "forks": forks}});
        let stop = st(json!({"hegota": {"at": 5_000, "support": "none"}, "stops_at": {"fork": "hegota", "at": 5_000}}));
        let by = |c: Vec<Check>| c.into_iter().find(|c| c.what == "forks").unwrap();
        let before = by(assess(&stop, &json!([]), Some(1_000), 1_000));
        assert_eq!((before.level, before.detail.as_str()), (Level::Warn, "hegota in 0d 1h: skim doesn't implement it and stops there"));
        assert!(before.hint.unwrap().contains("--experimental-hegota"));
        assert_eq!(by(assess(&stop, &json!([]), Some(5_000), 5_000)).level, Level::Fail);
        let on = st(json!({"hegota": {"at": 5_000, "support": "experimental"}, "stops_at": null}));
        assert_eq!(by(assess(&on, &json!([]), Some(1_000), 1_000)).level, Level::Ok);
        assert_eq!(by(assess(&on, &json!([]), Some(5_000), 5_000)).detail, "Hegota active: followed with experimental support");
        // Nothing scheduled: no forks check at all.
        assert!(assess(&st(json!({"hegota": null, "stops_at": null})), &json!([]), Some(1_000), 1_000).iter().all(|c| c.what != "forks"));
    }
}
