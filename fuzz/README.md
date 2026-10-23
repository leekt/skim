# Fuzzing skim's untrusted-input parsers

Everything skim reads from the P2P network is attacker-controlled. These targets feed arbitrary
bytes to each parser and fail on any panic, sanitizer error, or memory blow-up.

| Target | What it parses |
|---|---|
| `envelope` | gossiped / requested `SignedExecutionPayloadEnvelope`, header rebuild + hash, BAL decode |
| `beacon_block` | gossiped `SignedBeaconBlock` |
| `bal` | EIP-7928 block access lists (gossip, eth/71) |
| `rpc_chunks` | eth2 req/resp framing: chunked responses, snappy payloads, Status, MetaData |
| `light_client` | light-client bootstrap, optimistic and finality updates (incl. BLS paths) |
| `snap_proof` | SNAP Merkle proofs and proof-less trie rebuilds |
| `snap1_response` | snap/1 responses decoded by skim's own protocol handler |

```bash
cargo install cargo-fuzz
cd fuzz
cargo +nightly fuzz run -O envelope seeds/envelope -- -max_total_time=300 -rss_limit_mb=4096
cargo +nightly fuzz run -O snap_proof regressions/snap_proof/* -- -runs=0        # replay past crashes
```

The seven targets match the binaries in `fuzz/Cargo.toml`; fuzzing is a separate nightly workspace. CI replays checked-in regression inputs with `-runs=0`. The ordinary Rust workspace tests use the toolchain from `rust-toolchain.toml` and `cargo test --workspace --profile ci --locked`.

**First run (5 min per target):**
- 1–44 M inputs per target.
- One crash found: a SNAP response repeating a slot hash made the trie builder panic. Any peer could trigger it.
- Fixed: `proof::root_of` now rejects repeated keys. Regression in `regressions/snap_proof`, plus a unit test.
