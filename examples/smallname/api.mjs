// bigname's /v1 HTTP API, the routes an ENS frontend uses. Lists come from the index; every
// value in a response (owner, expiry, status, resolver, records, primary name) is read live from
// skim at one block hash, reported in `meta.as_of`. Index candidates that no longer hold are
// dropped, so the index can be behind or over-inclusive without a response being wrong.
import http from "node:http";
import { readFileSync } from "node:fs";
import { namehash, labelhash, isAddress, getAddress } from "viem";
import { CHAIN_ID, AMSTERDAM_BLOCK, V1, V1_RESOLVERS, V2, VIEWS } from "./contracts.mjs";
import { Ingest, dnsLabels } from "./ingest.mjs";
import { Reader, nameState, records, primaryName, normalizeName, resolverRecords, registryEntry } from "./live.mjs";

const PAGE_DEFAULT = 50;
const PAGE_MAX = 200;
const ROOT = "0x" + "00".repeat(32);
const HISTORY_TYPES = ["registration", "renewal", "release", "expiry", "transfer", "authority", "resolver", "record", "primary_name", "permission", "subregistry", "migration", "reservation", "token", "contract"];

class ApiError extends Error {
  constructor(status, code, message, details = {}) {
    super(message);
    Object.assign(this, { status, code, details });
  }
}
const bad = (m, d) => new ApiError(400, "invalid_input", m, d);
const unsupported = (m, reason) => new ApiError(422, "unsupported", m, { unsupported_reason: reason });

const enc = (o) => Buffer.from(JSON.stringify(o)).toString("base64url");
function dec(c, scope) {
  try {
    const o = JSON.parse(Buffer.from(c, "base64url").toString());
    if (o.s !== scope) throw 0;
    return o;
  } catch {
    throw bad("malformed cursor, or one from a different query");
  }
}

export function serve(store, { skim, port, host }) {
  const html = (() => {
    try {
      return readFileSync(new URL("./page.html", import.meta.url));
    } catch {
      return Buffer.from("smallname");
    }
  })();

  // ---- index helpers ----
  const labelOf = (lh) => store.get("SELECT label FROM labels WHERE labelhash = ?", lh)?.label ?? null;
  /** The display name of an indexed node (`[labelhash]` for unknown labels), or null. */
  function nameOf(node) {
    const parts = [];
    for (let n = node, i = 0; n !== ROOT && i < 64; i++) {
      const row = store.get("SELECT parent, labelhash FROM names WHERE node = ?", n);
      if (!row) return null;
      parts.push(labelOf(row.labelhash) ?? `[${row.labelhash.slice(2)}]`);
      n = row.parent;
    }
    return parts.join(".");
  }
  /**
   * nameOf, plus one fallback for names whose ancestors predate the index: the NameWrapper keeps
   * every wrapped name in DNS form. The first unnamed ancestor is looked up there (live), checked
   * against its node, and recorded (labels and edges), so the full name can be built.
   */
  const ingest = new Ingest(store);
  async function nameOfLive(rd, node) {
    const n = nameOf(node);
    if (n) return n;
    let missing = node;
    for (let i = 0; i < 64; i++) {
      const row = store.get("SELECT parent FROM names WHERE node = ?", missing);
      if (!row) break;
      missing = row.parent;
    }
    const r = await rd.one({ to: V1.nameWrapper, abi: VIEWS.nameWrapper, functionName: "names", args: [missing] });
    if (!r.ok || r.value === "0x") return null;
    try {
      const name = dnsLabels(r.value).join(".");
      store.tx(() => ingest.fullName(name, missing, 0, null));
    } catch {
      return null;
    }
    return nameOf(node);
  }
  const firstSeen = (node, types) =>
    // A migration's ENSv2 registration isn't a new registration (it keeps the ENSv1 lease's date).
    store.get(
      `SELECT ts FROM events INDEXED BY events_node WHERE node = ? AND type IN (${types.map(() => "?").join(",")})
       AND tx NOT IN (SELECT tx FROM migrations WHERE node = ? AND tx IS NOT NULL) ORDER BY block DESC, log_index DESC LIMIT 1`,
      node, ...types, node)?.ts ?? null;
  const releasedInV2 = (node) => {
    const r = store.get("SELECT kind FROM events INDEXED BY events_node WHERE node = ? AND kind IN ('LabelRegistered','LabelUnregistered') ORDER BY block DESC, log_index DESC LIMIT 1", node);
    return r?.kind === "LabelUnregistered";
  };

  /** Record keys ever written to `node` on `resolver`, after its latest version reset. */
  function inventory(node, resolver) {
    if (!resolver) return [];
    const r = resolver.toLowerCase();
    const v = store.get("SELECT block, log_index FROM versions WHERE node = ? AND resolver = ? ORDER BY block DESC, log_index DESC LIMIT 1", node, r);
    return store
      .all("SELECT key, block, log_index FROM record_keys WHERE node = ? AND resolver = ?", node, r)
      .filter((k) => !v || k.block > v.block || (k.block === v.block && k.log_index > v.log_index))
      .map((k) => k.key)
      .sort();
  }

  // Two positions in every response that reads both: `as_of` is the block the live values were
  // read at; `index` is how far the event index (lists, history) has processed. Lists can miss
  // names whose events the index hasn't reached yet (a transfer to the address after
  // `indexed_block`), and anything before `history_from_block`.
  const indexMeta = (head) => {
    const indexed = Number(store.meta("indexed_block") ?? 0);
    const gaps = JSON.parse(store.meta("gaps") || "[]");
    const fin = store.meta("finalized_block");
    return {
      indexed_block: indexed, history_from_block: historyFrom(), lag_blocks: head ? Math.max(0, head - indexed) : null,
      // Verified by skim's light client: index rows at or below it can't be rewound, so exports
      // that can't be undone should wait for it.
      finalized_block: fin ? Number(fin) : null,
      ...(gaps.length && { gaps }),
    };
  };
  const asOf = (rd) => ({ as_of: { [CHAIN_ID]: rd.head }, index: indexMeta(rd.head.block_number) });
  // History and lists come from events since Glamsterdam only (skim serves logs from the fork).
  // History starts where this index started (the fork at the earliest; later if peers had
  // already expired older receipts, or the index was started later).
  const historyFrom = () => Number(store.meta("started_at_block") ?? AMSTERDAM_BLOCK);
  const partialMeta = () => ({
    completeness: "partial",
    unsupported_reason: historyFrom() <= AMSTERDAM_BLOCK ? "pre_glamsterdam_history" : "history_before_index_start",
    history_from_block: historyFrom(),
  });
  const ref = (a) => (a ? { chain_id: CHAIN_ID, address: a.toLowerCase() } : null);

  /** A name row: identity plus live state (null if the name isn't registered now). */
  async function nameRow(rd, name, node = namehash(name)) {
    const st = await nameState(rd, name, { releasedInV2: releasedInV2(node) });
    if (st.unavailable) return { name, display_name: name, namespace: "ens", namehash: node, read_status: "failed", failure_reason: "state_unavailable" };
    const row = {
      name, display_name: name, namespace: "ens", namehash: node,
      labelhash: name ? labelhash(name.split(".")[0]) : null,
      owner: st.owner ?? null, manager: st.manager ?? null, status: st.status, authority: st.authority,
      expires_at: st.expires_at ?? null, grace_ends_at: st.grace_ends_at ?? null,
      registered_at: s(firstSeen(node, ["registration"])),
      created_at: s(store.get("SELECT ts FROM names WHERE node = ?", node)?.ts ?? null),
    };
    if (st.ens_v1) row.ens_v1 = st.ens_v1;
    if (st.authority === "ens_v2") {
      // History, not authority: when the name moved from ENSv1 (a completed migration).
      const m = store.get("SELECT ts FROM migrations WHERE node = ? ORDER BY block DESC LIMIT 1", node);
      row.is_migrated = !!m;
      if (m?.ts) row.migrated_at = String(m.ts);
    }
    if (st.unsupported_reason) Object.assign(row, { read_status: "unsupported", unsupported_reason: st.unsupported_reason });
    row._resolver = st.resolver ?? null;
    return row;
  }
  const s = (x) => (x == null ? null : String(x));
  const strip = (r) => {
    const { _resolver, ...rest } = r;
    return rest;
  };

  function parseName(raw) {
    const n = normalizeName(decodeURIComponent(raw));
    if (n.invalid) throw bad(`invalid name: ${n.invalid}`);
    if (n.name.endsWith(".base.eth")) throw unsupported("basenames is not indexed by smallname", "not_supported_for_namespace");
    return n.name;
  }
  function parseAddress(raw) {
    if (!isAddress(raw, { strict: false })) throw bad("invalid address");
    return getAddress(raw);
  }
  function pageSize(q) {
    const n = q.get("page_size") == null ? PAGE_DEFAULT : Number(q.get("page_size"));
    if (!Number.isInteger(n) || n < 1 || n > PAGE_MAX) throw bad(`page_size must be 1..${PAGE_MAX}`);
    return n;
  }
  function checkNamespace(q) {
    const ns = q.get("namespace");
    if (ns && ns !== "ens") throw unsupported(`namespace ${ns} is not indexed by smallname`, "not_supported_for_namespace");
  }
  /**
   * Fills a page from index candidates that are re-checked live: `candidates(after, n)` returns
   * rows in key order, `check(row)` the response row or null (no longer holds). Checks continue
   * past dropped candidates (bounded), so a page is full unless the candidates run out.
   */
  async function fill(candidates, check, keyOf, size, after) {
    const out = [];
    let last = after;
    let more = false;
    for (let rounds = 0; out.length < size && rounds < 8; rounds++) {
      const batch = candidates(last, size * 2);
      if (!batch.length) break;
      const checked = await Promise.all(batch.map(check));
      for (let i = 0; i < batch.length; i++) {
        last = keyOf(batch[i]);
        if (checked[i]) out.push(checked[i]);
        if (out.length === size) {
          more = i < batch.length - 1 || candidates(last, 1).length > 0;
          break;
        }
      }
      if (out.length < size && batch.length < size * 2) {
        more = false;
        break;
      }
      more = true;
    }
    return { rows: out, last, more };
  }
  const page = (cursor, next, size, total = null) => ({ cursor: cursor ?? null, next_cursor: next, page_size: size, total_count: total, has_more: !!next });

  // ---- routes ----
  async function nameDetail(rd, q, name) {
    const node = namehash(name);
    const row = await nameRow(rd, name, node);
    if (row.read_status === "failed") return { data: row, meta: asOf(rd) };
    if (row.status === "unregistered" && !row.owner && !store.get("SELECT 1 FROM names WHERE node = ?", node)) throw new ApiError(404, "not_found", `${name} is not registered`);
    const keys = inventory(node, row._resolver);
    const want = [...new Set(["addr:60", ...keys])];
    const rec = await records(rd, name, want);
    const g = { seen_addresses: [], addresses: {}, seen_texts: [], texts: {}, seen_singletons: [], abis: {} };
    for (const k of want) {
      const a = rec.records[k];
      const v = a.status === "ok" ? a.value : a.status === "not_found" ? null : undefined;
      if (k.startsWith("addr:")) {
        const c = k.slice(5);
        if (keys.includes(k) || v) g.seen_addresses.push(c);
        if (v !== undefined && (keys.includes(k) || v)) g.addresses[c] = v;
      } else if (k.startsWith("text:")) {
        g.seen_texts.push(k.slice(5));
        if (v !== undefined) g.texts[k.slice(5)] = v;
      } else if (k === "contenthash" || k === "name") {
        g.seen_singletons.push(k);
        if (v !== undefined) g[k] = v;
      }
    }
    const data = {
      ...strip(row), read_status: "ok",
      resolver: ref(rec.resolver && rec.resolver !== "0x0000000000000000000000000000000000000000" ? row._resolver ?? rec.resolver : row._resolver),
      records: g, primary_address: rec.records["addr:60"]?.status === "ok" ? rec.records["addr:60"].value : null,
      chain_id: CHAIN_ID, network: "sepolia",
    };
    if (q.get("include")?.split(",").includes("counts")) {
      data.subname_count = store.get("SELECT count(*) c FROM names WHERE parent = ?", node).c;
      data.record_count = keys.length;
    }
    const unknown = ["registered_at", "created_at"].filter((k) => data[k] == null);
    const meta = { ...asOf(rd), source: "verified" };
    if (unknown.length) Object.assign(meta, { unsupported_fields: unknown, unsupported_reason: partialMeta().unsupported_reason });
    // Record keys are known from writes since the fork; `addr:60` is always read.
    if (unknown.length) meta.records_completeness = "partial";
    return { data, meta };
  }

  async function nameRecords(rd, q, name) {
    const node = namehash(name);
    const st = await nameState(rd, name, { releasedInV2: releasedInV2(node) });
    const inv = inventory(node, st.resolver);
    let keys = q.get("keys") ? q.get("keys").split(",").filter(Boolean) : inv.length ? inv : ["addr:60"];
    if (keys.length > 200) throw unsupported("at most 200 keys", "too_many_keys");
    keys = keys.map((k) => (k === "avatar" ? "text:avatar" : k));
    const rec = await records(rd, name, keys);
    if (q.get("values") === "false") for (const a of Object.values(rec.records)) delete a.value;
    const data = { namespace: "ens", canonical_name: name, resolver: ref(st.resolver), records: rec.records };
    if (q.get("include")?.split(",").includes("inventory")) {
      data.inventory = {
        known_keys: inv.filter((k) => rec.records[k]?.status === "ok"),
        unset_keys: inv.filter((k) => rec.records[k]?.status === "not_found"),
        unsupported_keys: [], abi_content_types: inv.filter((k) => k.startsWith("abi:")).map((k) => Number(k.slice(4))),
      };
    }
    return { data, meta: { ...asOf(rd), source: "verified" } };
  }

  async function subnames(rd, q, name, cursor) {
    const node = namehash(name);
    const size = pageSize(q);
    const scope = `sub:${node}:${q.get("q") ?? ""}:${q.get("match") ?? ""}:${q.get("include_expired") ?? ""}`;
    const after = cursor ? dec(cursor, scope).k : "";
    const qq = q.get("q");
    const like = qq ? (q.get("match") === "contains" ? `%${qq}%` : `${qq}%`) : "%";
    // Children in label order (from the (parent, label) index); unknown labels as `[labelhash]`.
    const cands = (k, n) => store.all(
      `SELECT node, label FROM names WHERE parent = ? AND label > ? ${qq ? "AND label LIKE ?" : ""} ORDER BY label LIMIT ?`,
      node, k, ...(qq ? [like] : []), n);
    const check = async (r) => {
      const row = await nameRow(rd, `${r.label}.${name}`, r.node);
      return q.get("include_expired") === "true" || row.status === "active" || row.status === "expired" ? strip(row) : null;
    };
    const f = await fill(cands, check, (r) => r.label, size, after);
    const total = store.get("SELECT count(*) c FROM names WHERE parent = ?", node).c;
    return { data: f.rows, page: page(cursor, f.more ? enc({ s: scope, k: f.last }) : null, size, total), meta: { ...asOf(rd), ...partialMeta() } };
  }

  function historyQuery(q, where, args, scope, cursor) {
    const size = pageSize(q);
    const order = q.get("order") === "asc" ? "ASC" : "DESC";
    const types = q.get("type")?.split(",").filter(Boolean);
    const exclude = q.get("exclude_type")?.split(",").filter(Boolean);
    for (const t of [...(types ?? []), ...(exclude ?? [])]) if (!HISTORY_TYPES.includes(t)) throw bad(`unknown type ${t}`);
    const conds = [where];
    const a = [...args];
    if (types) {
      conds.push(`type IN (${types.map(() => "?").join(",")})`);
      a.push(...types);
    }
    if (exclude) {
      conds.push(`type NOT IN (${exclude.map(() => "?").join(",")})`);
      a.push(...exclude);
    }
    if (q.get("from_timestamp")) conds.push("ts >= ?") && a.push(Number(q.get("from_timestamp")));
    if (q.get("to_timestamp")) conds.push("ts <= ?") && a.push(Number(q.get("to_timestamp")));
    if (q.get("from_block")) conds.push("block >= ?") && a.push(Number(q.get("from_block")));
    if (q.get("to_block")) conds.push("block <= ?") && a.push(Number(q.get("to_block")));
    const sc = `${scope}:${order}:${types ?? ""}:${exclude ?? ""}`;
    if (cursor) {
      const c = dec(cursor, sc);
      conds.push(order === "DESC" ? "(block < ? OR (block = ? AND log_index < ?))" : "(block > ? OR (block = ? AND log_index > ?))");
      a.push(c.b, c.b, c.l);
    }
    const rows = store.all(`SELECT * FROM events WHERE ${conds.join(" AND ")} ORDER BY block ${order}, log_index ${order} LIMIT ?`, ...a, size + 1);
    const more = rows.length > size;
    const inc = new Set(q.get("include")?.split(",") ?? []);
    const data = rows.slice(0, size).map((e) => {
      const ev = {
        id: `${e.block.toString(16).padStart(16, "0")}${e.log_index.toString(16).padStart(8, "0")}${e.block_hash?.slice(2, 42) ?? ""}`.padEnd(64, "0"),
        type: e.type, name: e.node ? nameOf(e.node) : null, namespace: "ens", registration_id: null,
        block_number: e.block, timestamp: s(e.ts), transaction_hash: e.tx, log_index: e.log_index,
      };
      if (inc.has("raw")) ev.kind = e.kind;
      if (inc.has("data")) Object.assign(ev, { contract_address: e.address, data: JSON.parse(e.data) });
      return ev;
    });
    const last = rows[size - 1];
    let total = null;
    if (inc.has("total_count")) total = store.get(`SELECT count(*) c FROM events WHERE ${conds.slice(0, cursor ? -1 : undefined).join(" AND ")}`, ...a.slice(0, cursor ? -3 : undefined)).c;
    return { data, page: page(cursor, more ? enc({ s: sc, b: last.block, l: last.log_index }) : null, size, total), meta: { ...partialMeta(), index: indexMeta(null) } };
  }

  async function addressNames(rd, q, address, cursor) {
    const size = pageSize(q);
    const relation = q.get("relation") ?? "any";
    const coin = q.get("coin_type") ?? "60";
    const rels = relation === "any" ? ["owner", "manager"] : relation === "resolves_to" ? [`addr:${coin === "evm" ? "60" : coin}`] : [relation];
    for (const r of rels) if (!["owner", "manager"].includes(r) && !r.startsWith("addr:")) throw unsupported(`relation ${relation} is not supported`, "relation_not_supported");
    const scope = `addr:${address}:${relation}:${coin}`;
    const after = cursor ? dec(cursor, scope).k : "";
    // Candidates in node order; each page re-checks them live.
    const cands = (k, n) => store.all(
      `SELECT node, group_concat(relation) AS rels FROM rel WHERE address = ? AND relation IN (${rels.map(() => "?").join(",")}) AND node > ? GROUP BY node ORDER BY node LIMIT ?`,
      address.toLowerCase(), ...rels, k, n);
    const check = async (r) => {
      const name = await nameOfLive(rd, r.node);
      if (!name) return null;
      const row = await nameRow(rd, name, r.node);
      const holds = [];
      if (row.owner?.toLowerCase() === address.toLowerCase()) holds.push("owner");
      if (row.manager?.toLowerCase() === address.toLowerCase()) holds.push("manager");
      if (rels[0].startsWith("addr:")) {
        const rec = await records(rd, name, [rels[0]]);
        if (rec.records[rels[0]]?.value?.toLowerCase() === address.toLowerCase()) holds.push("resolves_to");
      }
      const wanted = relation === "any" ? holds.filter((h) => h !== "resolves_to") : holds.filter((h) => h === relation);
      return wanted.length && row.status !== "released" ? { ...strip(row), relations: wanted } : null;
    };
    const f = await fill(cands, check, (r) => r.node, size, after);
    const out = f.rows;
    const primary = await primaryName(rd, address);
    for (const r of out) r.is_primary = primary.status === "ok" && primary.name === r.name;
    out.sort((a, b) => (a.name < b.name ? -1 : 1));
    return { data: out, page: page(cursor, f.more ? enc({ s: scope, k: f.last }) : null, size), meta: { ...asOf(rd), ...partialMeta() } };
  }

  async function search(rd, q, cursor) {
    const term = q.get("q");
    if (!term) throw bad("q is required");
    const size = pageSize(q);
    const like = q.get("match") === "contains" ? `%${term}%` : `${term}%`;
    const scope = `search:${term}:${q.get("match") ?? ""}`;
    const after = cursor ? dec(cursor, scope).k : "";
    const cands = (k, n) => store.all(
      "SELECT n.node, l.label FROM labels l JOIN names n ON n.labelhash = l.labelhash WHERE l.label LIKE ? AND (l.label || n.node) > ? ORDER BY l.label, n.node LIMIT ?",
      like, k, n);
    const check = async (r) => {
      const name = await nameOfLive(rd, r.node);
      if (!name) return null;
      const row = await nameRow(rd, name, r.node);
      return row.status === "active" || row.status === "expired" ? strip(row) : null;
    };
    const f = await fill(cands, check, (r) => r.label + r.node, size, after);
    return { data: f.rows, page: page(cursor, f.more ? enc({ s: scope, k: f.last }) : null, size), meta: { ...asOf(rd), ...partialMeta() } };
  }

  async function expiring(rd, q, cursor) {
    checkNamespace(q);
    if (!q.get("namespace")) throw bad("namespace is required");
    const size = pageSize(q);
    const after = Number(q.get("expires_after") ?? 0);
    const before = Number(q.get("expires_before") ?? 2 ** 53);
    const order = q.get("order") === "desc" ? "DESC" : "ASC";
    const scope = `exp:${after}:${before}:${order}`;
    const c0 = cursor ? dec(cursor, scope) : null;
    // Latest emitted expiry per node and source; the live check below decides.
    const cands = (k, n) => store.all(
      `WITH latest AS (SELECT node, source, expires, row_number() OVER (PARTITION BY node, source ORDER BY block DESC, log_index DESC) AS rn FROM expiry)
       SELECT node, max(expires) AS expires FROM latest WHERE rn = 1 AND source IN ('v1_lease', 'v2') GROUP BY node
       HAVING expires >= ? AND expires < ? ${k ? (order === "ASC" ? "AND (expires > ? OR (expires = ? AND node > ?))" : "AND (expires < ? OR (expires = ? AND node < ?))") : ""}
       ORDER BY expires ${order}, node ${order} LIMIT ?`,
      after, before, ...(k ? [k.e, k.e, k.n] : []), n);
    const check = async (r) => {
      const name = await nameOfLive(rd, r.node);
      if (!name) return null;
      const row = await nameRow(rd, name, r.node);
      const e = Number(row.expires_at);
      return row.expires_at != null && e >= after && e < before ? strip(row) : null;
    };
    const f = await fill(cands, check, (r) => ({ e: r.expires, n: r.node }), size, c0 ? { e: c0.e, n: c0.n } : null);
    return { data: f.rows, page: page(cursor, f.more ? enc({ s: scope, ...f.last }) : null, size), meta: { ...asOf(rd), ...partialMeta() } };
  }

  async function lookup(rd, body) {
    if (!body || !Array.isArray(body.inputs)) throw bad("body must be {inputs: [...]}");
    if (body.inputs.length > 1000) throw bad("at most 1000 inputs");
    const detail = body.profile === "detail";
    const data = await Promise.all(
      body.inputs.map(async (input) => {
        if (typeof input === "string" && isAddress(input, { strict: false })) {
          const p = await primaryName(rd, getAddress(input));
          return { input, kind: "address", status: p.status, ...(p.name && { record: { name: p.name, display_name: p.name, namespace: "ens", namehash: namehash(p.name), is_primary: true } }) };
        }
        const n = normalizeName(String(input));
        if (n.invalid) return { input, kind: "name", status: "invalid_name" };
        const row = await nameRow(rd, n.name);
        const normalization = n.changed ? { normalization: { changed: true, input_name: input } } : {};
        if (row.read_status === "failed") return { input, kind: "name", status: "failed", ...normalization };
        if (row.status === "unregistered") return { input, kind: "name", status: "not_found", ...normalization };
        const out = { input, kind: "name", status: "ok", ...normalization, record: { ...strip(row), chain_id: CHAIN_ID, network: "sepolia" } };
        if (detail) out.record.records = (await records(rd, n.name, inventory(namehash(n.name), row._resolver).concat("addr:60"))).records;
        return out;
      }),
    );
    return { data, meta: asOf(rd) };
  }

  async function status() {
    const rd = await Reader.atHead(skim);
    const indexed = Number(store.meta("indexed_block") ?? 0);
    const lag = rd.head.block_number - indexed;
    const ts = store.get("SELECT ts FROM blocks WHERE number = ?", indexed)?.ts;
    return {
      data: {
        status: lag <= 3 ? "ready" : lag <= 64 ? "degraded" : "stale",
        pending_invalidation_count: 0, pending_invalidation_count_capped: false, dead_letter_count: 0,
        chains: {
          [CHAIN_ID]: {
            latest_block: rd.head.block_number, indexed_block: indexed, safe_block: null, finalized_block: store.meta("finalized_block") ? Number(store.meta("finalized_block")) : null, started_at_block: Number(store.meta("started_at_block") ?? 0) || null,
            lag_blocks: lag, lag_seconds: ts ? Math.max(0, Number(rd.head.timestamp) - ts) : null,
            network_block: rd.head.block_number, network_head_observed_at: rd.head.timestamp,
            network_head_age_seconds: Math.max(0, Math.round(Date.now() / 1000) - Number(rd.head.timestamp)),
            network_head_status: "ok", status: lag <= 3 ? "ready" : "behind",
            index: { events: store.get("SELECT count(*) c FROM events").c, names: store.get("SELECT count(*) c FROM names").c, labels: store.get("SELECT count(*) c FROM labels").c },
          },
        },
      },
      meta: {},
    };
  }

  const namespaces = (ns) => {
    if (ns !== "ens") throw new ApiError(404, "not_found", `namespace ${ns} is not indexed by smallname`);
    const full = { completeness: "full" };
    const partial = { completeness: "partial", unsupported_reason: partialMeta().unsupported_reason };
    return {
      data: {
        namespace: "ens",
        capabilities: { name_profile: full, name_history: partial, subnames: partial, verified_records: full, verified_primary_name: full },
        history_from_block: historyFrom(),
        networks: [{ network: "sepolia", chain_id: CHAIN_ID, resolution: { protocol: "ens_v2", since_block: 11820291 } }],
      },
      meta: {},
    };
  };

  // ---- contract views: resolvers, registries, permissions ----

  function parseContract(chainRaw, addrRaw) {
    if (String(chainRaw) !== String(CHAIN_ID)) throw new ApiError(404, "not_found", `chain ${chainRaw} is not indexed by smallname`);
    return parseAddress(addrRaw).toLowerCase();
  }
  /** What a resolver is, as far as the index knows. */
  function resolverKind(a) {
    if (V1_RESOLVERS.map((x) => x.toLowerCase()).includes(a)) return { kind: "ens_v1_public_resolver", declared: true };
    if (a === V2.publicResolverV2.toLowerCase()) return { kind: "ens_v2_public_resolver", declared: true };
    if (a === V2.ensv1Resolver.toLowerCase()) return { kind: "ens_v1_mirror_resolver", declared: true };
    if (store.get("SELECT 1 FROM v2_resolvers WHERE address = ?", a)) return { kind: "ens_v2_permissioned_resolver", declared: true };
    return { kind: "unknown", declared: false };
  }

  /** GET /v1/resolvers/{chain}/{address}: the resolver, and the names currently using it. */
  async function resolverOverview(rd, q, a, cursor) {
    const size = pageSize(q);
    const scope = `res:${a}`;
    const after = cursor ? dec(cursor, scope).k : "";
    // Candidates: names whose resolver was ever set to it; each is checked live.
    const cands = (k, n) => store.all(
      "SELECT DISTINCT node FROM events WHERE type = 'resolver' AND actor = ? AND node IS NOT NULL AND node > ? ORDER BY node LIMIT ?", a, k, n);
    const check = async (r) => {
      const name = await nameOfLive(rd, r.node);
      if (!name) return null;
      const row = await nameRow(rd, name, r.node);
      return row._resolver?.toLowerCase() === a ? strip(row) : null;
    };
    const f = await fill(cands, check, (r) => r.node, size, after);
    const k = resolverKind(a);
    return {
      data: {
        chain_id: CHAIN_ID, address: a, ...k,
        record_key_count: store.get("SELECT count(*) c FROM record_keys WHERE resolver = ?", a).c,
        bound_names: f.rows,
      },
      page: page(cursor, f.more ? enc({ s: scope, k: f.last }) : null, size),
      meta: { ...asOf(rd), ...partialMeta() },
    };
  }

  /**
   * GET /v1/resolvers/{chain}/{address}/links: an ENSv2 record-id resolver's links (`Linked`):
   * which record id each node uses, the latest per node.
   */
  function resolverLinks(q, a, cursor) {
    const size = pageSize(q);
    const scope = `links:${a}`;
    const after = cursor ? dec(cursor, scope).k : "";
    const rows = store.all(
      `WITH l AS (SELECT node, record_id, block, row_number() OVER (PARTITION BY node ORDER BY block DESC, log_index DESC) AS rn FROM v2_links WHERE resolver = ?)
       SELECT node, record_id, block FROM l WHERE rn = 1 AND node > ? ORDER BY node LIMIT ?`, a, after, size + 1);
    const more = rows.length > size;
    const data = rows.slice(0, size).map((r) => ({
      node: r.node, name: nameOf(r.node), record_id: BigInt("0x" + r.record_id).toString(), updated_block: r.block,
    }));
    return { data, page: page(cursor, more ? enc({ s: scope, k: rows[size - 1].node }) : null, size), meta: { index: indexMeta(null), ...partialMeta() } };
  }

  /** GET /v1/resolvers/{chain}/{address}/roles: EAC role grants on this resolver (latest per resource and account). */
  function resolverRoles(q, a, cursor) {
    const u = new URLSearchParams(q);
    u.set("contract_address", a);
    return permissions(u, cursor);
  }

  /** GET /v1/resolvers/{chain}/{address}/records?name=: what this resolver holds for a name. */
  async function resolverNameRecords(rd, q, a) {
    if (!q.get("name")) throw bad("name is required");
    const name = parseName(q.get("name"));
    const node = namehash(name);
    const inv = inventory(node, a);
    const keys = q.get("keys") ? q.get("keys").split(",").filter(Boolean) : inv.length ? inv : ["addr:60"];
    if (keys.length > 200) throw unsupported("at most 200 keys", "too_many_keys");
    const recs = await resolverRecords(rd, a, node, keys);
    return { data: { chain_id: CHAIN_ID, address: a, name, namehash: node, ...resolverKind(a), records: recs, known_keys: inv }, meta: { ...asOf(rd), source: "verified" } };
  }

  function registryRow(a) {
    const r = store.get("SELECT parent_node, fixed, block FROM registries WHERE address = ?", a);
    if (!r) throw new ApiError(404, "not_found", `${a} is not a known ENSv2 registry`);
    return r;
  }

  /** GET /v1/registries/{chain}/{address}: an ENSv2 registry's place in the tree and size. */
  async function registryOverview(rd, a) {
    const r = registryRow(a);
    const parentName = r.parent_node ? (r.parent_node === "0x" + "00".repeat(32) ? "" : await nameOfLive(rd, r.parent_node)) : null;
    return {
      data: {
        chain_id: CHAIN_ID, address: a,
        parent: r.parent_node ? { namehash: r.parent_node, name: parentName } : null,
        placement: r.fixed ? "confirmed" : r.parent_node ? "mounted" : "unmounted",
        label_count: store.get("SELECT count(*) c FROM v2_tokens WHERE registry = ?", a).c,
        announced_block: r.block || null,
      },
      meta: { ...asOf(rd), ...partialMeta() },
    };
  }

  /** GET /v1/registries/{chain}/{address}/labels: its labels, each read live (getState). */
  async function registryLabels(rd, q, a, cursor) {
    const r = registryRow(a);
    const size = pageSize(q);
    const owner = q.get("owner") ? parseAddress(q.get("owner")).toLowerCase() : null;
    const exclude = q.get("exclude_owner") ? parseAddress(q.get("exclude_owner")).toLowerCase() : null;
    const scope = `reg:${a}:${owner ?? ""}:${exclude ?? ""}`;
    const after = cursor ? dec(cursor, scope).k : "";
    const parentName = r.parent_node ? await nameOfLive(rd, r.parent_node) : null;
    const cands = (k, n) => store.all(
      `SELECT t.labelhash, t.node, coalesce(l.label, '[' || substr(t.labelhash, 3) || ']') AS label FROM v2_tokens t LEFT JOIN labels l ON l.labelhash = t.labelhash
       WHERE t.registry = ? AND t.labelhash > ? ORDER BY t.labelhash LIMIT ?`, a, k, n);
    const check = async (t) => {
      const e = await registryEntry(rd, a, t.labelhash);
      if (e.unavailable) return { label: t.label, labelhash: t.labelhash, read_status: "failed", failure_reason: "state_unavailable" };
      const o = e.owner?.toLowerCase() ?? null;
      if ((owner && o !== owner) || (exclude && o === exclude)) return null;
      const name = parentName != null ? (parentName ? `${t.label}.${parentName}` : t.label) : null;
      return { label: t.label, labelhash: t.labelhash, name, namehash: t.node, status: e.status, owner: e.owner, expires_at: e.expiry == null ? null : String(e.expiry) };
    };
    const f = await fill(cands, check, (t) => t.labelhash, size, after);
    return { data: f.rows, page: page(cursor, f.more ? enc({ s: scope, k: f.last }) : null, size), meta: { ...asOf(rd), ...partialMeta() } };
  }

  /**
   * GET /v1/permissions: current EAC role grants (ENSv2 registries and resolvers), from the
   * indexed EACRolesChanged events: the latest bitmap per (contract, resource, account); a zero
   * bitmap is a revoked grant and isn't listed. Name and registration anchors need resource ids
   * (TokenResource), which aren't indexed: those return 422.
   */
  function permissions(q, cursor) {
    if (q.get("registration_id")) throw unsupported("registration_id anchors are not supported by smallname", "registration_ids_not_indexed");
    const conds = ["kind = 'EACRolesChanged'"];
    const args = [];
    if (q.get("name")) {
      // A name's grants: the resource ids its registry tokens were assigned (TokenResource).
      const node = namehash(parseName(q.get("name")));
      const res = store.all("SELECT DISTINCT registry, resource FROM v2_resources WHERE node = ?", node);
      if (!res.length) return { data: [], page: page(cursor, null, pageSize(q)), meta: { index: indexMeta(null), ...partialMeta() } };
      conds.push(`(${res.map(() => "(address = ? AND json_extract(data, '$.resource') = ?)").join(" OR ")})`);
      for (const r of res) args.push(r.registry, r.resource);
    }
    if (q.get("address")) conds.push("actor = ?") && args.push(parseAddress(q.get("address")).toLowerCase());
    if (q.get("registry")) {
      const [c, ad] = q.get("registry").split(":");
      conds.push("address = ?") && args.push(parseContract(c, ad));
    }
    if (q.get("contract_address")) conds.push("address = ?") && args.push(parseAddress(q.get("contract_address")).toLowerCase());
    if (args.length === 0) throw bad("at least one of name, address, registry or contract_address is required");
    const size = pageSize(q);
    const scope = `perm:${args.join(",")}`;
    const after = cursor ? dec(cursor, scope).o : 0;
    const rows = store.all(
      `WITH g AS (
         SELECT address, json_extract(data, '$.resource') AS resource, lower(json_extract(data, '$.account')) AS account,
                json_extract(data, '$.newRoleBitmap') AS roles, block, ts,
                row_number() OVER (PARTITION BY address, json_extract(data, '$.resource'), lower(json_extract(data, '$.account')) ORDER BY block DESC, log_index DESC) AS rn
         FROM events WHERE ${conds.join(" AND ")})
       SELECT * FROM g WHERE rn = 1 AND roles != '0' ORDER BY address, resource, account LIMIT ? OFFSET ?`,
      ...args, size + 1, after);
    const more = rows.length > size;
    const data = rows.slice(0, size).map((r) => {
      const isRegistry = !!store.get("SELECT 1 FROM registries WHERE address = ?", r.address);
      return {
        contract: { chain_id: CHAIN_ID, address: r.address, kind: isRegistry ? "registry" : "resolver" },
        resource: String(r.resource), account: getAddress(r.account),
        roles: "0x" + BigInt(r.roles).toString(16),
        updated_block: r.block, updated_at: r.ts == null ? null : String(r.ts),
      };
    });
    return { data, page: page(cursor, more ? enc({ s: scope, o: after + size }) : null, size), meta: { index: indexMeta(null), ...partialMeta() } };
  }

  async function route(req, url) {
    const q = url.searchParams;
    const cursor = q.get("cursor");
    const p = url.pathname.replace(/\/+$/, "") || "/";
    let m;
    if (req.method === "GET" && p === "/v1/status") return status();
    if (req.method === "GET" && (m = p.match(/^\/v1\/namespaces\/([^/]+)$/))) return namespaces(m[1]);
    if (req.method === "POST" && p === "/v1/lookup") return lookup(await Reader.atHead(skim), await readJson(req));
    if (req.method !== "GET") throw new ApiError(405, "invalid_input", "method not allowed");
    if ((m = p.match(/^\/v1\/names\/([^/]+)$/))) return nameDetail(await Reader.atHead(skim), q, parseName(m[1]));
    if ((m = p.match(/^\/v1\/names\/([^/]+)\/records$/))) return nameRecords(await Reader.atHead(skim), q, parseName(m[1]));
    if ((m = p.match(/^\/v1\/names\/([^/]+)\/subnames$/))) return subnames(await Reader.atHead(skim), q, parseName(m[1]), cursor);
    if ((m = p.match(/^\/v1\/names\/([^/]+)\/history$/))) {
      const name = parseName(m[1]);
      return historyQuery(q, "node = ?", [namehash(name)], `h:${namehash(name)}`, cursor);
    }
    if (p === "/v1/names") return expiring(await Reader.atHead(skim), q, cursor);
    if ((m = p.match(/^\/v1\/addresses\/([^/]+)\/names$/))) return addressNames(await Reader.atHead(skim), q, parseAddress(m[1]), cursor);
    if ((m = p.match(/^\/v1\/addresses\/([^/]+)\/primary-name$/))) {
      const rd = await Reader.atHead(skim);
      const address = parseAddress(m[1]);
      const coin = q.get("coin_type") ?? "60";
      const v = await primaryName(rd, address, BigInt(coin));
      return {
        data: {
          address, coin_type: Number(coin), namespace: "ens",
          answers: [{ source: "indexed", status: "unsupported", unsupported_reason: "served_live_only" }, { source: "verified", ...v }],
          verification: { status: v.status, ...(v.name && { name: v.name }) },
        },
        meta: asOf(rd),
      };
    }
    if ((m = p.match(/^\/v1\/addresses\/([^/]+)\/history$/))) {
      const a = parseAddress(m[1]).toLowerCase();
      return historyQuery(q, "actor = ?", [a], `ah:${a}`, cursor);
    }
    if (p === "/v1/events") {
      const conds = ["1 = 1"];
      const args = [];
      if (q.get("name")) conds.push("node = ?") && args.push(namehash(parseName(q.get("name"))));
      if (q.get("address")) conds.push("actor = ?") && args.push(parseAddress(q.get("address")).toLowerCase());
      if (q.get("contract_address")) conds.push("address = ?") && args.push(q.get("contract_address").toLowerCase());
      return historyQuery(q, conds.join(" AND "), args, `ev:${args.join(",")}`, cursor);
    }
    if (p === "/v1/search") return search(await Reader.atHead(skim), q, cursor);
    if ((m = p.match(/^\/v1\/resolvers\/([^/]+)\/([^/]+)$/))) return resolverOverview(await Reader.atHead(skim), q, parseContract(m[1], m[2]), cursor);
    if ((m = p.match(/^\/v1\/resolvers\/([^/]+)\/([^/]+)\/records$/))) return resolverNameRecords(await Reader.atHead(skim), q, parseContract(m[1], m[2]));
    if ((m = p.match(/^\/v1\/registries\/([^/]+)\/([^/]+)$/))) return registryOverview(await Reader.atHead(skim), parseContract(m[1], m[2]));
    if ((m = p.match(/^\/v1\/registries\/([^/]+)\/([^/]+)\/labels$/))) return registryLabels(await Reader.atHead(skim), q, parseContract(m[1], m[2]), cursor);
    if (p === "/v1/permissions") return permissions(q, cursor);
    if ((m = p.match(/^\/v1\/resolvers\/([^/]+)\/([^/]+)\/links$/))) return resolverLinks(q, parseContract(m[1], m[2]), cursor);
    if ((m = p.match(/^\/v1\/resolvers\/([^/]+)\/([^/]+)\/roles$/))) return resolverRoles(q, parseContract(m[1], m[2]), cursor);
    if (/^\/v1\/diagnostics/.test(p)) throw unsupported("diagnostics are bigname's internals; smallname has none", "not_supported_by_smallname");
    throw new ApiError(404, "not_found", `no route ${p}`);
  }

  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url, "http://x");
    const cors = { "access-control-allow-origin": "*", "access-control-allow-headers": "content-type", "access-control-allow-methods": "GET, POST, OPTIONS" };
    if (req.method === "OPTIONS") return res.writeHead(204, cors).end();
    if (req.method === "GET" && (url.pathname === "/" || url.pathname === "/index.html")) return res.writeHead(200, { ...cors, "content-type": "text/html; charset=utf-8" }).end(html);
    if (url.pathname === "/healthz") return res.writeHead(200, cors).end("ok");
    const t0 = performance.now();
    try {
      const body = await route(req, url);
      body.meta = { ...body.meta, smallname_ms: Math.round(performance.now() - t0) };
      res.writeHead(200, { ...cors, "content-type": "application/json", "cache-control": "no-store" }).end(JSON.stringify(body, (_, v) => (typeof v === "bigint" ? v.toString() : v)));
    } catch (e) {
      const err = e instanceof ApiError ? e : new ApiError(500, "internal_error", e.message);
      if (!(e instanceof ApiError)) console.error(e);
      res.writeHead(err.status, { ...cors, "content-type": "application/json" }).end(JSON.stringify({ error: { code: err.code, message: err.message, details: err.details } }));
    }
  });
  server.listen(port, host, () => console.log(`smallname API on ${host ?? ""}:${port} (skim ${skim})`));
  return server;
}

async function readJson(req) {
  let b = "";
  for await (const c of req) {
    b += c;
    if (b.length > 1 << 20) throw bad("body too large");
  }
  try {
    return JSON.parse(b);
  } catch {
    throw bad("body is not JSON");
  }
}
