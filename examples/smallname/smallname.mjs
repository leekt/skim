// smallname: ENS names, records, lists and history on Sepolia, from a skim node.
//
//   node --experimental-sqlite smallname/smallname.mjs run      # follow skim + serve the API
//   node --experimental-sqlite smallname/smallname.mjs serve    # API only (index as it is)
//   node --experimental-sqlite smallname/smallname.mjs import-labels <file>
//        # optional: a newline-separated label list (e.g. a published ENS label list) to name
//        # `[labelhash]` rows and pre-index `.eth` parents; each label is used only where its hash occurs
//
// Env: SKIM_RPC (default http://localhost:8545), DB (default smallname.db), PORT (8787), HOST,
// FROM_BLOCK (where a new index starts; default: the head. Older blocks only if skim serves them).
//
// Current values (owner, expiry, resolver, records, primary name) are always read live from
// skim. The index holds what state can't answer (label preimages, the name tree, history,
// candidates for lists), and only from the block it started at: there's no historical backfill.
import { createReadStream } from "node:fs";
import { createInterface } from "node:readline";
import { Store } from "./store.mjs";
import { Ingest } from "./ingest.mjs";
import { follow } from "./sync.mjs";
import { serve } from "./api.mjs";

const SKIM = process.env.SKIM_RPC ?? "http://localhost:8545";
const DB = process.env.DB ?? "smallname.db";
const PORT = Number(process.env.PORT ?? 8787);
const HOST = process.env.HOST; // e.g. 127.0.0.1 behind a proxy; default: all interfaces
const FROM = process.env.FROM_BLOCK ? Number(process.env.FROM_BLOCK) : undefined;
const cmd = process.argv[2] ?? "run";

const store = new Store(DB);
if (cmd === "run") {
  serve(store, { skim: SKIM, port: PORT, host: HOST });
  await follow(store, SKIM, { fromBlock: FROM });
} else if (cmd === "serve") {
  serve(store, { skim: SKIM, port: PORT, host: HOST });
} else if (cmd === "import-labels") {
  const file = process.argv[3];
  if (!file) {
    console.error("usage: import-labels <file> (one label per line)");
    process.exit(2);
  }
  const ingest = new Ingest(store);
  let seen = 0;
  let named = 0;
  let batch = [];
  const flush = () => {
    store.tx(() => {
      for (const l of batch) named += ingest.importLabel(l);
    });
    batch = [];
  };
  for await (const line of createInterface({ input: createReadStream(file) })) {
    const l = line.trim();
    if (!l) continue;
    seen++;
    batch.push(l);
    if (batch.length === 10_000) flush();
  }
  flush();
  console.log(`${seen} labels read, ${named} index rows named`);
} else {
  console.error(`unknown command ${cmd} (run | serve | import-labels)`);
  process.exit(2);
}
