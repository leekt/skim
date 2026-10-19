// smallname's own tests (no network): node --experimental-sqlite smallname/test.mjs
import assert from "node:assert/strict";
import { mkdtempSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { encodeEventTopics, encodeAbiParameters, keccak256, toHex, namehash, labelhash, toEventSelector } from "viem";
import { Store } from "./store.mjs";
import { Ingest } from "./ingest.mjs";
import { applyResponse } from "./sync.mjs";
import { EVENTS, V1 } from "./contracts.mjs";

const tests = [];
const test = (name, fn) => tests.push([name, fn]);

const hash = (s) => keccak256(toHex(s));
const block = (n, fork) => ({ number: n, hash: hash(`${fork}${n}`), parentHash: hash(`${fork}${n - 1}`), timestamp: 1_800_000_000 + n * 12 });

/** A registry NewOwner log (creates `label.eth`, owned by `owner`) in block `b`. */
function newOwner(b, label, owner, logIndex = 0) {
  return {
    address: V1.registry,
    topics: encodeEventTopics({ abi: [EVENTS.NewOwner], eventName: "NewOwner", args: { node: namehash("eth"), label: labelhash(label) } }),
    data: encodeAbiParameters([{ type: "address" }], [owner]),
    blockNumber: toHex(b.number),
    blockHash: b.hash,
    blockTimestamp: toHex(b.timestamp),
    logIndex: toHex(logIndex),
    transactionIndex: "0x0",
    transactionHash: hash(`tx${b.hash}${logIndex}`),
    removed: false,
  };
}

const apply = (b, logs) => ({ type: "apply", block: b, logs });
const fresh = () => {
  const store = new Store(join(mkdtempSync(join(tmpdir(), "smallname-")), "t.db"));
  return { store, ingest: new Ingest(store) };
};
const cursorOf = (store) => ({ number: Number(store.meta("indexed_block")), hash: store.meta("indexed_hash") });

test("a rewind and the new fork's events commit with the cursor, in one transaction", () => {
  const { store, ingest } = fresh();
  const [a99, a100, b100] = [block(99, "A"), block(100, "A"), { ...block(100, "B"), parentHash: block(99, "A").hash }];
  const alice = "0x00000000000000000000000000000000000a11ce";
  const bob = "0x0000000000000000000000000000000000000b0b";
  applyResponse(store, ingest, { events: [apply(a99, []), apply(a100, [newOwner(a100, "forka", alice)])], cursor: { number: 100, hash: a100.hash }, nextFrom: 101 }, { cursor: null, start: 99 });
  assert.deepEqual(cursorOf(store), { number: 100, hash: a100.hash });
  assert.equal(store.get("SELECT count(*) c FROM events WHERE block = 100").c, 1);
  // Reorg: A100 -> B100. skim answers: rewind to 99, then B100's events, cursor B100.
  applyResponse(
    store,
    ingest,
    { events: [{ type: "rewind", to: { number: 99, hash: a99.hash } }, apply(b100, [newOwner(b100, "forkb", bob)])], cursor: { number: 100, hash: b100.hash }, nextFrom: 101 },
    { cursor: cursorOf(store) },
  );
  assert.deepEqual(cursorOf(store), { number: 100, hash: b100.hash });
  const rows = store.all("SELECT block_hash, actor FROM events WHERE block = 100");
  assert.deepEqual(rows.map((r) => r.block_hash), [b100.hash], "only B100's event remains");
  assert.equal(rows[0].actor, bob);
  assert.equal(store.get("SELECT count(*) c FROM names WHERE labelhash = ?", labelhash("forka")).c, 0, "A100's name tree edge is gone");
  assert.equal(store.get("SELECT count(*) c FROM names WHERE labelhash = ?", labelhash("forkb")).c, 1);
});

test("a failing response leaves nothing half-applied", () => {
  const { store, ingest } = fresh();
  const a100 = block(100, "A");
  applyResponse(store, ingest, { events: [apply(a100, [newOwner(a100, "x", "0x00000000000000000000000000000000000a11ce")])], cursor: { number: 100, hash: a100.hash }, nextFrom: 101 }, { cursor: null, start: 100 });
  const before = cursorOf(store);
  const broken = { events: [{ type: "rewind", to: { number: 99, hash: block(99, "A").hash } }, { type: "apply", block: block(100, "B"), logs: null }], cursor: { number: 100, hash: block(100, "B").hash }, nextFrom: 101 };
  assert.throws(() => applyResponse(store, ingest, broken, { cursor: before }));
  assert.deepEqual(cursorOf(store), before, "cursor unchanged");
  assert.equal(store.get("SELECT count(*) c FROM events WHERE block = 100").c, 1, "the rewind was rolled back with the rest");
});

/** A new ENSv2 resolver announcing itself (ResolverCreated) in block `b`. */
function resolverCreated(b, address, logIndex = 0) {
  return {
    address,
    topics: [toEventSelector(EVENTS.ResolverCreated)],
    data: "0x",
    blockNumber: toHex(b.number),
    blockHash: b.hash,
    blockTimestamp: toHex(b.timestamp),
    logIndex: toHex(logIndex),
    transactionIndex: "0x0",
    transactionHash: hash(`tx${b.hash}${logIndex}`),
    removed: false,
  };
}

test("a pass that finds new contracts stores nothing, so a fork can't leave its events behind", () => {
  // Review P1: A101 announces a resolver; the batch must be re-read with it in the filter. Then
  // A101 is replaced by B101 (no events) before the re-read. Nothing of A101 may remain.
  const { store, ingest } = fresh();
  const [a100, a101] = [block(100, "A"), block(101, "A")];
  const b101 = { ...block(101, "B"), parentHash: a100.hash };
  const resolver = "0x00000000000000000000000000000000000ce501";
  applyResponse(store, ingest, { events: [apply(a100, [])], cursor: { number: 100, hash: a100.hash }, nextFrom: 101 }, { cursor: null, start: 100 });
  const first = applyResponse(store, ingest, { events: [apply(a101, [resolverCreated(a101, resolver)])], cursor: { number: 101, hash: a101.hash }, nextFrom: 102 }, { cursor: cursorOf(store) });
  assert.deepEqual(first.rescan, [resolver], "asks for a re-read with the new contract");
  assert.equal(store.get("SELECT count(*) c FROM v2_resolvers").c, 0, "the provisional pass stored nothing");
  assert.equal(store.get("SELECT count(*) c FROM events WHERE block = 101").c, 0);
  assert.deepEqual(cursorOf(store), { number: 100, hash: a100.hash });
  // The re-read sees B101 (A100 is still canonical: no rewind).
  const second = applyResponse(store, ingest, { events: [], cursor: { number: 101, hash: b101.hash }, nextFrom: 102 }, { cursor: cursorOf(store), extra: first.rescan });
  assert.equal(second.rescan, undefined);
  assert.deepEqual(cursorOf(store), { number: 101, hash: b101.hash });
  assert.equal(store.get("SELECT count(*) c FROM v2_resolvers").c, 0, "no orphan resolver");
  assert.equal(store.get("SELECT count(*) c FROM events WHERE block = 101").c, 0, "no orphan event");
  // Without the fork, the re-read (with the contract in the filter) commits it with its cursor.
  const s2 = fresh();
  applyResponse(s2.store, s2.ingest, { events: [apply(a100, [])], cursor: { number: 100, hash: a100.hash }, nextFrom: 101 }, { cursor: null, start: 100 });
  const again = { events: [apply(a101, [resolverCreated(a101, resolver)])], cursor: { number: 101, hash: a101.hash }, nextFrom: 102 };
  const r1 = applyResponse(s2.store, s2.ingest, again, { cursor: cursorOf(s2.store) });
  const r2 = applyResponse(s2.store, s2.ingest, again, { cursor: cursorOf(s2.store), extra: r1.rescan });
  assert.equal(r2.rescan, undefined);
  assert.equal(s2.store.get("SELECT count(*) c FROM v2_resolvers").c, 1);
  assert.deepEqual(cursorOf(s2.store), { number: 101, hash: a101.hash });
});

test("a reorg restores a record key's earlier write instead of forgetting the key", () => {
  // Review P2: text:email written at 90 and again at 101; a rewind to 100 must leave it known
  // from 90, exactly as an index built only from the surviving chain would.
  const inventory = (store) => store.all("SELECT key, max(block * 1000000 + log_index) pos FROM record_key_writes GROUP BY node, resolver, key ORDER BY key");
  const node = namehash("alice.eth");
  const resolver = "0x00000000000000000000000000000000000ce501";
  const { store, ingest } = fresh();
  store.tx(() => {
    ingest.recordKey(node, resolver, "text:email", { block: 90, logIndex: 2 });
    ingest.recordKey(node, resolver, "text:email", { block: 101, logIndex: 0 });
    ingest.recordKey(node, resolver, "text:url", { block: 101, logIndex: 1 });
    store.rollbackInTx(100);
  });
  const survivor = fresh();
  survivor.store.tx(() => survivor.ingest.recordKey(node, resolver, "text:email", { block: 90, logIndex: 2 }));
  assert.deepEqual(inventory(store), inventory(survivor.store));
  assert.deepEqual(inventory(store).map((r) => r.key), ["text:email"]);
});

test("index files with the old one-row-per-key inventory are migrated on open", () => {
  const path = join(mkdtempSync(join(tmpdir(), "smallname-")), "old.db");
  const db = new DatabaseSync(path);
  db.exec("CREATE TABLE record_keys (node TEXT NOT NULL, resolver TEXT NOT NULL, key TEXT NOT NULL, block INTEGER NOT NULL, log_index INTEGER NOT NULL, PRIMARY KEY (node, resolver, key))");
  db.exec("INSERT INTO record_keys VALUES ('0x01', '0x02', 'text:email', 90, 2)");
  db.close();
  const store = new Store(path);
  assert.deepEqual(store.all("SELECT key, block FROM record_key_writes").map((r) => [r.key, r.block]), [["text:email", 90]]);
  assert.equal(store.get("SELECT count(*) c FROM sqlite_master WHERE name = 'record_keys'").c, 0);
});

let failed = 0;
for (const [name, fn] of tests) {
  try {
    await fn();
    console.log(`ok   ${name}`);
  } catch (e) {
    failed++;
    console.log(`FAIL ${name}\n     ${e.message}`);
  }
}
console.log(`${tests.length - failed}/${tests.length} passed`);
process.exit(failed ? 1 : 0);
