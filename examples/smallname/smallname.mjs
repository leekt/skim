// smallname: ENS names, records, lists and history on Sepolia, from a skim node.
//
//   node --experimental-sqlite smallname/smallname.mjs run      # follow skim + serve the API
//   node --experimental-sqlite smallname/smallname.mjs serve    # API only (index as it is)
//
// Env: SKIM_RPC (default http://localhost:8545), DB (default smallname.db), PORT (8787), HOST,
// FROM_BLOCK (where a new index starts; default: the head. Older blocks only if skim serves them).
//
// Current values (owner, expiry, resolver, records, primary name) are always read live from
// skim. The index holds what state can't answer (label preimages, the name tree, history,
// candidates for lists), and only from the block it started at: there's no historical backfill.
import { Store } from "./store.mjs";
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
} else {
  console.error(`unknown command ${cmd} (run | serve)`);
  process.exit(2);
}
