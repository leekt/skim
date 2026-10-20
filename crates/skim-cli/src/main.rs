#![recursion_limit = "512"]
//! `skim` command line. For now: M0 probes (PROJECT.md §10).

// Linux only. With glibc's allocator a long catch-up left RSS at 2.4x the live heap (heaptrack
// on aarch64 Linux: 228 MB peak heap, 554 MB RSS), as freed receipt buffers stayed in per-thread
// arenas; mimalloc returns them. On macOS the system allocator is the smaller one: following the
// head with the same load, mimalloc's footprint was 236-255 MB against 86-94 MB (M7, "Memory").
#[cfg(all(target_os = "linux", not(any(feature = "dhat-heap", feature = "alloc-system", feature = "alloc-jemalloc"))))]
type BaseAlloc = mimalloc::MiMalloc;
// Allocator comparisons on Linux: `alloc-system` (glibc) or `alloc-jemalloc`.
#[cfg(all(target_os = "linux", feature = "alloc-jemalloc", not(feature = "dhat-heap")))]
type BaseAlloc = tikv_jemallocator::Jemalloc;
#[cfg(all(not(feature = "dhat-heap"), any(not(target_os = "linux"), all(feature = "alloc-system", not(feature = "alloc-jemalloc")))))]
type BaseAlloc = std::alloc::System;
// Heap profiling (`--features dhat-heap`): every allocation with its call site; on exit,
// `dhat-heap.json` (view it at https://nnethercote.github.io/dh_view/dh_view.html).
#[cfg(feature = "dhat-heap")]
type BaseAlloc = dhat::Alloc;

/// Whichever allocator, counted: `heap::live()` is skim's live heap, so RSS minus it is what the
/// allocator holds beyond that (free pages it keeps, fragmentation) plus code and stacks.
#[global_allocator]
#[allow(clippy::default_constructed_unit_structs)]
static GLOBAL: heap::Counting<BaseAlloc> = heap::Counting(BaseAlloc {});

/// Live heap accounting around the global allocator: one relaxed atomic per allocation and free.
mod heap {
    use std::alloc::{GlobalAlloc, Layout};
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    static LIVE: AtomicUsize = AtomicUsize::new(0);

    /// Bytes currently allocated by skim (requested sizes; the allocator's own overhead is not
    /// in it, by design).
    pub fn live() -> usize {
        LIVE.load(Relaxed)
    }

    pub struct Counting<A>(pub A);

    // SAFETY: every call forwards to the inner allocator unchanged; only a counter is updated.
    unsafe impl<A: GlobalAlloc> GlobalAlloc for Counting<A> {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            let p = unsafe { self.0.alloc(l) };
            if !p.is_null() {
                LIVE.fetch_add(l.size(), Relaxed);
            }
            p
        }
        unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
            let p = unsafe { self.0.alloc_zeroed(l) };
            if !p.is_null() {
                LIVE.fetch_add(l.size(), Relaxed);
            }
            p
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            unsafe { self.0.dealloc(p, l) };
            LIVE.fetch_sub(l.size(), Relaxed);
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
            let q = unsafe { self.0.realloc(p, l, new) };
            if !q.is_null() {
                if new >= l.size() {
                    LIVE.fetch_add(new - l.size(), Relaxed);
                } else {
                    LIVE.fetch_sub(l.size() - new, Relaxed);
                }
            }
            q
        }
    }
}

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
mod probe_retention;
mod probe_serve;
mod rpc;
mod run;
mod serving;
mod stats;
mod status;
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
    /// M0 Spike B (Sepolia only): EL peers, eth/71 BALs, snap/2 point lookups, root-servable lag.
    ProbeEl(probe_el::Args),
    /// Diagnostics (Sepolia only): CL req/resp (payload envelopes by range, light-client bootstrap).
    ProbeClReq(probe_clreq::Args),
    /// Check what a skim node serves to peers (EL and CL), verifying every answer (Sepolia only).
    ProbeServe(probe_serve::Args),
    /// How far back public EL peers serve BALs, bodies and receipts, by block age (Sepolia only).
    ProbeRetention(probe_retention::Args),
    /// Time the restart catch-up walk over consensus peers (where the time goes; Sepolia only).
    ProbeCatchup(probe_catchup::Args),
    /// M0 Spike C (Sepolia only): cold-call SNAP rounds and latency against a reference node.
    ProbeCall(probe_call::Args),
    /// M1 (Sepolia only): follow the chain from CL gossip BALs into the MVCC store; time block ->
    /// state ready -> eth_call done vs when a reference node can answer the same block.
    Follow(follow::Args),
    /// Follow the chain and serve JSON-RPC (eth_call, eth_getStorageAt, ...) on top of it. Any
    /// Glamsterdam network (`--network`); the other subcommands are Sepolia diagnostics.
    Run(run::Args),
    /// Ask a running skim how it's doing: chain, peers, light client, subscriptions, with hints.
    Status(status::Args),
    /// P3 baseline: same per-block eth_call workload against a full node's RPC.
    BenchNode(bench_node::Args),
    /// Peer-count sweep (Sepolia only): one large learned wave fetched with 1/2/4/all peers per block.
    ProbeWave(probe_call::WaveArgs),
    /// M0 Spike A (Sepolia only): CL gossip listener; execution-payload envelope timing and BAL checks.
    ProbeCl(probe_cl::Args),
}

/// `skim` alone, or with only `skim run`'s flags, runs the node: a zero-config start on the
/// default network. `--help` and `--version` still describe the whole CLI.
fn with_default_subcommand(mut argv: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let first = argv.get(1).and_then(|a| a.to_str()).map(str::to_owned);
    if first.as_deref().is_none_or(|a| a.starts_with('-') && !matches!(a, "-h" | "--help" | "-V" | "--version")) {
        argv.insert(1, "run".into());
    }
    argv
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();
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
    match Cli::parse_from(with_default_subcommand(std::env::args_os().collect())).cmd {
        Cmd::ProbeEl(args) => probe_el::run(args).await,
        Cmd::ProbeClReq(args) => probe_clreq::run(args).await,
        Cmd::ProbeServe(args) => probe_serve::run(args).await,
        Cmd::ProbeRetention(args) => probe_retention::run(args).await,
        Cmd::ProbeCatchup(args) => probe_catchup::run(args).await,
        Cmd::ProbeCall(args) => probe_call::run(args).await,
        Cmd::Follow(args) => follow::run(args).await,
        Cmd::Run(args) => run::run(args).await,
        Cmd::Status(args) => status::run(args).await,
        Cmd::BenchNode(args) => bench_node::run(args).await,
        Cmd::ProbeWave(args) => probe_call::run_wave(args).await,
        Cmd::ProbeCl(args) => probe_cl::run(args).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        with_default_subcommand(a.iter().map(Into::into).collect()).into_iter().map(|s| s.into_string().unwrap()).collect()
    }

    #[test]
    fn the_live_heap_counter_follows_allocations() {
        // Other test threads allocate too: a large allocation dominates the noise.
        let before = heap::live();
        let v = vec![1u8; 64 << 20];
        let during = heap::live();
        assert!(during >= before + (64 << 20) - (1 << 20), "{before} -> {during}");
        drop(v);
        assert!(heap::live() < during - (60 << 20));
    }

    #[test]
    fn skim_alone_or_with_only_flags_runs_the_node() {
        assert_eq!(args(&["skim"]), ["skim", "run"]);
        assert_eq!(args(&["skim", "--rpc", "127.0.0.1:9"]), ["skim", "run", "--rpc", "127.0.0.1:9"]);
        assert_eq!(args(&["skim", "probe-el"]), ["skim", "probe-el"]);
        assert_eq!(args(&["skim", "--help"]), ["skim", "--help"]);
        assert!(Cli::try_parse_from(args(&["skim"])).is_ok());
    }
}
