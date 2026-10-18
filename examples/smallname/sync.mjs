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
 * Follows the chain through skim's block stream (`skim_streamNext`). skim does the block work:
 * catch-up over history, the head, reorgs (`rewind`), and data it can no longer serve (`gap`).
 * smallname only applies events. Each response is applied in one SQLite transaction together
 * with the new cursor, so a crash re-delivers a batch (applying is idempotent) and never skips
 * one.
 */
export async function follow(store, url, { fromBlock, log = console.log, signal } = {}) {
  const ingest = new Ingest(store);
  while (!signal?.aborted) {
    try {
      const cursorNumber = store.meta("indexed_block");
      const cursorHash = store.meta("indexed_hash");
      const cursor = cursorNumber && cursorHash ? { number: Number(cursorNumber), hash: cursorHash } : null;
      const recent = store.all("SELECT number, hash FROM blocks ORDER BY number DESC LIMIT 128");
      const first = !cursorNumber;
      const start = cursor ? undefined : Number(store.meta("next_from") || Math.max(fromBlock ?? 0, AMSTERDAM_BLOCK));
      const before = discovered(store).length;
      const r = await rpc(url, "skim_streamNext", [{
        cursor,
        recent,
        // A new index: from the fork (or FROM_BLOCK); skim reports what it can't serve as a gap.
        fromBlock: start,
        filters: queries(store),
        maxBlocks: 2048,
        waitMs: 10_000,
      }], { timeout: 900_000 });
      let applied = 0;
      store.tx(() => {
        for (const e of r.events) {
          if (e.type === "rewind") {
            log(`reorg: rewinding to ${e.to.number}`);
            store.rollback(e.to.number);
            store.meta("indexed_hash", e.to.hash);
          } else if (e.type === "gap") {
            log(`gap ${e.from}..${e.to}: ${e.reason}`);
            if (first) store.meta("started_at_block", e.to + 1); // history starts after it
            else store.meta("gaps", JSON.stringify([...JSON.parse(store.meta("gaps") || "[]"), [e.from, e.to]])); // a hole
          } else if (e.type === "apply") {
            const b = e.block;
            const ts = Number(b.timestamp);
            applied += applyAll(store, ingest, e.logs.map(parseLog), () => ts);
            store.run("INSERT OR REPLACE INTO blocks (number, hash, parent, ts) VALUES (?, ?, ?, ?)", b.number, b.hash, b.parentHash, ts);
          }
        }
        if (first && !store.meta("started_at_block")) store.meta("started_at_block", start);
        // New ENSv2 contracts were announced in this batch: re-read it with them in the filter
        // (keep the old cursor; re-applying is idempotent).
        if (discovered(store).length > before) {
          if (!cursor) store.meta("next_from", start); // first batch: from the start again
          return;
        }
        if (r.cursor) {
          store.run("INSERT OR REPLACE INTO blocks (number, hash, parent, ts) VALUES (?, ?, NULL, NULL)", r.cursor.number, r.cursor.hash);
          store.meta("indexed_block", r.cursor.number);
          store.meta("indexed_hash", r.cursor.hash);
          store.meta("next_from", "");
        } else {
          store.meta("next_from", r.nextFrom);
        }
      });
      if (applied || r.events.length) log(`${r.cursor?.number ?? r.nextFrom - 1}: ${applied} ENS events (head ${r.head}${r.finalized ? `, finalized ${r.finalized.number}` : ""})`);
      if (r.finalized) store.meta("finalized_block", r.finalized.number);
      const fin = store.meta("finalized_block");
      store.pruneBlocks(Number(store.meta("indexed_block") ?? 0), fin ? Math.min(Number(fin), Number(store.meta("indexed_block") ?? 0)) : null);
    } catch (e) {
      log(`stream: ${e.message}`);
      await sleep(5000);
    }
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
