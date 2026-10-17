// Current state, read from skim at one block hash per request: owners, expiries, resolvers,
// records, primary names. Nothing here is stored; the index only says where to look.
import { decodeFunctionResult, encodeFunctionData, labelhash, namehash, stringToBytes, toHex, bytesToHex, hexToBigInt, decodeErrorResult, parseAbi } from "viem";
import { normalize } from "viem/ens";
import { V1, V2, VIEWS, UNIVERSAL_RESOLVER, V2_STATUS, V1_GRACE_S, V2_GRACE_S, PARENT_CANNOT_CONTROL, ETH_NODE } from "./contracts.mjs";

const ZERO = "0x0000000000000000000000000000000000000000";
const UR_ERRORS = parseAbi([
  "error ResolverNotFound(bytes name)",
  "error ResolverNotContract(bytes name, address resolver)",
  "error UnsupportedResolverProfile(bytes4 selector)",
  "error ResolverError(bytes errorData)",
  "error ReverseAddressMismatch(string primary, bytes primaryAddress)",
  "error HttpError(uint16 status, string message)",
  "error OffchainLookup(address sender, string[] urls, bytes callData, bytes4 callbackFunction, bytes extraData)",
]);

/** DNS wire format of a normalized name. */
export function dnsEncode(name) {
  const parts = name ? name.split(".") : [];
  const bytes = [];
  for (const p of parts) {
    const b = stringToBytes(p);
    if (b.length > 255) throw new Error("label too long");
    bytes.push(b.length, ...b);
  }
  bytes.push(0);
  return bytesToHex(new Uint8Array(bytes));
}

/** A batch of eth_calls pinned to `blockHash`, as one JSON-RPC batch to skim. */
export class Reader {
  constructor(url, blockHash) {
    this.url = url;
    this.at = { blockHash };
  }

  static async atHead(url) {
    const res = await fetch(url, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "eth_getBlockByNumber", params: ["latest", false] }) });
    const h = (await res.json()).result;
    const r = new Reader(url, h.hash);
    r.head = { block_number: Number.parseInt(h.number, 16), block_hash: h.hash, timestamp: String(Number.parseInt(h.timestamp, 16)) };
    return r;
  }

  /** calls: [{to, abi, functionName, args}] -> [{ok, value} | {ok: false, error, data}]. Identical
   *  calls within one request (same block) are made once. */
  async many(calls) {
    if (!calls.length) return [];
    this.memo ??= new Map();
    const keys = calls.map((c) => `${c.to.toLowerCase()}:${encodeFunctionData({ abi: c.abi, functionName: c.functionName, args: c.args })}`);
    const todo = [];
    const resolvers = [];
    for (const [i, k] of keys.entries()) {
      if (!this.memo.has(k)) {
        let res;
        this.memo.set(k, new Promise((r) => (res = r)));
        todo.push(calls[i]);
        resolvers.push(res);
      }
    }
    if (todo.length) {
      const out = await this.fetchMany(todo).catch((e) => todo.map(() => ({ ok: false, error: e.message })));
      out.forEach((r, i) => resolvers[i](r));
    }
    return Promise.all(keys.map((k) => this.memo.get(k)));
  }

  async fetchMany(calls) {
    const body = calls.map((c, i) => ({
      jsonrpc: "2.0", id: i, method: "eth_call",
      params: [{ to: c.to, data: encodeFunctionData({ abi: c.abi, functionName: c.functionName, args: c.args }) }, this.at],
    }));
    const res = await fetch(this.url, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body), signal: AbortSignal.timeout(30_000) });
    const out = await res.json();
    const byId = new Map((Array.isArray(out) ? out : [out]).map((r) => [r.id, r]));
    return calls.map((c, i) => {
      const r = byId.get(i);
      if (!r) return { ok: false, error: "no response" };
      if (r.error) return { ok: false, error: r.error.message, code: r.error.code, data: r.error.data };
      try {
        return { ok: true, value: decodeFunctionResult({ abi: c.abi, functionName: c.functionName, data: r.result }) };
      } catch (e) {
        return { ok: false, error: `undecodable: ${e.shortMessage ?? e.message}` };
      }
    });
  }
  async one(c) {
    return (await this.many([c]))[0];
  }
}

/** A Universal Resolver revert, named (ResolverNotFound etc.). */
function urError(r) {
  if (r.code !== 3 || !r.data) return { refused: true, error: r.error }; // skim couldn't answer: not a chain fact
  try {
    return { reverted: decodeErrorResult({ abi: UR_ERRORS, data: r.data }).errorName };
  } catch {
    return { reverted: "unknown", data: r.data };
  }
}

const ENCODED = /^\[[0-9a-f]{64}\]$/;

/** ENSIP-15 normalization, label by label; `[labelhash]` labels (unknown preimages) pass as is. */
export function normalizeName(input) {
  try {
    const n = input.split(".").map((l) => (ENCODED.test(l.toLowerCase()) ? l.toLowerCase() : normalize(l))).join(".");
    return { name: n, changed: n !== input };
  } catch (e) {
    return { invalid: e.shortMessage ?? e.message };
  }
}

/** ENSv2 walk from the root registry: the registry holding the name's label, its entry and resolver. */
async function v2Entry(rd, labels) {
  let registry = V2.rootRegistry;
  // TLD first: each step is the subregistry of the previous label.
  for (let i = labels.length - 1; i >= 0; i--) {
    const label = labels[i];
    if (ENCODED.test(label)) return null; // ENSv2 registries are addressed by label text
    const id = hexToBigInt(labelhash(label));
    if (i === 0) {
      const [state, resolver] = await rd.many([
        { to: registry, abi: VIEWS.v2Registry, functionName: "getState", args: [id] },
        { to: registry, abi: VIEWS.v2Registry, functionName: "getResolver", args: [label] },
      ]);
      if (!state.ok) return { registry, unavailable: state.error };
      const s = state.value;
      let owner = null;
      if (s.status === 2) {
        const o = await rd.one({ to: registry, abi: VIEWS.v2Registry, functionName: "ownerOf", args: [s.tokenId] });
        owner = o.ok && o.value !== ZERO ? o.value : null;
      }
      return {
        registry, status: V2_STATUS[s.status] ?? String(s.status), expiry: s.expiry, tokenId: s.tokenId,
        owner, resolver: resolver.ok && resolver.value !== ZERO ? resolver.value : null,
      };
    }
    const sub = await rd.one({ to: registry, abi: VIEWS.v2Registry, functionName: "getSubregistry", args: [label] });
    if (!sub.ok || sub.value === ZERO) return null;
    registry = sub.value;
  }
  return null;
}

/** ENSv1 state of a node: registry (current, then old), .eth lease, NameWrapper. */
async function v1State(rd, node, labels) {
  const is2ld = labels.length === 2 && labels[1] === "eth";
  const id = hexToBigInt(labelhash(labels[0] ?? ""));
  const calls = [
    { to: V1.registry, abi: VIEWS.registry, functionName: "owner", args: [node] },
    { to: V1.registry, abi: VIEWS.registry, functionName: "resolver", args: [node] },
    { to: V1.registry, abi: VIEWS.registry, functionName: "recordExists", args: [node] },
    { to: V1.registryOld, abi: VIEWS.registry, functionName: "owner", args: [node] },
    { to: V1.nameWrapper, abi: VIEWS.nameWrapper, functionName: "getData", args: [hexToBigInt(node)] },
  ];
  if (is2ld) {
    calls.push({ to: V1.baseRegistrar, abi: VIEWS.baseRegistrar, functionName: "ownerOf", args: [id] });
    calls.push({ to: V1.baseRegistrar, abi: VIEWS.baseRegistrar, functionName: "nameExpires", args: [id] });
  }
  const [owner, resolver, exists, oldOwner, wrapped, leaseOwner, leaseExpires] = await rd.many(calls);
  for (const r of [owner, resolver, exists, wrapped]) if (!r.ok) return { unavailable: r.error };
  const w = wrapped.value;
  return {
    registryOwner: owner.value !== ZERO ? owner.value : null,
    resolver: resolver.value !== ZERO ? resolver.value : null,
    generation: exists.value ? "ens_v1" : oldOwner?.ok && oldOwner.value !== ZERO ? "ens_v0" : null,
    oldOwner: oldOwner?.ok && oldOwner.value !== ZERO ? oldOwner.value : null,
    wrapped: owner.value.toLowerCase() === V1.nameWrapper.toLowerCase(),
    wrapper: { owner: w[0] !== ZERO ? w[0] : null, fuses: w[1], expiry: w[2] },
    is2ld,
    // ownerOf reverts once the lease has expired (past grace too, for this registrar).
    leaseOwner: leaseOwner?.ok && leaseOwner.value !== ZERO ? leaseOwner.value : null,
    leaseExpires: leaseExpires?.ok ? leaseExpires.value : null,
  };
}

const s = (x) => (x == null ? null : String(x));
const MAX_U64 = 2n ** 64n - 1n;

/**
 * A name's current state at the reader's block: authority, owner/manager, expiry/status and
 * resolver. The authority rule follows the chain (bigname's ADR 0007): a registered ENSv2
 * entry decides; else a live ENSv1 holding; a released ENSv2 registration stays ENSv2.
 */
export async function nameState(rd, name, { releasedInV2 = false } = {}) {
  const node = namehash(name);
  const labels = name.split(".");
  const now = BigInt(rd.head.timestamp);
  const [v2, v1] = await Promise.all([labels.length && name ? v2Entry(rd, labels) : null, v1State(rd, node, labels)]);
  if (v1.unavailable || v2?.unavailable) return { unavailable: v1.unavailable ?? v2.unavailable };
  const out = { node, v1, v2 };
  if (v2 && v2.status === "registered") {
    out.authority = "ens_v2";
    out.owner = v2.owner;
    out.manager = v2.owner;
    const e = v2.expiry === MAX_U64 ? null : v2.expiry;
    out.expires_at = s(e);
    const grace = v2.registry.toLowerCase() === V2.ethRegistry.toLowerCase() ? V2_GRACE_S : 0n;
    out.grace_ends_at = e == null ? null : s(e + grace);
    out.status = e == null || now < e ? "active" : now < e + grace ? "expired" : "released";
    out.resolver = v2.resolver;
  } else if (releasedInV2 && !(v2?.status === "reserved")) {
    out.authority = "ens_v2";
    out.status = "released";
  } else if ([v1.registryOwner, v1.leaseOwner, v1.wrapper.owner].some((a) => a?.toLowerCase() === V2.graveyard.toLowerCase())) {
    // Migrated to ENSv2 (the ENSv1 side is held by the Graveyard), but its ENSv2 entry couldn't
    // be read (e.g. a label whose text isn't known). Never report the Graveyard as the owner.
    out.authority = "ens_v2";
    out.status = "unsupported";
    out.unsupported_reason = "migrated_label_unknown";
  } else if (v1.registryOwner || v1.leaseOwner || v1.oldOwner || (v1.wrapped && v1.wrapper.owner)) {
    out.authority = v1.generation === "ens_v0" ? "ens_v0" : "ens_v1";
    const wrapperLive = v1.wrapper.expiry === 0n || now <= v1.wrapper.expiry;
    if (v1.wrapped) {
      // Emancipated/locked names lose their holder at wrapper expiry; plain wrapped ones don't.
      const pcc = (Number(v1.wrapper.fuses) & PARENT_CANNOT_CONTROL) !== 0;
      out.owner = !wrapperLive && pcc ? null : v1.wrapper.owner;
      out.manager = out.owner;
    } else {
      out.owner = v1.is2ld ? v1.leaseOwner : v1.registryOwner ?? v1.oldOwner;
      out.manager = v1.registryOwner ?? v1.oldOwner;
    }
    if (v1.is2ld && v1.leaseExpires != null && v1.leaseExpires > 0n && v1.leaseExpires >= MAX_U64 - V1_GRACE_S) {
      // A lease that can't run out (max uint64, minus the grace the registrar adds): no expiry.
      out.expires_at = null;
      out.grace_ends_at = null;
      out.status = "active";
    } else if (v1.is2ld && v1.leaseExpires != null && v1.leaseExpires > 0n) {
      const e = v1.leaseExpires;
      out.expires_at = s(e);
      out.grace_ends_at = s(e + V1_GRACE_S);
      out.status = now < e ? "active" : now <= e + V1_GRACE_S ? "expired" : "released";
    } else if (v1.wrapped && v1.wrapper.expiry > 0n && (Number(v1.wrapper.fuses) & PARENT_CANNOT_CONTROL) !== 0) {
      out.expires_at = s(v1.wrapper.expiry);
      out.status = wrapperLive ? "active" : "released";
    } else {
      out.expires_at = null;
      out.status = out.owner || out.manager ? "active" : "released";
    }
    out.resolver = v1.resolver;
    out.ens_v1 = {
      expires_at: v1.is2ld && v1.leaseExpires < MAX_U64 - V1_GRACE_S ? s(v1.leaseExpires) : null,
      resolver: v1.resolver ? { chain_id: 11155111, address: v1.resolver.toLowerCase() } : null,
      wrapper_state: v1.wrapped ? "wrapped" : "unwrapped",
      wrapper_fuses: v1.wrapped ? { fuses: Number(v1.wrapper.fuses), cannot_unwrap: (Number(v1.wrapper.fuses) & 1) !== 0 } : null,
      wrapper_expires_at: v1.wrapped ? s(v1.wrapper.expiry) : null,
    };
  } else {
    out.authority = null;
    out.status = v2?.status === "reserved" ? "unregistered" : "unregistered";
  }
  if (v2?.status === "reserved") out.reserved_until = s(v2.expiry);
  return out;
}

/** A resolver record through the Universal Resolver: key is `addr:<coin>`, `text:<k>`, `contenthash` or `name`. */
export function recordCall(name, key) {
  const node = namehash(name);
  let data;
  if (key === "addr:60") data = encodeFunctionData({ abi: VIEWS.resolver, functionName: "addr", args: [node] });
  else if (key.startsWith("addr:")) data = encodeFunctionData({ abi: VIEWS.resolver, functionName: "addr", args: [node, BigInt(key.slice(5))] });
  else if (key.startsWith("text:")) data = encodeFunctionData({ abi: VIEWS.resolver, functionName: "text", args: [node, key.slice(5)] });
  else if (key === "contenthash") data = encodeFunctionData({ abi: VIEWS.resolver, functionName: "contenthash", args: [node] });
  else if (key === "name") data = encodeFunctionData({ abi: VIEWS.resolver, functionName: "name", args: [node] });
  else return null;
  return { to: UNIVERSAL_RESOLVER, abi: VIEWS.universalResolver, functionName: "resolve", args: [dnsEncode(name), data] };
}

function decodeRecord(key, bytes) {
  const fn = key === "addr:60" ? ["addr", 1] : key.startsWith("addr:") ? ["addr", 2] : key.startsWith("text:") ? ["text"] : [key];
  const abiItem = VIEWS.resolver.filter((x) => x.name === fn[0] && (fn[1] == null || x.inputs.length === fn[1]));
  const v = decodeFunctionResult({ abi: abiItem, functionName: fn[0], data: bytes });
  return v;
}

/** Record answers for `keys`: {status: ok|not_found|unsupported|failed, value?}. */
export async function records(rd, name, keys) {
  const calls = keys.map((k) => recordCall(name, k));
  const results = await rd.many(calls.filter(Boolean));
  let i = 0;
  const out = {};
  let resolver = null;
  for (const [j, k] of keys.entries()) {
    if (!calls[j]) {
      out[k] = { status: "unsupported", unsupported_reason: "record_key_not_supported" };
      continue;
    }
    const r = results[i++];
    if (!r.ok) {
      const e = urError(r);
      out[k] = e.refused ? { status: "failed", failure_reason: "state_unavailable" } : { status: "not_found", ...(e.reverted !== "ResolverNotFound" && { reason: e.reverted }) };
      continue;
    }
    resolver = r.value[1];
    try {
      let v = decodeRecord(k, r.value[0]);
      if (k.startsWith("addr:") && k !== "addr:60" && v === "0x") v = null;
      if (k === "addr:60" && v === ZERO) v = null;
      if (k === "contenthash" && v === "0x") v = null;
      if ((k.startsWith("text:") || k === "name") && v === "") v = null;
      out[k] = v == null ? { status: "not_found" } : { status: "ok", value: typeof v === "string" ? v : String(v) };
    } catch {
      out[k] = { status: "failed", failure_reason: "undecodable" };
    }
  }
  return { records: out, resolver };
}

/** Primary name through the Universal Resolver (forward-verified, ENSIP-19). */
export async function primaryName(rd, address, coinType = 60n) {
  const r = await rd.one({ to: UNIVERSAL_RESOLVER, abi: VIEWS.universalResolver, functionName: "reverse", args: [address, BigInt(coinType)] });
  if (r.ok) return r.value[0] ? { status: "ok", name: r.value[0] } : { status: "not_found" };
  const e = urError(r);
  if (e.refused) return { status: "failed", failure_reason: "state_unavailable" };
  if (e.reverted === "ReverseAddressMismatch") return { status: "mismatch" };
  return { status: "not_found" };
}

export { ETH_NODE };
