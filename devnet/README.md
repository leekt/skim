# Devnet

A local Glamsterdam devnet (Kurtosis and ethPandaOps' ethereum-package) to test skim on a network
other than Sepolia, including the `minimal` consensus preset.

Needs Docker and [Kurtosis](https://docs.kurtosis.com/install). On macOS with Docker Desktop,
export `DOCKER_HOST=unix://$HOME/.docker/run/docker.sock` if Kurtosis can't find Docker.

```
devnet/run.sh <skim linux binary> [extra skim args...]  # devnet up (if needed), skim in a container
docker logs -f skim-skim-devnet-node              # skim's log
python3 -I devnet/check.py <EL rpc port> [skim rpc port] [enclave]  # defaults: 8600, skim-devnet
kurtosis enclave inspect skim-devnet              # the devnet's services and mapped ports
kurtosis enclave rm -f skim-devnet                # tear down
```

- **The binary** must be a Linux build for the Docker host's architecture: the release workflow's
  artifact, or a `cargo build` inside a Linux container.
- **Why a container:** the devnet's beacon nodes (Lodestar) listen on QUIC only, and Kurtosis
  doesn't map that UDP port to the host. On the devnet's network skim reaches them directly.
- **The network config** comes from the enclave's `el_cl_genesis_data` artifact
  (`devnet/out/<enclave>/net`): exactly what `skim run --network <dir>` reads.
- **Defaults:** enclave `skim-devnet`, `network_params.yaml`, skim RPC on `127.0.0.1:8600`.
  `ENCLAVE`, `PARAMS` (relative to `devnet/`) and `SKIM_RPC_PORT` select separate runs; `CONFIG`
  passes an app config (subscriptions; see [state edge cases](#state-edge-cases)) and `RUST_LOG`
  sets skim's log filter (default `info`). Extra arguments after the binary go to `skim run`. Each
  launch replaces skim's container and its container-local `/tmp/skim` restart data; it does
  not replace the existing devnet chain.
- **Bootstrap:** the runner uses the devnet beacon node's finalized root as `--checkpoint`
  and its EL nodes as trusted peers. The app YAML does not select the chain; see
  [network configuration](../docs/guide/configuration.md#networks).

Verified (2026-10-19, `82131c4`), on the minimal preset, with peers found by discovery alone:
- the light client bootstraps from the devnet's finalized root and follows sync committee periods;
- blocks apply ~0.53 s after their slot;
- `devnet/check.py`: balances, the deposit contract's code and an `eth_call` (state fetched over
  SNAP from the devnet's nodes) match geth at the same block hash.

Found along the way:
- **The package's `bootstrap_nodes.txt` names an external bootnode**, so `run.sh` writes the local
  beacon nodes' ENRs instead.
- **Lodestar's devnet build answers `na` to yamux over TCP.** skim now dials QUIC first when a peer
  announces it.
- **Re-running the package into an existing enclave replaces the chain** (a new genesis), so
  `run.sh` never does that.

## Scheduled blob-parameter fork check

`network_params.bpo.yaml` schedules a blob-parameter fork at epoch 12 after Gloas at epoch 4.
Use a separate enclave and port so the chain starts with that schedule:

```bash
ENCLAVE=skim-bpo PARAMS=network_params.bpo.yaml SKIM_RPC_PORT=8601 \
  devnet/run.sh /path/to/skim-linux
python3 -I devnet/check.py <EL rpc port> 8601 skim-bpo
```

A live crossing was verified on 2026-10-19 (`162eb48`): all CL peers kept through the fork and after
the old topics were left, head advancing, `check.py` all matching ([M7 log](../docs/experiments/m7-gates-and-acquisition.md)).
New EL sessions after the fork need `a1ac222` or later (the fork id; see [state edge cases](#state-edge-cases)).
The RPC comparison checks state at one block; for a crossing, also watch light-client status and
head/peer progression.

## State edge cases
`examples/devnet-edgecases.mjs` compares skim (applying BALs) with an executing node, key by key, at the same block hash, over the cases Nethermind's BAL catch-up tests ([PR 14157](https://github.com/NethermindEth/nethermind/pull/14157)): account creation, a touched missing account, create + selfdestruct in one transaction, slot clearing, EIP-7702 delegation set and cleared (with storage written through the delegation), and a reverted call with value. skim tracks every key before the transactions land, so its values come from BALs, not from a fetch afterwards.

```
cd examples
node devnet-edgecases.mjs plan http://<el rpc> <dir>       # addresses (from the devnet's prefunded keys) and <dir>/skim.yaml
CONFIG=<dir>/skim.yaml ../devnet/run.sh <skim binary>      # skim subscribed to them
node devnet-edgecases.mjs run http://<el rpc> http://127.0.0.1:8600 <dir>
```
With `ENCLAVE`/`SKIM_RPC_PORT` set for another devnet (the result below used the BPO one), set them on the `run.sh` line too and use that port.

**Result (2026-10-19, build `a1ac222`, the BPO devnet, Nethermind as the reference):** all 11 steps match on balance, nonce, code and four storage slots of each of the 9 accounts. The values move as the cases intend: D's code is `0xef0100…` after delegating and `0x` after clearing, with its slot 3 written through the delegation kept; the self-destructed contract has no code and its beneficiary holds the 1,000 wei; the reverted call's write isn't there.

**Found on the way:** on this devnet (a blob-parameter fork after Amsterdam) skim's EL fork id matched no peer once both forks were active: reth folds a genesis's forks in mainnet's order. Nethermind closed each session as a protocol breach, then refused the address at the handshake for a while. Fixed in `a1ac222` (forks in activation order). If a container keeps failing with `ECIES ... UnreadableStream` after such a run, its address is still refused: start skim from another one.

