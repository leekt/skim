# Getting started

In about five minutes you'll have skim following Sepolia and answering `eth_call` locally, with your app's state kept ready.

## 1. Build
```bash
git clone https://github.com/leekt/skim && cd skim
cargo build --release            # Rust 1.97, pinned by rust-toolchain.toml
```
The binary is `./target/release/skim`. Nothing else is needed: no node, no database server, no API key.

## 2. Run
```bash
./target/release/skim run --config skim.example.yaml
```

| Port | Protocol | What |
|---|---|---|
| `127.0.0.1:8545` | HTTP | JSON-RPC (`--rpc`), and the dashboard at `/dashboard` |
| `9130` TCP + UDP, `9131` UDP | libp2p / discv5 / QUIC | consensus-layer gossip (`--cl-port`) |
| `30388` TCP + UDP | RLPx / discv4 | execution-layer peers (`--el-port`) |

Inbound connections aren't required, because skim dials out. Opening the P2P ports helps it find peers faster.

**What you'll see in the log:**
```text
INFO skim::run: following Sepolia from CL gossip; RPC answers once the first block is applied
INFO skim::rpc: JSON-RPC listening addr=127.0.0.1:8545
INFO skim::run: state ready number=11919448 changed=12 …     # one line per block, ~every 12 s
INFO skim_el: session peer=… client=Geth/v1.17.7 … snap2=false
```
- **First block:** the first `state ready` arrives within a slot or two of starting. From then on `eth_blockNumber` answers.
- **Peers:** SNAP-serving EL peers take **1–4 minutes** to connect on public Sepolia. Until they do, reads of state skim hasn't seen yet answer `-32002` (unavailable). skim never answers with a guessed value.

## 3. Use it like any node
```bash
cast block-number --rpc-url http://127.0.0.1:8545
cast call 0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14 "totalSupply()(uint256)" --rpc-url http://127.0.0.1:8545
```

```js
import { createPublicClient, http } from 'viem'
import { sepolia } from 'viem/chains'
const client = createPublicClient({ chain: sepolia, transport: http('http://127.0.0.1:8545') })
await client.getBlockNumber()
```

- The **first** read of any account or slot fetches it over SNAP, verified with a Merkle proof: a few hundred ms.
- **After that** it is local and kept current from every block's access list: 0.5–6 ms per `eth_call`.

## 4. Tell skim what your app needs
Everything works on demand. Declaring your app's state makes it **always ready**: no fetch on the hot path, ever. Copy `skim.example.yaml` and edit the `subscriptions` list:

```yaml
state:
  subscriptions:
    - name: my-pool
      address: "0x6Ce0896eAE6D4BD668fDe41BB784548fb8F59b50"
      account: true
      storage: ["0x0", "0x4"]
    - name: my-quote                    # don't know the slots? subscribe the call itself
      calls:
        - to: "0xEd1f6473345F45b75F8179591dd5bA1888cf2FB3"
          data: "0xc6a5026a…"
      on_block: true
```

Check readiness:
```bash
curl -s localhost:8545 -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"skim_subscriptionStatus","params":[]}'
```
```json
[{"name":"my-pool","ready":true,"block":11919451,"required_keys":5,"missing_keys":0, …}]
```

**Not sure what to subscribe?** Run your app against skim and open `/dashboard` on skim's RPC address (default `127.0.0.1:8545`), tab **Addresses**:
- It lists every contract your calls read, and whether a subscription pins it.
- It shows how many keys had to be fetched from the network.
- It suggests a `skim.yaml` block for the hot ones that aren't pinned.

## Next
- [Uniswap V3 and V4 examples (viem)](../../examples): complete, runnable configs and scripts
- [How skim works](how-it-works.md)
- [Configuration reference](configuration.md)
- [JSON-RPC reference](json-rpc.md)
- [Archiving history for your state](archive.md)
- [Trust and correctness](trust-and-correctness.md)
- [Troubleshooting](troubleshooting.md)
