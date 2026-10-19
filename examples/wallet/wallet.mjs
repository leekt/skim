// A small CLI wallet on top of skim: encrypted keystores on disk, signing in this process, and
// skim as the only Ethereum endpoint. A demo, not audited key management (see the README).
//
//   npm run wallet -- new                          create a key, store it encrypted
//   npm run wallet -- import < key.txt             import a key from stdin (or WALLET_PRIVATE_KEY)
//   npm run wallet -- list                         keystores in the wallet directory
//   npm run wallet -- address                      the selected account's address
//   npm run wallet -- balance [token]              ETH balance, or an ERC-20 balance
//   npm run wallet -- send <to> <amount> [--token <erc20>] [--yes]
//
// Options: --from <address|index> picks the account (default: the first keystore; also
// WALLET_FROM). Environment: WALLET_DIR (default ~/.skim-wallet), WALLET_PASSWORD (otherwise
// prompted with echo off), SKIM_RPC (default http://127.0.0.1:8545).
//
// skim holds no keys. Everything `send` needs comes from standard JSON-RPC: eth_chainId,
// eth_getTransactionCount('pending'), eth_feeHistory + eth_maxPriorityFeePerGas, eth_estimateGas,
// then eth_sendRawTransaction and eth_getTransactionReceipt.
import { chmodSync, mkdirSync, readdirSync, readFileSync, writeFileSync, openSync, closeSync } from 'node:fs'
import { homedir } from 'node:os'
import { join } from 'node:path'
import tty from 'node:tty'
import {
  createPublicClient, encodeFunctionData, erc20Abi, formatEther, formatGwei, formatUnits, getAddress,
  http, isAddress, parseEther, parseUnits,
} from 'viem'
import { generatePrivateKey, privateKeyToAccount } from 'viem/accounts'
import { decryptKeystore, encryptKeystore, keystoreFileName } from './keystore.mjs'

const RPC = process.env.SKIM_RPC ?? 'http://127.0.0.1:8545'
const DIR = process.env.WALLET_DIR ?? join(homedir(), '.skim-wallet')

// ---- arguments ---------------------------------------------------------------------------------
const positional = []
const flags = {}
for (let i = 2; i < process.argv.length; i++) {
  const a = process.argv[i]
  if (a === '--yes' || a === '-y') flags.yes = true
  else if (a === '--token' || a === '--from') flags[a.slice(2)] = process.argv[++i]
  else if (a === '--help' || a === '-h') flags.help = true
  else positional.push(a)
}
const [command, ...args] = positional

function die(msg) {
  console.error(`error: ${msg}`)
  process.exit(1)
}

// ---- terminal input (echo off for secrets) -----------------------------------------------------
// Prompts read the controlling terminal (/dev/tty), not stdin, so `import` can take the key on
// stdin and still ask for a password.
function openTty() {
  try {
    const fd = openSync('/dev/tty', 'r+')
    return { input: new tty.ReadStream(fd), output: new tty.WriteStream(fd), fd }
  } catch {
    return null
  }
}

async function prompt(question, { hidden = false } = {}) {
  const t = openTty()
  if (!t) die(`no terminal to ask "${question.trim()}" (set WALLET_PASSWORD, or pass --yes for confirmations)`)
  t.output.write(question)
  t.input.setRawMode(true)
  t.input.resume()
  try {
    return await new Promise((resolve, reject) => {
      let line = ''
      t.input.on('data', (chunk) => {
        for (const ch of chunk.toString('utf8')) {
          if (ch === '\r' || ch === '\n') return resolve(line)
          if (ch === '\u0003') return reject(new Error('interrupted'))           // Ctrl-C
          if (ch === '\u0004' && !line) return reject(new Error('end of input')) // Ctrl-D
          if (ch === '\u007f' || ch === '\b') {
            if (line) { line = line.slice(0, -1); if (!hidden) t.output.write('\b \b') }
            continue
          }
          line += ch
          if (!hidden) t.output.write(ch)
        }
      })
    })
  } finally {
    t.output.write('\n')
    t.input.setRawMode(false)
    t.input.destroy()
    t.output.destroy()
    try { closeSync(t.fd) } catch {}
  }
}

async function password({ confirm = false } = {}) {
  if (process.env.WALLET_PASSWORD !== undefined) return process.env.WALLET_PASSWORD
  const pw = await prompt('password: ', { hidden: true })
  if (confirm && (await prompt('repeat password: ', { hidden: true })) !== pw) die('passwords differ')
  if (confirm && pw.length < 8) console.error('warning: password shorter than 8 characters')
  return pw
}

async function readStdin() {
  const chunks = []
  for await (const c of process.stdin) chunks.push(c)
  return Buffer.concat(chunks).toString('utf8').trim()
}

// ---- keystore directory ------------------------------------------------------------------------
function keystores() {
  let files
  try { files = readdirSync(DIR).filter((f) => f.startsWith('UTC--')).sort() } catch { return [] }
  return files.map((file) => {
    const json = JSON.parse(readFileSync(join(DIR, file), 'utf8'))
    return { file, address: getAddress(`0x${json.address}`), json }
  })
}

function selected() {
  const all = keystores()
  if (!all.length) die(`no keystores in ${DIR} (run \`new\` or \`import\`)`)
  const want = flags.from ?? process.env.WALLET_FROM
  if (want === undefined) return all[0]
  const ks = /^\d+$/.test(want) ? all[Number(want)] : all.find((k) => k.address.toLowerCase() === want.toLowerCase())
  if (!ks) die(`no keystore for ${want}`)
  return ks
}

function save(privateKey, pw) {
  const json = encryptKeystore(privateKey, pw)
  const address = getAddress(`0x${json.address}`)
  if (keystores().some((k) => k.address === address)) die(`${address} is already in ${DIR}`)
  mkdirSync(DIR, { recursive: true, mode: 0o700 })
  chmodSync(DIR, 0o700)
  const path = join(DIR, keystoreFileName(address))
  writeFileSync(path, JSON.stringify(json), { mode: 0o600, flag: 'wx' })
  console.log(`${address}\n  saved ${path}`)
}

async function unlock() {
  const ks = selected()
  const pw = await password()
  try {
    return privateKeyToAccount(decryptKeystore(ks.json, pw))
  } catch (e) {
    die(e.message)
  }
}

// ---- chain -------------------------------------------------------------------------------------
const client = createPublicClient({ transport: http(RPC, { timeout: 60_000 }) }) // on-demand reads can wait on peers

async function tokenInfo(token) {
  if (!isAddress(token)) die(`not an address: ${token}`)
  const [symbol, decimals] = await Promise.all([
    client.readContract({ address: token, abi: erc20Abi, functionName: 'symbol' }).catch(() => '?'),
    client.readContract({ address: token, abi: erc20Abi, functionName: 'decimals' }),
  ])
  return { address: getAddress(token), symbol, decimals }
}

/** EIP-1559 fees: next block's base fee from eth_feeHistory, tip from eth_maxPriorityFeePerGas
 *  (or the recent median reward, if higher). maxFee leaves room for two full base-fee rises. */
async function fees() {
  const [history, tip] = await Promise.all([
    client.request({ method: 'eth_feeHistory', params: ['0x5', 'latest', [50]] }),
    client.request({ method: 'eth_maxPriorityFeePerGas' }),
  ])
  const nextBase = BigInt(history.baseFeePerGas.at(-1))
  const rewards = (history.reward ?? []).map((r) => BigInt(r[0])).sort((a, b) => (a < b ? -1 : a > b ? 1 : 0))
  const median = rewards.length ? rewards[rewards.length >> 1] : 0n
  const priority = BigInt(tip) > median ? BigInt(tip) : median
  return { nextBase, maxPriorityFeePerGas: priority, maxFeePerGas: 2n * nextBase + priority }
}

// ---- commands ----------------------------------------------------------------------------------
const commands = {
  async new() {
    save(generatePrivateKey(), await password({ confirm: true }))
  },

  async import() {
    if (args.length) die('the key is not taken from arguments (they end up in shell history): use WALLET_PRIVATE_KEY or stdin')
    let key = process.env.WALLET_PRIVATE_KEY
    if (!key) key = process.stdin.isTTY ? await prompt('private key: ', { hidden: true }) : await readStdin()
    key = key.trim()
    if (!key.startsWith('0x')) key = `0x${key}`
    if (!/^0x[0-9a-fA-F]{64}$/.test(key)) die('expected a 32-byte hex private key')
    save(key.toLowerCase(), await password({ confirm: true }))
  },

  async list() {
    const all = keystores()
    if (!all.length) return console.log(`no keystores in ${DIR}`)
    all.forEach((k, i) => console.log(`${i}  ${k.address}  ${k.file}`))
  },

  async address() {
    console.log(selected().address)
  },

  async balance() {
    const { address } = selected()
    const block = await client.getBlockNumber()
    const token = args[0] ?? flags.token
    if (token) {
      const t = await tokenInfo(token)
      const bal = await client.readContract({ address: t.address, abi: erc20Abi, functionName: 'balanceOf', args: [address], blockNumber: block })
      console.log(`${address}  ${formatUnits(bal, t.decimals)} ${t.symbol}  (block ${block})`)
    } else {
      const bal = await client.getBalance({ address, blockNumber: block })
      console.log(`${address}  ${formatEther(bal)} ETH  (block ${block})`)
    }
  },

  async send() {
    const [to, amount] = args
    if (!to || !isAddress(to) || amount === undefined) die('usage: send <to> <amount> [--token <erc20>] [--yes]')
    const recipient = getAddress(to)
    const from = selected().address

    // Everything the transaction needs, from skim.
    const token = flags.token ? await tokenInfo(flags.token) : null
    const tx = token
      ? { to: token.address, value: 0n, data: encodeFunctionData({ abi: erc20Abi, functionName: 'transfer', args: [recipient, parseUnits(amount, token.decimals)] }) }
      : { to: recipient, value: parseEther(amount), data: undefined }
    const [chainId, nonce, fee, ethBalance] = await Promise.all([
      client.request({ method: 'eth_chainId' }).then(Number),
      client.request({ method: 'eth_getTransactionCount', params: [from, 'pending'] }).then(Number),
      fees(),
      client.getBalance({ address: from }),
    ])
    if (token) {
      const bal = await client.readContract({ address: token.address, abi: erc20Abi, functionName: 'balanceOf', args: [from] })
      if (bal < parseUnits(amount, token.decimals)) die(`balance ${formatUnits(bal, token.decimals)} ${token.symbol} is less than ${amount}`)
    }
    const gas = BigInt(await client.request({
      method: 'eth_estimateGas',
      params: [{ from, to: tx.to, value: `0x${tx.value.toString(16)}`, ...(tx.data && { data: tx.data }) }],
    }))
    const maxCost = tx.value + gas * fee.maxFeePerGas
    if (ethBalance < maxCost) die(`balance ${formatEther(ethBalance)} ETH is less than value + max fee ${formatEther(maxCost)} ETH`)

    console.log(
      [
        `from      ${from}`,
        `to        ${recipient}`,
        token ? `amount    ${amount} ${token.symbol}  (token ${token.address})` : `amount    ${amount} ETH`,
        `chain id  ${chainId}   nonce ${nonce}   gas ${gas}`,
        `fees      base ~${formatGwei(fee.nextBase)} gwei, tip ${formatGwei(fee.maxPriorityFeePerGas)} gwei, max ${formatGwei(fee.maxFeePerGas)} gwei`,
        `max cost  ${formatEther(maxCost)} ETH  (balance ${formatEther(ethBalance)} ETH)`,
      ].join('\n'),
    )
    if (!flags.yes) {
      const answer = (await prompt('send? [y/N] ')).trim().toLowerCase()
      if (answer !== 'y' && answer !== 'yes') return console.log('not sent')
    }

    const account = await unlock()
    if (account.address !== from) die('keystore address does not match its key')
    const raw = await account.signTransaction({
      type: 'eip1559', chainId, nonce, gas, to: tx.to, value: tx.value, data: tx.data,
      maxFeePerGas: fee.maxFeePerGas, maxPriorityFeePerGas: fee.maxPriorityFeePerGas,
    })
    const hash = await client.request({ method: 'eth_sendRawTransaction', params: [raw] })
    console.log(`sent ${hash}`)

    const t0 = performance.now()
    const receipt = await client.waitForTransactionReceipt({ hash, pollingInterval: 1_000, timeout: 5 * 60_000 })
    console.log(
      `included in block ${receipt.blockNumber} (index ${receipt.transactionIndex}), status ${receipt.status}, ` +
        `gas used ${receipt.gasUsed}, fee ${formatEther(receipt.gasUsed * receipt.effectiveGasPrice)} ETH, ` +
        `after ${((performance.now() - t0) / 1000).toFixed(1)} s`,
    )
  },
}

if (flags.help || !commands[command]) {
  console.log(readFileSync(new URL(import.meta.url), 'utf8').split('\n').filter((l) => l.startsWith('//')).slice(0, 13).map((l) => l.slice(3)).join('\n'))
  process.exit(command && !flags.help ? 1 : 0)
}
try {
  await commands[command]()
} catch (e) {
  die(e.name === 'HttpRequestError' ? `no answer from ${RPC} (is skim running? SKIM_RPC overrides)` : (e.shortMessage ?? e.message))
}
