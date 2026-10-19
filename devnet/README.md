# Devnet

A local Glamsterdam devnet (Kurtosis and ethPandaOps' ethereum-package) to test skim on a network
other than Sepolia, including the `minimal` consensus preset.

Needs Docker and [Kurtosis](https://docs.kurtosis.com/install). On macOS with Docker Desktop,
export `DOCKER_HOST=unix://$HOME/.docker/run/docker.sock` if Kurtosis can't find Docker.

```
devnet/run.sh <skim linux binary>                 # devnet up (if needed), skim in a container on it
docker logs -f skim-devnet-node                   # skim's log
python3 -I devnet/check.py <EL rpc port>          # skim's answers against a devnet node
kurtosis enclave inspect skim-devnet              # the devnet's services and mapped ports
kurtosis enclave rm -f skim-devnet                # tear down
```

- **The binary** must be a Linux build for the Docker host's architecture: the release workflow's
  artifact, or a `cargo build` inside a Linux container.
- **Why a container:** the devnet's beacon nodes (Lodestar) listen on QUIC only, and Kurtosis
  doesn't map that UDP port to the host. On the devnet's network skim reaches them directly.
- **The network config** comes from the enclave's `el_cl_genesis_data` artifact
  (`devnet/out/net`): exactly what `skim run --network <dir>` reads.

Verified (2026-10-19, `ec5ced9`+), on the minimal preset:
- the light client bootstraps from the devnet's finalized root and rotates sync committee periods;
- blocks apply ~0.54 s after their slot;
- balances and an `eth_call` (state fetched over SNAP from the devnet's nodes) match geth at the
  same block hash.
