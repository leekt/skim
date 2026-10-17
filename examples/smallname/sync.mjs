// Getting logs into the index, all through skim: history since Glamsterdam (skim serves logs from
// the fork on) in ranges, then the head block by block, with reorgs rolled back. History before
// the fork isn't indexed; responses say so.
import { toEventSelector } from "viem";
import { ADDRESSED, AMSTERDAM_BLOCK, EVENTS, V2, V2_DISCOVERY_EVENTS } from "./contracts.mjs";
import { Ingest } from "./ingest.mjs";

let rpcId = 0;
async function rpcOnce(url, method, params, timeout) {
  const res = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: ++rpcId, method, params }),
    signal: AbortSignal.timeout(timeout),
  });
  const text = await res.text();
  let j;
  try {
    j = JSON.parse(text);
  } catch {
    throw Object.assign(new Error(`${method}: HTTP ${res.status}`), { transient: true });
  }
  if (j.error) {
    const tooBig = /more than \d+ results|range|exceed/i.test(j.error.message);
    const limited = res.status === 429 || /rate|too many requests|capacity|throughput|temporarily unavailable|timeout|internal error/i.test(j.error.message);
    throw Object.assign(new Error(`${method}: ${j.error.message}`), { rpc: j.error, transient: limited && !tooBig });
  }
  if (!("result" in j)) throw Object.assign(new Error(`${method}: HTTP ${res.status}, no result`), { transient: true });
  return j.result;
}

/** One JSON-RPC call; rate limits and dropped connections are retried with backoff. */
export async function rpc(url, method, params, { timeout = 60_000, retries = 12 } = {}) {
  for (let i = 0; ; i++) {
    try {
      return await rpcOnce(url, method, params, timeout);
    } catch (e) {
      const transient = e.transient || e.name === "TimeoutError" || e.name === "TypeError";
      if (!transient || i >= retries) throw e;
      await sleep(Math.min(60_000, 500 * 2 ** i));
    }
  }
}

const hex = (n) => "0x" + n.toString(16);
const num = (h) => Number.parseInt(h, 16);
const DISCOVERY_TOPICS = V2_DISCOVERY_EVENTS.map((n) => toEventSelector(EVENTS[n]));
const SEEDED = new Set([V2.rootRegistry, V2.ethRegistry].map((a) => a.toLowerCase()));

/** Registries and record-id resolvers found so far, other than the ones queried by address. */
function discovered(store) {
  return store
    .all("SELECT address FROM registries UNION SELECT address FROM v2_resolvers")
    .map((r) => r.address)
    .filter((a) => !SEEDED.has(a) && !ADDRESSED.includes(a));
}

/** The log queries for a block or range: known contracts by address, ENSv2 topics from anyone. */
function queries(store) {
  return [{ address: [...ADDRESSED, ...discovered(store)] }, { topics: [DISCOVERY_TOPICS] }];
}

function parseLog(l) {
  return {
    ...l,
    address: l.address.toLowerCase(),
    blockNumber: num(l.blockNumber),
    logIndex: num(l.logIndex),
    transactionIndex: l.transactionIndex == null ? null : num(l.transactionIndex),
    ts: l.blockTimestamp ? num(l.blockTimestamp) : null,
  };
}

/** eth_getLogs over [from, to], split while skim says the range is too large. */
async function logsRange(url, filter, from, to) {
  try {
    return (await rpc(url, "eth_getLogs", [{ ...filter, fromBlock: hex(from), toBlock: hex(to) }], { timeout: 600_000 })).map(parseLog);
  } catch (e) {
    if (from < to && (e.rpc?.code === -32005 || /range|exceed|more than/i.test(String(e.message)))) {
      const mid = Math.floor((from + to) / 2);
      return [...(await logsRange(url, filter, from, mid)), ...(await logsRange(url, filter, mid + 1, to))];
    }
    throw e;
  }
}

/** History in ranges, up to `to` (a block well behind the head, so it won't reorg). */
async function catchUp(store, ingest, url, to, log, step = 2048) {
  let from = Number(store.meta("indexed_block")) + 1;
  const t0 = Date.now();
  while (from <= to) {
    const b = Math.min(from + step - 1, to);
    // Two queries: every known contract by address (declared and discovered), and the ENSv2
    // announcement topics from any emitter. ENSv2 contracts announced in this range have their
    // other events fetched with the next range's address list, so the range is re-read once.
    const before = discovered(store).length;
    const logs = (await Promise.all(queries(store).map((f) => logsRange(url, f, from, b)))).flat();
    store.tx(() => applyAll(store, ingest, logs));
    if (discovered(store).length > before) {
      const more = await logsRange(url, { address: discovered(store) }, from, b);
      store.tx(() => applyAll(store, ingest, more));
    }
    // The range end's hash, where skim still has the block (older ranges don't need it: reorgs
    // are checked from the followed window on).
    const h = await rpc(url, "eth_getBlockByNumber", [hex(b), false]).catch(() => null);
    store.tx(() => {
      if (h) store.run("INSERT OR REPLACE INTO blocks (number, hash, parent, ts) VALUES (?, ?, ?, ?)", b, h.hash, h.parentHash, num(h.timestamp));
      store.meta("indexed_block", b);
      store.meta("indexed_hash", h?.hash ?? "");
    });
    log(`history ${b} (${(((b - AMSTERDAM_BLOCK) / (to - AMSTERDAM_BLOCK)) * 100).toFixed(1)}%)  ${store.get("SELECT count(*) c FROM events").c} events  ${((Date.now() - t0) / 1000).toFixed(0)} s`);
    from = b + 1;
  }
}

const byPosition = (a, b) => a.blockNumber - b.blockNumber || a.logIndex - b.logIndex;

function applyAll(store, ingest, logs, tsOf) {
  let n = 0;
  const seen = new Set(); // a log can match both queries
  for (const l of logs.sort(byPosition)) {
    const k = `${l.blockNumber}:${l.logIndex}`;
    if (seen.has(k)) continue;
    seen.add(k);
    if (ingest.apply(l, l.ts ?? tsOf?.(l.blockNumber) ?? null)) n++;
  }
  return n;
}

/**
 * Follows the head through `url` (skim). Each block is fetched by hash, so logs and header always
 * belong together; a parent mismatch walks back to the common ancestor and rolls the index back.
 */
export async function follow(store, url, { fromBlock, log = console.log, onBlock, signal } = {}) {
  const ingest = new Ingest(store);
  while (!signal?.aborted) {
    try {
      const head = await rpc(url, "eth_getBlockByNumber", ["latest", false]);
      const target = num(head.number);
      let last = Number(store.meta("indexed_block") ?? 0);
      if (!last) {
        // A new index starts as far back as skim can still serve logs: the fork at the earliest,
        // and no earlier than what connected peers keep (history expiry). FROM_BLOCK can start
        // it later. From then on the index keeps what it indexed.
        const served = (await rpc(url, "skim_status", []).catch(() => null))?.history?.logs_from ?? AMSTERDAM_BLOCK;
        last = Math.max(fromBlock ?? 0, served, AMSTERDAM_BLOCK) - 1;
        store.meta("indexed_block", last);
        store.meta("started_at_block", last + 1);
        log(`new index: from block ${last + 1}`);
      }
      // Far behind: history in ranges first (up to 64 blocks behind the head).
      if (target - last > 128) {
        await catchUp(store, ingest, url, target - 64, log);
        continue;
      }
      if (target <= last && store.meta("indexed_hash") === head.hash) {
        await sleep(2000);
        continue;
      }
      for (let n = last + 1; n <= Math.min(target, last + 256); n++) {
        const src = url;
        const h = await rpc(src, "eth_getBlockByNumber", [hex(n), false]);
        if (!h) break;
        const prev = store.get("SELECT hash FROM blocks WHERE number = ?", n - 1)?.hash;
        if (prev && prev !== h.parentHash) {
          const at = await ancestor(store, url, n - 1);
          log(`reorg: rolling back to ${at}`);
          store.rollback(at);
          break;
        }
        const logs = (await Promise.all(queries(store).map((f) => rpc(src, "eth_getLogs", [{ ...f, blockHash: h.hash }])))).flat().map(parseLog);
        const ts = num(h.timestamp);
        const applied = store.tx(() => {
          const k = applyAll(store, ingest, logs, () => ts);
          store.run("INSERT OR REPLACE INTO blocks (number, hash, parent, ts) VALUES (?, ?, ?, ?)", n, h.hash, h.parentHash, ts);
          store.meta("indexed_block", n);
          store.meta("indexed_hash", h.hash);
          return k;
        });
        if (applied) log(`block ${n}: ${applied} ENS events`);
        onBlock?.(n, h.hash);
      }
      store.pruneBlocks(Number(store.meta("indexed_block")));
    } catch (e) {
      log(`follow: ${e.message}`);
      await sleep(5000);
    }
  }
}

/** The newest indexed block that is still on `url`'s chain. */
async function ancestor(store, url, from) {
  for (let n = from; n > 0; n--) {
    const ours = store.get("SELECT hash FROM blocks WHERE number = ?", n)?.hash;
    if (!ours) return n; // older than the followed window: nothing indexed there can be checked
    const theirs = await rpc(url, "eth_getBlockByNumber", [hex(n), false]);
    if (theirs?.hash === ours) return n;
  }
  return 0;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
