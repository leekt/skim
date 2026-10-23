// Resolve ENS names through skim and a reference node at the same block, and compare.
//
//   SKIM_RPC=http://localhost:8545 REF_RPC=<sepolia rpc> node bigname/ens.mjs [name ...]
//
// Two reads per name, both fixed to one block hash (what bigname's verified lookups do):
//   - viem getEnsAddress (Universal Resolver, ENSv2 with the ENSv1 fallback)
//   - the raw UniversalResolver.resolve(dnsEncode(name), addr(node)) eth_call, so reverts
//     are compared byte for byte too
// plus getEnsName (reverse resolution) for every address the names resolved to.
import { createPublicClient, http, encodeFunctionData, toHex, namehash } from 'viem';
import { sepolia } from 'viem/chains';
import { normalize, packetToBytes } from 'viem/ens';

const SKIM = process.env.SKIM_RPC ?? 'http://localhost:8545';
const REF = process.env.REF_RPC;
if (!REF) throw new Error('set REF_RPC to another Sepolia endpoint');
const UR = '0xeEeEEEeE14D718C2B47D9923Deab1335E144EeEe';
const names = process.argv.slice(2).length
  ? process.argv.slice(2)
  : ['ens.eth', 'nick.eth', 'vitalik.eth', 'test.eth', 'wrappedtest.eth', 'doesnotexist12345.eth'];

const skim = createPublicClient({ chain: sepolia, transport: http(SKIM, { timeout: 60_000 }) });
const ref = createPublicClient({ chain: sepolia, transport: http(REF, { timeout: 60_000 }) });

const urAbi = [{ name: 'resolve', type: 'function', stateMutability: 'view',
  inputs: [{ name: 'name', type: 'bytes' }, { name: 'data', type: 'bytes' }],
  outputs: [{ type: 'bytes' }, { type: 'address' }] }];
const addrAbi = [{ name: 'addr', type: 'function', stateMutability: 'view',
  inputs: [{ name: 'node', type: 'bytes32' }], outputs: [{ type: 'address' }] }];

// A result or a failure, comparable across nodes: the revert data, not the message text.
const settle = (p) => p.then((v) => ({ ok: v ?? null }), (e) => {
  let c = e;
  while (c && !c.data && c.cause) c = c.cause;
  const data = c?.data?.data ?? c?.data;
  return { err: typeof data === 'string' ? `revert ${data.slice(0, 10)}…(${(data.length - 2) / 2} bytes)` : e.shortMessage ?? String(e), raw: data };
});
const same = (a, b) => a.ok !== undefined ? a.ok === b.ok : (a.raw !== undefined && a.raw === b.raw) || (a.raw === undefined && a.err === b.err);
const short = (v) => (typeof v === 'string' && v.length > 66 ? `${v.slice(0, 10)}…${v.slice(-40)} (${(v.length - 2) / 2} bytes)` : String(v));
const show = (r) => (r.ok !== undefined ? short(r.ok) : r.err);

// The skim head's parent: both nodes have it by the time we ask.
const head = await skim.getBlock({ blockTag: 'latest' });
const block = await skim.getBlock({ blockHash: head.parentHash });
console.log(`block ${block.number} ${block.hash}`);
const at = { blockHash: block.hash };

let diffs = 0;
const resolved = new Set();
const row = async (label, run) => {
  const t = performance.now();
  const a = await settle(run(skim));
  const ms = performance.now() - t;
  const b = await settle(run(ref));
  const ok = same(a, b);
  if (typeof a.ok === 'string' && /^0x[0-9a-fA-F]{40}$/.test(a.ok)) resolved.add(a.ok);
  if (!ok) diffs++;
  console.log(`${ok ? 'MATCH' : 'DIFF '} ${label.padEnd(40)} ${show(a)}${ok ? '' : `  ref=${show(b)}`}  (skim ${ms.toFixed(1)} ms)`);
};

// eth_call with a blockHash (EIP-1898), as bigname fixes its lookups to a block.
const call = (c, data) => c.request({ method: 'eth_call', params: [{ to: UR, data }, at] });

for (const name of names) {
  const n = normalize(name);
  await row(`getEnsAddress ${n}`, (c) => c.getEnsAddress({ name: n, blockNumber: block.number }));
  const data = encodeFunctionData({ abi: urAbi, functionName: 'resolve',
    args: [toHex(packetToBytes(n)), encodeFunctionData({ abi: addrAbi, functionName: 'addr', args: [namehash(n)] })] });
  await row(`UR.resolve ${n} @hash`, (c) => call(c, data));
}
for (const a of resolved) {
  await row(`getEnsName ${a.slice(0, 10)}…`, (c) => c.getEnsName({ address: a, blockNumber: block.number }));
}
console.log(diffs ? `${diffs} differences` : 'all match');
process.exit(diffs ? 1 : 0);
