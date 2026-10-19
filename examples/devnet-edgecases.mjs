// State edge cases on a local devnet: skim (applying BALs) against an executing node, key by key,
// at the same block hash. The cases are those Nethermind's BAL catch-up tests (PR 14157):
// account creation, a touched missing account, create + selfdestruct in one transaction, slot
// clearing, EIP-7702 delegation set and cleared, and a reverted transaction.
//
//   node devnet-edgecases.mjs plan <el rpc> <out dir>     # addresses, and skim.yaml subscribing to them
//   CONFIG=<out dir>/skim.yaml devnet/run.sh <skim>       # skim tracks those keys before anything happens
//   node devnet-edgecases.mjs run <el rpc> <skim rpc> <out dir>
//
// skim must track every key before the transactions land, so its values come from BALs (not from
// a fetch after the fact). Keys are compared after every step, at that step's block.
import { readFileSync, writeFileSync } from "node:fs";
import { createPublicClient, createWalletClient, http, getContractAddress, encodeAbiParameters, concat, toHex, zeroAddress, padHex } from "viem";
import { mnemonicToAccount, privateKeyToAccount, generatePrivateKey } from "viem/accounts";

// ethereum-package's default (public, devnet-only) mnemonic; its first accounts are prefunded.
const MNEMONIC = "giant issue aisle success illegal bike spike question tent bar rely arctic volcano long crawl hungry vocal artwork sniff fantasy very lucky have athlete";
const [A, B, D] = [0, 1, 2].map((i) => mnemonicToAccount(MNEMONIC, { addressIndex: 10 + i }));

/** Init code returning `runtime` (hex, no 0x). */
const deployer = (runtime) => {
  const n = runtime.length / 2;
  return `0x60${n.toString(16).padStart(2, "0")}600c60003960${n.toString(16).padStart(2, "0")}6000f3${runtime}`;
};
// Storage setter: SSTORE(calldata[0..32], calldata[32..64]).
const SETTER = "602035600035550000";
// Writes slot 0 = 1, then reverts.
const REVERTER = "600160005560006000fd";

const [cmd, elUrl, ...rest] = process.argv.slice(2);
const pub = createPublicClient({ transport: http(elUrl) });

async function plan(outDir) {
  const nonce = await pub.getTransactionCount({ address: A.address });
  for (const acct of [A, B, D]) {
    const bal = await pub.getBalance({ address: acct.address });
    if (bal === 0n) throw new Error(`${acct.address} isn't funded: not the devnet's mnemonic?`);
  }
  const p = {
    A: A.address, B: B.address, D: D.address,
    setter: getContractAddress({ from: A.address, nonce: BigInt(nonce) }),
    doomed: getContractAddress({ from: A.address, nonce: BigInt(nonce + 1) }),
    reverter: getContractAddress({ from: A.address, nonce: BigInt(nonce + 2) }),
    fresh: privateKeyToAccount(generatePrivateKey()).address,
    missing: privateKeyToAccount(generatePrivateKey()).address,
    beneficiary: privateKeyToAccount(generatePrivateKey()).address,
    firstNonce: nonce,
  };
  const slots = ["0x0", "0x1", "0x2", "0x3"];
  const subs = Object.entries(p)
    .filter(([k]) => k !== "firstNonce")
    .map(([k, a]) => `    - name: ${k}\n      address: "${a}"\n      account: true\n      storage: [${slots.map((s) => `"${s}"`).join(", ")}]`);
  writeFileSync(`${outDir}/plan.json`, JSON.stringify(p, null, 2));
  writeFileSync(`${outDir}/skim.yaml`, `state:\n  subscriptions:\n${subs.join("\n")}\n`);
  console.log(JSON.stringify(p, null, 2));
}

async function rpc(url, method, params) {
  const r = await (await fetch(url, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }) })).json();
  if (r.error) throw new Error(`${method}: ${r.error.message}`);
  return r.result;
}

/** Every planned key at `blockHash`, from both nodes. Returns mismatches. */
async function compare(skimUrl, p, blockHash, step) {
  const at = { blockHash };
  const bad = [];
  for (const [name, a] of Object.entries(p)) {
    if (name === "firstNonce") continue;
    const q = [["eth_getBalance", [a, at]], ["eth_getTransactionCount", [a, at]], ["eth_getCode", [a, at]],
      ...["0x0", "0x1", "0x2", "0x3"].map((s) => ["eth_getStorageAt", [a, s, at]])];
    for (const [m, params] of q) {
      const [x, y] = await Promise.all([rpc(elUrl, m, params), rpc(skimUrl, m, params).catch((e) => `ERR ${e.message}`)]);
      const norm = (v) => (typeof v === "string" && m === "eth_getStorageAt" ? padHex(v, { size: 32 }) : typeof v === "string" && v.startsWith("0x") && m !== "eth_getCode" ? toHex(BigInt(v)) : v);
      if (norm(x) !== norm(y)) bad.push(`${step}: ${name} ${m} ${params.slice(1, -1).join(" ")} node=${x} skim=${y}`);
    }
  }
  return bad;
}

async function run(skimUrl, outDir) {
  const p = JSON.parse(readFileSync(`${outDir}/plan.json`, "utf8"));
  const chainId = await pub.getChainId();
  const chain = { id: chainId, name: "devnet", nativeCurrency: { name: "ETH", symbol: "ETH", decimals: 18 }, rpcUrls: { default: { http: [elUrl] } } };
  const w = (account) => createWalletClient({ account, chain, transport: http(elUrl) });
  const set = (slot, value) => encodeAbiParameters([{ type: "uint256" }, { type: "uint256" }], [slot, value]);
  if ((await pub.getTransactionCount({ address: A.address })) !== p.firstNonce) throw new Error("A's nonce moved since the plan: re-plan");

  const steps = [
    ["deploy the storage setter", () => w(A).sendTransaction({ data: deployer(SETTER) })],
    ["create + selfdestruct in one transaction (value to a beneficiary)", () => w(A).sendTransaction({ data: concat(["0x73", p.beneficiary, "0xff"]), value: 1000n })],
    ["deploy a contract that writes then reverts", () => w(A).sendTransaction({ data: deployer(REVERTER) })],
    ["set slots 1 and 2", async () => { await w(A).sendTransaction({ to: p.setter, data: set(1n, 5n) }); return w(A).sendTransaction({ to: p.setter, data: set(2n, 7n) }); }],
    ["create an account by sending to it", () => w(A).sendTransaction({ to: p.fresh, value: 10n ** 15n })],
    ["touch a missing account (zero-value call)", () => w(B).sendTransaction({ to: p.missing, value: 0n })],
    ["clear slot 1", () => w(A).sendTransaction({ to: p.setter, data: set(1n, 0n) })],
    ["a reverted call with value", () => w(A).sendTransaction({ to: p.reverter, value: 123n, gas: 100000n })],
    ["7702: delegate D to the setter", async () => {
      const auth = await w(D).signAuthorization({ contractAddress: p.setter, executor: "self" });
      return w(D).sendTransaction({ to: D.address, authorizationList: [auth] });
    }],
    ["write D's storage through its delegation", () => w(A).sendTransaction({ to: p.D, data: set(3n, 9n) })],
    ["7702: clear D's delegation", async () => {
      const auth = await w(D).signAuthorization({ contractAddress: zeroAddress, executor: "self" });
      return w(D).sendTransaction({ to: D.address, authorizationList: [auth] });
    }],
  ];
  let failures = [];
  for (const [step, send] of steps) {
    const hash = await send();
    const r = await pub.waitForTransactionReceipt({ hash });
    // Wait until skim has applied the block (its latest reaches it).
    for (let i = 0; i < 60; i++) {
      const head = BigInt(await rpc(skimUrl, "eth_blockNumber", []));
      if (head >= r.blockNumber) break;
      await new Promise((ok) => setTimeout(ok, 1000));
    }
    const bad = await compare(skimUrl, p, r.blockHash, step);
    console.log(`${bad.length ? "MISMATCH" : "match   "} block ${r.blockNumber} ${r.status.padEnd(8)} ${step}`);
    for (const b of bad) console.log(`  ${b}`);
    failures = failures.concat(bad);
  }
  console.log(failures.length ? `${failures.length} mismatches` : "all match");
  process.exit(failures.length ? 1 : 0);
}

if (cmd === "plan") await plan(rest[0]);
else if (cmd === "run") await run(rest[0], rest[1]);
else throw new Error("usage: plan <el rpc> <out dir> | run <el rpc> <skim rpc> <out dir>");
