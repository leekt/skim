# Optional EVM2 execution experiment

Skim can use the EVM2 interpreter for plain `eth_call` and the warm calls made by
`eth_estimateGas`. The default remains revm. This is an opt-in experiment pinned to
`alloy-rs/evm2` revision `e06fb4b39994923a3d52ef978cf603301f6fb54c`; its API is still
changing upstream.

```sh
cargo build --release -p skim-cli --features evm2
./target/release/skim run --evm-backend evm2 --no-call-cache
```

Supply the same normal chain/config/checkpoint arguments used for a revm run.
`--evm-backend evm2` on a build without that feature fails at startup. Rust 1.97
(the repository toolchain) meets the pinned dependency's Rust 1.96 requirement; the
workspace's declared `rust-version` is still 1.95, which is enough only without the feature.
The lockfile is shared by every build: adding the dependency moved `ruint` from 1.20.1 to
1.21.1 for default (revm-only) builds too, and Cargo resolves (and normally fetches) the
optional git dependency even when the feature is off, which matters for offline or vendored
builds.

## Integration boundary

`Simulator::execute` selects the backend. Both use the same MVCC snapshot,
branch-aware `BLOCKHASH` lookup, access/miss recording, parent-root resolution,
state proofs, learned predictions, batched fetching, round limits and retention.
Missing-state placeholders are still discarded by `call_retained`; they are never
returned as an answer. EVM2 has its own bounded 4096-entry analyzed bytecode cache,
using the existing clock eviction policy and code hashes as keys.

The bridge carries legacy, EIP-2930, EIP-1559, blob and set-code transaction fields,
including signed or recovered authorizations, and preserves Amsterdam state gas,
refunds, floor gas and native transfer logs. Unsupported envelope shapes, EVM2
validation errors and stop reasons the bridge cannot represent use revm.
Malformed-precompile results also use revm: EVM2's generic `PrecompileError` loses
error text that revm exposes in its contextual halt reason.

Traces, access-list generation, state/block overrides and `eth_simulateV1` continue
through their existing revm paths. There is no stateful EVM2 commit path here.
Selecting EVM2 changes every caller of `Simulator::execute`, not only `eth_call`: the
first, resolving run of `eth_estimateGas` as well as its warm probes, the per-block call
subscriptions (whose required key sets come from the accounts and slots a run reads), and the
Uniswap V3 pool-registration probes. A difference in what EVM2 reads would therefore change
which keys subscriptions keep current, not just a call's answer.

## Offline parity and CPU measurements

```sh
cargo test -p skim-sim --features evm2
cargo check -p skim-cli --features evm2
cargo test --release -p skim-sim --features evm2 --test evm2_parity \
  benchmark_warm_calls_and_estimates -- --ignored --nocapture
```

The differential fixtures compare exact execution results (including gas and logs)
and recorded accounts, storage, missing keys and block hashes. They cover
success, revert, representative halts, out of gas, reads, writes, state creation,
refunds, contract creation, transaction types 0–4, precompiles, CHAINID, BASEFEE,
BLOBBASEFEE and native value-transfer logs. Existing header-only miss tests select
EVM2 when compiled with the feature, checking bounded missing-header failures and
header resolution without waiting for SNAP peers. Existing tracing/simulation
regressions remain enabled.

The paired benchmark uses synthetic bytecode against a warm local Store. It
includes backend setup, access recording and bytecode-cache lookup. One fixture
performs 1000 arithmetic/jump iterations; the other reads one storage slot. The
estimate case performs the same success binary search as the RPC handler. Seven
samples alternate backend order. It uses full LTO with one codegen unit, Apple M1
Max and Rust 1.97's automatic packed dispatch backend.

The final sweep ran in a coordinated quiet window, with sibling builds and
benchmarks paused. Median microseconds per operation:

| Fixture | Operation | revm | EVM2 | Ratio |
| --- | --- | ---: | ---: | ---: |
| 1000 arithmetic iterations | call | 31.335 | 18.998 | 1.649× |
| 1000 arithmetic iterations | estimate | 682.499 | 413.381 | 1.651× |
| one SLOAD | call | 2.277 | 1.747 | 1.303× |
| one SLOAD | estimate | 52.803 | 40.192 | 1.314× |

The complete seven-sample arrays and environment are in
`evm2-offline-2026-10-27.txt`. These are CPU fixtures, not live Uniswap quotes or
end-to-end RPC throughput. Result-cache hits and SNAP/network waits can reduce
the application-level gain. The fixture directly populates a local Store; proof
acquisition and verification are outside this benchmark.

Validation completed with 31 default-feature `skim-sim` tests, 36 with `evm2`, and the
two additional async laboratory regressions.

**Not yet covered** (needed before running `--evm-backend evm2` on a production node): the
`Simulator::execute` fallback to revm (validation errors, unsupported stop reasons), unknown
code and unknown accounts (only an unknown storage slot is a fixture), precompiles that compute
(KZG, BLS12-381, P256VERIFY: EVM2 builds them on different crypto libraries than revm),
header opcodes (PREVRANDAO, COINBASE, TIMESTAMP, NUMBER, GASLIMIT, GASPRICE, ORIGIN), and
maximum call depth (EVM2 recurses on the native stack: roughly 700 reachable frames under the
2^24 regular-gas cap fit worker stacks in release builds, but not necessarily in debug builds).
The resume test counts distinct slots fetched, so it can't yet tell resuming from restarting. `cargo check -p skim-cli --features
evm2` and `git diff --check` passed.

## Resumable state-miss laboratory

```sh
cargo test -p skim-sim --features evm2-async --test evm2_resume
cargo test -p skim-sim --features evm2-async --test evm2_resume \
  -- --ignored --nocapture
```

The optional `evm2-async` feature enables a controlled test harness, not a production
SNAP resolver. It checks that an execution resumes across three storage awaits,
returns the expected result, discards its sender nonce, and discards the incomplete
transaction on cancellation. The timing experiment assigns each missing slot a
synthetic 10 ms wait. One run took about 43 ms with demand reads, versus 13 ms for
three concurrent waits. This verifies the tradeoff, not a live SNAP latency claim:
resuming can save repeated CPU prefixes while losing batching across independent
misses.

Production resolution retains its current predictor and proof-verified fetch
waves. An async production adapter needs bounded look-ahead/batching, retention,
archive refusal, reorg handling, cancellation and CPU-admission behavior at every
suspension boundary before it should replace those waves. In particular, a request
must not hold a CPU permit while awaiting the network. The library's native fiber
stack reserves 16 MiB of address space per suspended execution, another admission
limit to evaluate under concurrent cold requests.
