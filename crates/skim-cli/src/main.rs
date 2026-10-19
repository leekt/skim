#![recursion_limit = "512"]
//! `skim` command line. For now: M0 probes (PROJECT.md §10).

// Linux only. With glibc's allocator a long catch-up left RSS at 2.4x the live heap (heaptrack
// on aarch64 Linux: 228 MB peak heap, 554 MB RSS), as freed receipt buffers stayed in per-thread
// arenas; mimalloc returns them. On macOS the system allocator is the smaller one: following the
// head with the same load, mimalloc's footprint was 236-255 MB against 86-94 MB (M7, "Memory").
#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// Chain data and the post-fork history store live in skim-chain (shared with skim-hub); these
// keep `crate::chaindata` / `crate::history` paths working.
use skim_chain::{chaindata, history};

mod bench_node;
mod chain;
mod dashboard;
mod persist;
mod light;
mod metrics;
mod follow;
mod probe_cl;
mod probe_clreq;
mod probe_call;
mod probe_catchup;
mod probe_el;
mod probe_serve;
mod rpc;
mod run;
mod serving;
mod stats;
mod stream;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "skim", about = "P2P-backed Ethereum simulation client")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// M0 Spike B: Sepolia EL peers, eth/71 BALs, snap/2 point lookups, root-servable lag.
    ProbeEl(probe_el::Args),
    /// Diagnostics: CL req/resp (payload envelopes by range, light-client bootstrap).
    ProbeClReq(probe_clreq::Args),
    /// Check what a skim node serves to peers (EL and CL), verifying every answer.
    ProbeServe(probe_serve::Args),
    /// Time the restart catch-up walk over consensus peers (where the time goes).
    ProbeCatchup(probe_catchup::Args),
    /// M0 Spike C: cold-call SNAP rounds and latency vs Infura eth_call at the same block.
    ProbeCall(probe_call::Args),
    /// M1: follow the chain from CL gossip BALs into the MVCC store; time block -> state ready
    /// -> eth_call done vs when Infura can answer the same block.
    Follow(follow::Args),
    /// Follow the chain and serve JSON-RPC (eth_call, eth_getStorageAt, ...) on top of it.
    Run(run::Args),
    /// P3 baseline: same per-block eth_call workload against a full node's RPC.
    BenchNode(bench_node::Args),
    /// Peer-count sweep: one large learned wave fetched with 1/2/4/all peers per block.
    ProbeWave(probe_call::WaveArgs),
    /// M0 Spike A: Sepolia CL gossip listener; execution-payload envelope timing and BAL checks.
    ProbeCl(probe_cl::Args),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // No transparent huge pages for skim: mimalloc asks for them, and a 2 MiB page that is only
    // partly used counts fully in RSS. Measured following the head on aarch64 Linux: 179 MB at
    // 26 min without, 251-256 MB with (same state; M7, "Memory"). MIMALLOC_ALLOW_THP=1 keeps them.
    #[cfg(target_os = "linux")]
    if std::env::var("MIMALLOC_ALLOW_THP").as_deref() != Ok("1") {
        // SAFETY: prctl with integer arguments only.
        unsafe { libc::prctl(libc::PR_SET_THP_DISABLE, 1, 0, 0, 0) };
    }
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,reth_network=warn,discv5=warn,libp2p_gossipsub=warn".into()),
        )
        .init();
    match Cli::parse().cmd {
        Cmd::ProbeEl(args) => probe_el::run(args).await,
        Cmd::ProbeClReq(args) => probe_clreq::run(args).await,
        Cmd::ProbeServe(args) => probe_serve::run(args).await,
        Cmd::ProbeCatchup(args) => probe_catchup::run(args).await,
        Cmd::ProbeCall(args) => probe_call::run(args).await,
        Cmd::Follow(args) => follow::run(args).await,
        Cmd::Run(args) => run::run(args).await,
        Cmd::BenchNode(args) => bench_node::run(args).await,
        Cmd::ProbeWave(args) => probe_call::run_wave(args).await,
        Cmd::ProbeCl(args) => probe_cl::run(args).await,
    }
}
