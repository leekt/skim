//! `skim probe-catchup`: runs the restart catch-up walk (consensus `ByRange` requests, newest
//! first) for a given number of blocks, without needing an old snapshot, and prints where the
//! time went. Diagnostics for restarts that fall back to EL peers.

use std::time::Duration;

use skim_cl::{ClNode, NodeConfig};

#[derive(clap::Args)]
pub struct Args {
    /// Blocks to walk back from the current slot (e.g. a restart gap).
    #[arg(long, default_value_t = 655)]
    blocks: usize,
    #[arg(long, default_value_t = 9240)]
    cl_port: u16,
    /// Seconds to let peers connect (and send Status) before the walk.
    #[arg(long, default_value_t = 30)]
    warmup: u64,
    /// Walks to run back to back.
    #[arg(long, default_value_t = 1)]
    runs: usize,
}

pub async fn run(args: Args) -> anyhow::Result<()> {
    let (cl, _rx) = ClNode::start(NodeConfig::sepolia(args.cl_port)).await?;
    tokio::time::sleep(Duration::from_secs(args.warmup)).await;
    for run in 0..args.runs {
        let peers = cl.stats.lock().unwrap().peer_list.iter().filter(|p| p.head_slot.is_some()).count();
        let slot = NodeConfig::sepolia(0).current_slot();
        let mut trace = crate::persist::CatchupTrace::default();
        let want = args.blocks;
        let mut found = 0usize;
        let r = crate::persist::walk_envelopes(&cl, slot, (want as u64) * 4, &mut trace, |_, _| {
            found += 1;
            found >= want
        })
        .await;
        println!("run {run}: {} CL peers with a status; result: {:?}", peers, r.as_ref().map(|_| "ok").map_err(|e| e.to_string()));
        println!("{}", serde_json::to_string_pretty(&trace.summary())?);
        let mut slow: Vec<_> = trace.requests.iter().filter(|q| q.ms > 2000 || q.error.is_some()).collect();
        slow.sort_by_key(|q| std::cmp::Reverse(q.ms));
        for q in slow.iter().take(8) {
            println!("  slow/failed: slots {}..={} peer {} {} ms envelopes {} {}",
                q.start_slot, q.end_slot, q.peer.as_deref().map_or("-", |p| &p[p.len().saturating_sub(8)..]), q.ms, q.envelopes,
                q.error.as_deref().unwrap_or(""));
        }
    }
    Ok(())
}
