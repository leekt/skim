#!/usr/bin/env bash
# Runs a skim Linux binary against the local Kurtosis devnet, as a container on the devnet's
# Docker network (beacon nodes there may listen on QUIC only, which isn't reachable from the host).
#
#   devnet/run.sh <skim linux binary> [extra skim args...]
#   ENCLAVE=skim-bpo PARAMS=network_params.bpo.yaml SKIM_RPC_PORT=8601 devnet/run.sh ...   (a second devnet)
#   CONFIG=skim.yaml devnet/run.sh ...   (an app config: subscriptions etc.)
#
# Starts the devnet if the enclave doesn't exist, exports its network config to devnet/out/<enclave>/net,
# then runs skim with the devnet's EL nodes as trusted peers and its finalized root as the
# light-client checkpoint. CL peers come from discovery (bootstrap_nodes.txt).
# JSON-RPC: http://127.0.0.1:${SKIM_RPC_PORT:-8600}. Logs: docker logs -f skim-<enclave>-node
set -euo pipefail
BIN=$(cd "$(dirname "$1")" && pwd)/$(basename "$1"); shift
HERE=$(cd "$(dirname "$0")" && pwd)
ENCLAVE=${ENCLAVE:-skim-devnet}
PARAMS=${PARAMS:-network_params.yaml}
OUT=$HERE/out/$ENCLAVE
PORT=${SKIM_RPC_PORT:-8600}
NAME=skim-$ENCLAVE-node

# Start the devnet only if the enclave doesn't exist: re-running the package into an existing
# enclave would replace the chain (a new genesis).
if ! kurtosis enclave ls 2>/dev/null | awk '{print $2}' | grep -qx "$ENCLAVE"; then
  kurtosis run --enclave "$ENCLAVE" github.com/ethpandaops/ethereum-package --args-file "$HERE/$PARAMS"
fi
kurtosis enclave inspect "$ENCLAVE" >/dev/null || { echo "kurtosis can't inspect enclave $ENCLAVE (see above); not touching it" >&2; exit 1; }
mkdir -p "$OUT/bin"
kurtosis files download "$ENCLAVE" el_cl_genesis_data "$OUT/net" >/dev/null
cp "$BIN" "$OUT/bin/skim"
CONFIG_ARGS=()
if [ -n "${CONFIG:-}" ]; then
  cp "$CONFIG" "$OUT/bin/skim.yaml"
  CONFIG_ARGS=(--config /b/skim.yaml)
fi

# Mapped localhost ports of the first EL and CL (for identities and the checkpoint).
inspect=$(kurtosis enclave inspect "$ENCLAVE")
port_of() { echo "$inspect" | awk -v svc="$1" -v p="$2" '$2 ~ "^"svc {f=1} f && $0 ~ p {match($0, /127\.0\.0\.1:[0-9]+/); print substr($0, RSTART+10, RLENGTH-10); exit}'; }
BEACON=$(port_of cl-1- "http: 4000")

# The package's bootstrap_nodes.txt names an external bootnode; skim discovers the local beacon
# nodes from their own ENRs instead (QUIC-only on Lodestar).
: > "$OUT/net/bootstrap_nodes.txt"
for svc in $(echo "$inspect" | awk '$2 ~ /^cl-[0-9]+-/ {print $2}'); do
  http=$(port_of "$svc" "http: 4000")
  curl -s "localhost:$http/eth/v1/node/identity" | python3 -c "import json,sys; print(json.load(sys.stdin)['data']['enr'])" >> "$OUT/net/bootstrap_nodes.txt"
done
CHECKPOINT=$(curl -s "localhost:$BEACON/eth/v1/beacon/headers/finalized" | python3 -c "import json,sys; print(json.load(sys.stdin)['data']['root'])")

# Every EL node as a trusted peer, at its address on the devnet network.
PEERS=()
for svc in $(echo "$inspect" | awk '$2 ~ /^el-[0-9]+-/ {print $2}'); do
  rpc=$(port_of "$svc" "rpc: 8545")
  enode=$(curl -s "localhost:$rpc" -H 'content-type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"admin_nodeInfo","params":[]}' \
    | python3 -c "import json,sys; print(json.load(sys.stdin)['result']['enode'].split('?')[0])")
  PEERS+=(--trusted-peer "$enode")
done

NET=$(docker network ls --format '{{.Name}}' | grep "^kt-$ENCLAVE$")
docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" --network "$NET" -p "127.0.0.1:$PORT:8545" -e RUST_LOG="${RUST_LOG:-info}" \
  -v "$OUT/bin:/b:ro" -v "$OUT/net:/net:ro" ubuntu:24.04 \
  sh -c 'cp /b/skim /usr/local/bin/skim && chmod +x /usr/local/bin/skim && exec skim "$@"' -- \
  run --network /net --rpc 0.0.0.0:8545 --data-dir /tmp/skim --checkpoint "$CHECKPOINT" "${PEERS[@]}" ${CONFIG_ARGS[@]+"${CONFIG_ARGS[@]}"} "$@"
echo "skim on the devnet: http://127.0.0.1:$PORT  (docker logs -f $NAME)"
