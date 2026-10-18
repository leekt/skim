// smallname's own tests (no network): node --experimental-sqlite smallname/test.mjs
import assert from "node:assert/strict";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { encodeEventTopics, encodeAbiParameters, keccak256, toHex, namehash, labelhash } from "viem";
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
