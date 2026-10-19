// A read-only wallet: balances for some addresses, then every change as blocks arrive.
//
//   skim run                                             # no config needed
//   cd examples && npm run watch -- 0xAddress [0xAddress …] [--token 0xERC20 …] [--no-tokens]
//
// At start: each address's ETH balance, nonce and ERC-20 balances (default tokens: Sepolia WETH
// and USDC, the ones the Uniswap examples use; --token replaces them, --no-tokens drops them).
// Then, for every block skim pushes over WebSocket (eth_subscribe newHeads):
//   - ETH balance and nonce, read at that block (2 reads per address); a nonce change means an
//     outgoing transaction, and the block's transactions from the address are listed
//   - ERC-20 Transfer logs to or from the addresses (2 eth_getLogs per block, any token), and the
//     new balance of a watched token when one of its transfers touches an address
// Every value is printed with the block it was read at. With no config, skim fetches each state
// read on demand from its peers and verifies it against the block's state root, so the reads per
// block are kept to the few above. The WebSocket reconnects on close; missed blocks are caught
// up (logs over the gap, balances at the new head).
//
// SKIM_RPC (default http://127.0.0.1:8545) and SKIM_WS (default ws://127.0.0.1:8545) override the endpoints.
import { createPublicClient, erc20Abi, formatEther, formatUnits, getAddress, http, isAddress, numberToHex, pad, toEventSelector } from 'viem'
import { sepolia } from 'viem/chains'

const RPC = process.env.SKIM_RPC ?? 'http://127.0.0.1:8545'
const WS = process.env.SKIM_WS ?? 'ws://127.0.0.1:8545'
const DEFAULT_TOKENS = [
  '0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14', // WETH (Sepolia)
  '0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238', // USDC (Sepolia)
]
const MAX_CATCHUP = 256 // blocks of logs to scan after a reconnect; older gaps are reported, not scanned
const TRANSFER = toEventSelector('Transfer(address,address,uint256)')

// ---- arguments ---------------------------------------------------------------------------------
const addresses = []
let tokens = null
for (let i = 2; i < process.argv.length; i++) {
  const a = process.argv[i]
  if (a === '--token') (tokens ??= []).push(process.argv[++i])
  else if (a === '--no-tokens') tokens = []
  else addresses.push(a)
}
tokens ??= DEFAULT_TOKENS
for (const a of [...addresses, ...tokens]) if (!isAddress(a ?? '')) fail(`not an address: ${a}`)
if (!addresses.length) fail('usage: npm run watch -- <address> [address …] [--token <erc20> …] [--no-tokens]')
const watched = addresses.map((a) => getAddress(a))
const watchedSet = new Set(watched)
tokens = tokens.map((t) => getAddress(t))

function fail(msg) {
  console.error(msg)
  process.exit(1)
}

const client = createPublicClient({ chain: sepolia, transport: http(RPC, { timeout: 60_000 }) }) // on-demand reads can wait on peers
const short = (a) => `${a.slice(0, 6)}…${a.slice(-4)}`
const signed = (v, fmt) => (v < 0n ? `-${fmt(-v)}` : `+${fmt(v)}`)

// ---- token metadata (cached; also for tokens seen in transfers but not watched) -----------------
const meta = new Map()
function tokenMeta(token) {
  if (!meta.has(token)) {
    meta.set(token, Promise.all([
      client.readContract({ address: token, abi: erc20Abi, functionName: 'symbol' }).catch(() => short(token)),
      client.readContract({ address: token, abi: erc20Abi, functionName: 'decimals' }).catch(() => 18),
    ]).then(([symbol, decimals]) => ({ symbol, decimals })))
  }
  return meta.get(token)
}

// ---- state: last values seen per address --------------------------------------------------------
const state = new Map() // address -> { balance, nonce, tokens: Map(token -> balance) }
let last = null         // last block processed

async function readAccount(address, blockNumber) {
  const [balance, nonce] = await Promise.all([
    client.getBalance({ address, blockNumber }),
    client.getTransactionCount({ address, blockNumber }),
  ])
  return { balance, nonce }
}
const readToken = (token, address, blockNumber) =>
  client.readContract({ address: token, abi: erc20Abi, functionName: 'balanceOf', args: [address], blockNumber })

async function snapshot() {
  const block = await client.getBlockNumber()
  const t0 = performance.now()
  console.log(`balances at block ${block}`)
  for (const address of watched) {
    const [{ balance, nonce }, tokenBalances] = await Promise.all([
      readAccount(address, block),
      Promise.all(tokens.map((t) => readToken(t, address, block).catch(() => null))),
    ])
    const s = { balance, nonce, tokens: new Map() }
    const parts = [`${formatEther(balance)} ETH`, `nonce ${nonce}`]
    for (const [i, t] of tokens.entries()) {
      if (tokenBalances[i] === null) continue
      s.tokens.set(t, tokenBalances[i])
      const { symbol, decimals } = await tokenMeta(t)
      parts.push(`${formatUnits(tokenBalances[i], decimals)} ${symbol}`)
    }
    state.set(address, s)
    console.log(`  ${address}  ${parts.join('  ')}`)
  }
  console.log(`  (${(performance.now() - t0).toFixed(0)} ms; first reads of an account can take a peer round trip)`)
  last = block
}

// ---- per block ---------------------------------------------------------------------------------
async function transfers(fromBlock, toBlock, blockHash) {
  const who = watched.map((a) => pad(a.toLowerCase()))
  const range = blockHash ? { blockHash } : { fromBlock: numberToHex(fromBlock), toBlock: numberToHex(toBlock) }
  const [out, inn] = await Promise.all([
    client.request({ method: 'eth_getLogs', params: [{ ...range, topics: [TRANSFER, who] }] }),
    client.request({ method: 'eth_getLogs', params: [{ ...range, topics: [TRANSFER, null, who] }] }),
  ])
  const seen = new Set()
  return [...out, ...inn]
    .filter((l) => l.topics.length === 3) // ERC-20; ERC-721 Transfer has the id as a 4th topic
    .filter((l) => !seen.has(`${l.transactionHash}:${l.logIndex}`) && seen.add(`${l.transactionHash}:${l.logIndex}`))
    .sort((a, b) => Number(BigInt(a.blockNumber) - BigInt(b.blockNumber)) || Number(BigInt(a.logIndex) - BigInt(b.logIndex)))
}

async function onHead(head) {
  const n = BigInt(head.number)
  if (last !== null && n <= last) {
    console.log(`block ${n} again (reorg): re-reading`)
    last = n - 1n
  }
  const from = last === null ? n : last + 1n
  const gap = n - from
  const t0 = performance.now()
  const lines = []

  // ERC-20 transfers touching the addresses, over the new block (or the gap since the last one).
  let logs = []
  if (gap >= BigInt(MAX_CATCHUP)) lines.push(`  skipped transfer logs for blocks ${from}..${n - 1n} (gap over ${MAX_CATCHUP})`)
  else logs = await transfers(gap ? from : n, n, gap ? null : head.hash)
  const touched = new Set() // `${token}|${address}` pairs whose balance to re-read
  for (const l of logs) {
    const token = getAddress(l.address)
    const src = getAddress(`0x${l.topics[1].slice(26)}`), dst = getAddress(`0x${l.topics[2].slice(26)}`)
    const { symbol, decimals } = await tokenMeta(token)
    const amount = formatUnits(BigInt(l.data === '0x' ? 0 : l.data), decimals)
    const dir = watchedSet.has(src) && watchedSet.has(dst) ? 'self' : watchedSet.has(dst) ? 'in ' : 'out'
    lines.push(`  ${dir} ${amount} ${symbol}  ${short(src)} -> ${short(dst)}  tx ${short(l.transactionHash)}` + (gap ? ` (block ${BigInt(l.blockNumber)})` : ''))
    for (const a of [src, dst]) if (watchedSet.has(a) && tokens.includes(token)) touched.add(`${token}|${a}`)
  }

  // ETH balance and nonce, read at this block.
  for (const address of watched) {
    const prev = state.get(address)
    const { balance, nonce } = await readAccount(address, n)
    if (balance !== prev.balance) {
      lines.push(`  ${short(address)}  ETH ${formatEther(prev.balance)} -> ${formatEther(balance)} (${signed(balance - prev.balance, formatEther)})`)
    }
    if (nonce !== prev.nonce) {
      lines.push(`  ${short(address)}  nonce ${prev.nonce} -> ${nonce}: ${nonce - prev.nonce} outgoing transaction(s)`)
      if (!gap) {
        const block = await client.getBlock({ blockHash: head.hash, includeTransactions: true })
        for (const tx of block.transactions.filter((t) => getAddress(t.from) === address)) {
          lines.push(`      tx ${tx.hash}  to ${tx.to ?? '(create)'}  ${formatEther(tx.value)} ETH`)
        }
      }
    }
    prev.balance = balance
    prev.nonce = nonce
  }

  // Watched token balances, only where a transfer touched them.
  for (const key of touched) {
    const [token, address] = key.split('|')
    const prev = state.get(address).tokens.get(token) ?? 0n
    const now = await readToken(token, address, n)
    const { symbol, decimals } = await tokenMeta(token)
    const fmt = (v) => formatUnits(v, decimals)
    if (now !== prev) lines.push(`  ${short(address)}  ${symbol} ${fmt(prev)} -> ${fmt(now)} (${signed(now - prev, fmt)})`)
    state.get(address).tokens.set(token, now)
  }

  last = n
  const span = gap ? `blocks ${from}..${n}` : `block ${n}`
  if (lines.length) console.log(`${span}  (read at block ${n}, ${(performance.now() - t0).toFixed(0)} ms)\n${lines.join('\n')}`)
  else if (process.env.WATCH_QUIET === undefined) console.log(`${span}  no change  (${(performance.now() - t0).toFixed(0)} ms)`)
}

// ---- WebSocket: newHeads, with reconnect --------------------------------------------------------
// Heads are processed one at a time, in order; a head that arrives while one is in progress waits.
let queue = Promise.resolve()
let backoff = 1_000

function connect() {
  const ws = new WebSocket(WS)
  ws.onopen = () => {
    backoff = 1_000
    ws.send(JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'eth_subscribe', params: ['newHeads'] }))
  }
  ws.onmessage = (ev) => {
    const msg = JSON.parse(ev.data)
    if (msg.id === 1) {
      if (msg.error) console.error(`eth_subscribe failed: ${msg.error.message}`)
      else console.log(`following new blocks over ${WS}`)
      return
    }
    if (msg.method !== 'eth_subscription') return
    const head = msg.params.result
    queue = queue.then(() => onHead(head)).catch((e) => console.error(`block ${BigInt(head.number)}: ${e.shortMessage ?? e.message}`))
  }
  // Node's WebSocket fires only `error` (no `close`) when a connection attempt fails, so both
  // events lead here, once per socket.
  let retrying = false
  ws.onerror = ws.onclose = () => {
    if (retrying) return
    retrying = true
    console.error(`WebSocket to ${WS} closed; reconnecting in ${backoff / 1000} s`)
    setTimeout(connect, backoff)
    backoff = Math.min(backoff * 2, 10_000)
  }
}

try {
  await snapshot()
} catch (e) {
  fail(e.name === 'HttpRequestError' ? `no answer from ${RPC} (is skim running? SKIM_RPC overrides)` : (e.shortMessage ?? e.message))
}
connect()
