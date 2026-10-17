// Logs -> index rows. Pure bookkeeping: no current-state decisions are made here (owners,
// resolvers, records and expiry status are read live per request). What's kept is what state
// can't give back: preimages, the name tree, history, and candidates for lists.
import { decodeEventLog, encodePacked, keccak256, labelhash as lh, namehash, toEventSelector, hexToBytes, bytesToString, toHex } from "viem";
import { EVENTS, V1, V1_CONTROLLERS, V1_RESOLVERS, V2, V2_DISCOVERY_EVENTS, ETH_NODE, V1_GRACE_S } from "./contracts.mjs";

const ZERO = "0x0000000000000000000000000000000000000000";
const ROOT = "0x" + "00".repeat(32);
const ADDR_REVERSE = namehash("addr.reverse");
const lower = (a) => a.toLowerCase();
const isSet = (list) => new Set(list.map(lower));

const V1_REGISTRIES = isSet([V1.registry, V1.registryOld]);
const CONTROLLERS = isSet(V1_CONTROLLERS);
const NODE_RESOLVERS = isSet([...V1_RESOLVERS, V2.publicResolverV2]);
const V2_DISCOVERY = new Set(V2_DISCOVERY_EVENTS);

// topic0 -> [event name, abi item]
const BY_TOPIC = new Map();
for (const [name, item] of Object.entries(EVENTS)) BY_TOPIC.set(toEventSelector(item), [name, item]);
export const TOPICS = Object.fromEntries(Object.entries(EVENTS).map(([n, i]) => [n, toEventSelector(i)]));

export const child = (parent, labelHash) => keccak256(encodePacked(["bytes32", "bytes32"], [parent, labelHash]));
const tokenNode = (id) => toHex(BigInt(id), { size: 32 });
const labelId = (x) => (BigInt(x) >> 32n).toString(16);

/** DNS wire format -> labels (`\x03eth\x00` -> ["eth"]). */
export function dnsLabels(hex) {
  const b = hexToBytes(hex);
  const out = [];
  for (let i = 0; i < b.length && b[i] !== 0; i += b[i] + 1) out.push(bytesToString(b.slice(i + 1, i + 1 + b[i])));
  return out;
}

const json = (o) => JSON.stringify(o, (_, v) => (typeof v === "bigint" ? v.toString() : v));

export class Ingest {
  constructor(store) {
    this.s = store;
    for (const [a, parent] of [[V2.rootRegistry, ROOT], [V2.ethRegistry, ETH_NODE]]) {
      store.run("INSERT OR IGNORE INTO registries (address, parent_node, block) VALUES (?, ?, 0)", lower(a), parent);
    }
  }

  /** A label preimage, kept only if it hashes to `labelHash`. */
  preimage(label, labelHash) {
    if (typeof label !== "string" || !label) return false;
    if (labelHash && lh(label) !== labelHash) return false;
    if (this.s.run("INSERT OR IGNORE INTO labels (labelhash, label) VALUES (?, ?)", lh(label), label).changes) {
      this.s.run("UPDATE names SET label = ? WHERE labelhash = ? AND label LIKE '[%'", label, lh(label));
    }
    return true;
  }

  /** `name` names `node`: its labels are preimages and its edges join the tree. */
  fullName(name, node, block, ts) {
    if (!name || namehash(name) !== node) return;
    let parent = ROOT;
    for (const label of name.split(".").reverse()) {
      this.preimage(label);
      const n = child(parent, lh(label));
      this.edge(n, parent, lh(label), block, ts);
      parent = n;
    }
  }

  edge(node, parent, labelHash, block, ts) {
    return this.s.run(
      "INSERT OR IGNORE INTO names (node, parent, labelhash, block, ts, label) VALUES (?, ?, ?, ?, ?, coalesce((SELECT label FROM labels WHERE labelhash = ?), ?))",
      node, parent, labelHash, block, ts, labelHash, `[${labelHash.slice(2)}]`).changes > 0;
  }
  rel(node, address, relation, block) {
    if (!node || !address || lower(address) === ZERO) return;
    this.s.run("INSERT OR IGNORE INTO rel (node, address, relation, block) VALUES (?, ?, ?, ?)", node, lower(address), relation, block);
  }
  expiry(node, source, expires, l) {
    if (!node) return;
    this.s.run("INSERT OR REPLACE INTO expiry (node, source, expires, block, log_index) VALUES (?, ?, ?, ?, ?)", node, source, Number(expires), l.block, l.logIndex);
  }
  recordKey(node, resolver, key, l) {
    this.s.run(
      "INSERT INTO record_keys (node, resolver, key, block, log_index) VALUES (?, ?, ?, ?, ?) ON CONFLICT DO UPDATE SET block = excluded.block, log_index = excluded.log_index",
      node, lower(resolver), key, l.block, l.logIndex);
  }
  registryNode(registry) {
    return this.s.get("SELECT parent_node FROM registries WHERE address = ?", lower(registry))?.parent_node ?? null;
  }
  v2Node(registry, tokenId) {
    return this.s.get("SELECT node FROM v2_tokens WHERE registry = ? AND label_id = ?", lower(registry), labelId(tokenId))?.node ?? null;
  }
  linkedNodes(resolver, recordId) {
    return this.s.all("SELECT DISTINCT node FROM v2_links WHERE resolver = ? AND record_id = ?", lower(resolver), BigInt(recordId).toString(16)).map((r) => r.node);
  }

  /** A registry learned its parent node: name its tokens. */
  mount(registry, parentNode, block) {
    this.s.run("INSERT INTO registries (address, parent_node, block) VALUES (?, ?, ?) ON CONFLICT (address) DO UPDATE SET parent_node = excluded.parent_node", lower(registry), parentNode, block);
    for (const t of this.s.all("SELECT label_id, labelhash FROM v2_tokens WHERE registry = ?", lower(registry))) {
      const node = child(parentNode, t.labelhash);
      this.s.run("UPDATE v2_tokens SET node = ? WHERE registry = ? AND label_id = ?", node, lower(registry), t.label_id);
      this.edge(node, parentNode, t.labelhash, block, null);
    }
  }

  /** One log (viem/RPC shape with numeric fields already parsed). Returns true if indexed. */
  apply(raw, ts) {
    const hit = BY_TOPIC.get(raw.topics[0]);
    if (!hit) return false;
    const [name, item] = hit;
    let args;
    try {
      args = decodeEventLog({ abi: [item], data: raw.data, topics: raw.topics, strict: true }).args;
    } catch {
      return false; // same selector, different indexing (another contract's event)
    }
    const addr = lower(raw.address);
    const l = { block: raw.blockNumber, logIndex: raw.logIndex };
    const isRegistry = !!this.s.get("SELECT 1 FROM registries WHERE address = ?", addr);
    const isV2Resolver = !!this.s.get("SELECT 1 FROM v2_resolvers WHERE address = ?", addr);
    if (V2_DISCOVERY.has(name) && !["RegistryCreated", "ResolverCreated"].includes(name)) {
      const resolverEvent = ["Linked", "AddressUpdated", "ContenthashUpdated", "TextUpdated", "NameUpdated"].includes(name);
      if (resolverEvent ? !isV2Resolver : !isRegistry) return false;
    }
    let node = null;
    let actor = null;
    let type = null;
    const ev = (t, n = node, a = actor) => {
      type = t;
      node = n;
      actor = a ? lower(a) : null;
    };
    switch (name) {
      // ---- ENSv1 registry ----
      case "NewOwner": {
        if (!V1_REGISTRIES.has(addr)) return false;
        const n = child(args.node, args.label);
        const created = this.edge(n, args.node, args.label, l.block, ts);
        this.rel(n, args.owner, "manager", l.block);
        ev(created ? "registration" : "transfer", n, args.owner);
        break;
      }
      case "RegistryTransfer":
        if (!V1_REGISTRIES.has(addr)) return false;
        this.rel(args.node, args.owner, "manager", l.block);
        ev("transfer", args.node, args.owner);
        break;
      case "NewResolver":
        if (!V1_REGISTRIES.has(addr)) return false;
        ev("resolver", args.node, args.resolver);
        break;
      // ---- ENSv1 BaseRegistrar ----
      case "TokenTransfer": {
        if (addr !== lower(V1.baseRegistrar)) return false;
        const n = child(ETH_NODE, tokenNode(args.tokenId));
        this.rel(n, args.to, "owner", l.block);
        ev(lower(args.to) === lower(V2.graveyard) ? "migration" : "transfer", n, args.to);
        break;
      }
      case "RegistrarNameRegistered": {
        if (addr !== lower(V1.baseRegistrar)) return false;
        const n = child(ETH_NODE, tokenNode(args.id));
        this.edge(n, ETH_NODE, tokenNode(args.id), l.block, ts);
        this.rel(n, args.owner, "owner", l.block);
        this.expiry(n, "v1_lease", args.expires, l);
        ev("registration", n, args.owner);
        break;
      }
      case "RegistrarNameRenewed": {
        if (addr !== lower(V1.baseRegistrar)) return false;
        const n = child(ETH_NODE, tokenNode(args.id));
        this.expiry(n, "v1_lease", args.expires, l);
        ev("renewal", n);
        break;
      }
      case "ControllerNameRenewed": {
        if (!CONTROLLERS.has(addr)) return false;
        this.preimage(args.name, args.label);
        const n = child(ETH_NODE, args.label);
        // NameWrapper.renew sets the wrapper expiry to lease expiry + grace, without an event.
        this.expiry(n, "wrapper", args.expires + V1_GRACE_S, l);
        ev("renewal", n);
        break;
      }
      // ---- NameWrapper ----
      case "NameWrapped": {
        if (addr !== lower(V1.nameWrapper)) return false;
        const labels = dnsLabels(args.name);
        this.fullName(labels.join("."), args.node, l.block, ts);
        this.rel(args.node, args.owner, "owner", l.block);
        this.expiry(args.node, "wrapper", args.expiry, l);
        ev("token", args.node, args.owner);
        break;
      }
      case "NameUnwrapped":
        if (addr !== lower(V1.nameWrapper)) return false;
        ev("token", args.node, args.owner);
        break;
      case "FusesSet":
        if (addr !== lower(V1.nameWrapper)) return false;
        ev("token", args.node);
        break;
      case "ExpiryExtended":
        if (addr !== lower(V1.nameWrapper)) return false;
        this.expiry(args.node, "wrapper", args.expiry, l);
        ev("renewal", args.node);
        break;
      case "TransferSingle":
      case "TransferBatch": {
        const ids = name === "TransferSingle" ? [args.id] : args.ids;
        let nodes;
        if (addr === lower(V1.nameWrapper)) nodes = ids.map(tokenNode);
        else if (isRegistry) nodes = ids.map((id) => this.v2Node(addr, id));
        else return false;
        for (const n of nodes) this.rel(n, args.to, "owner", l.block);
        ev(lower(args.to) === lower(V2.graveyard) ? "migration" : "transfer", nodes[0], args.to);
        break;
      }
      // ---- node-keyed resolvers ----
      case "AddrChanged":
      case "AddressChanged":
      case "TextChanged":
      case "TextChangedLegacy":
      case "ContenthashChanged":
      case "ABIChanged":
      case "NameChanged":
      case "VersionChanged": {
        if (!NODE_RESOLVERS.has(addr)) return false;
        const n = args.node;
        let key = null;
        let who = null;
        if (name === "AddrChanged") [key, who] = ["addr:60", args.a];
        if (name === "AddressChanged") {
          key = `addr:${args.coinType}`;
          if (args.newAddress.length === 42) who = args.newAddress;
        }
        if (name === "TextChanged" || name === "TextChangedLegacy") key = `text:${args.key}`;
        if (name === "ContenthashChanged") key = "contenthash";
        if (name === "ABIChanged") key = `abi:${args.contentType}`;
        if (name === "NameChanged") {
          key = "name";
          this.fullName(args.name, n, l.block, ts);
        }
        if (name === "VersionChanged") {
          this.s.run("INSERT OR IGNORE INTO versions (node, resolver, block, log_index) VALUES (?, ?, ?, ?)", n, addr, l.block, l.logIndex);
        } else {
          this.recordKey(n, addr, key, l);
        }
        if (who && key.startsWith("addr:")) this.rel(n, who, key, l.block);
        const reverse = name === "NameChanged" && this.s.get("SELECT 1 FROM names WHERE node = ? AND parent = ?", n, ADDR_REVERSE);
        ev(reverse ? "primary_name" : "record", n, who);
        break;
      }
      // ---- reverse ----
      case "ReverseClaimed": {
        if (addr !== lower(V1.reverseRegistrar)) return false;
        const label = lower(args.addr).slice(2);
        this.preimage(label);
        this.edge(args.node, ADDR_REVERSE, lh(label), l.block, ts);
        ev("primary_name", args.node, args.addr);
        break;
      }
      case "NameForAddrChanged":
        if (addr !== lower(V1.defaultReverseRegistrar)) return false;
        ev("primary_name", null, args.addr);
        break;
      // ---- ENSv2 registries ----
      case "RegistryCreated":
        this.s.run("INSERT OR IGNORE INTO registries (address, parent_node, block) VALUES (?, NULL, ?)", addr, l.block);
        ev("contract");
        break;
      case "ResolverCreated":
        this.s.run("INSERT OR IGNORE INTO v2_resolvers (address, block) VALUES (?, ?)", addr, l.block);
        ev("contract");
        break;
      case "LabelRegistered":
      case "LabelReserved": {
        this.preimage(args.label, args.labelHash);
        const parent = this.registryNode(addr);
        const n = parent ? child(parent, args.labelHash) : null;
        this.s.run("INSERT OR REPLACE INTO v2_tokens (registry, label_id, labelhash, node, block) VALUES (?, ?, ?, ?, ?)",
          addr, labelId(args.tokenId), args.labelHash, n, l.block);
        if (n) this.edge(n, parent, args.labelHash, l.block, ts);
        if (n && name === "LabelRegistered") this.rel(n, args.owner, "owner", l.block);
        this.expiry(n, "v2", args.expiry, l);
        ev(name === "LabelRegistered" ? "registration" : "reservation", n, args.owner);
        break;
      }
      case "LabelUnregistered":
        ev("release", this.v2Node(addr, args.tokenId));
        break;
      case "ExpiryUpdated": {
        const n = this.v2Node(addr, args.tokenId);
        this.expiry(n, "v2", args.newExpiry, l);
        ev("renewal", n);
        break;
      }
      case "SubregistryUpdated": {
        const n = this.v2Node(addr, args.tokenId);
        if (n && lower(args.subregistry) !== ZERO) {
          if (!this.s.get("SELECT 1 FROM registries WHERE address = ?", lower(args.subregistry)))
            this.s.run("INSERT INTO registries (address, parent_node, block) VALUES (?, NULL, ?)", lower(args.subregistry), l.block);
          this.mount(args.subregistry, n, l.block);
        }
        ev("subregistry", n, args.subregistry);
        break;
      }
      case "ResolverUpdated":
        ev("resolver", this.v2Node(addr, args.tokenId), args.resolver);
        break;
      case "TokenRegenerated": {
        // Same label, new version: the token id's high bits don't change.
        ev("token", this.v2Node(addr, args.newTokenId));
        break;
      }
      case "ParentUpdated": {
        const parentNode = this.registryNode(args.parent);
        this.preimage(args.label);
        if (parentNode) this.mount(addr, child(parentNode, lh(args.label)), l.block);
        ev("subregistry", parentNode ? child(parentNode, lh(args.label)) : null);
        break;
      }
      case "EACRolesChanged":
        if (!isRegistry && !isV2Resolver) return false;
        ev("permission", null, args.account);
        break;
      // ---- ENSv2 registrar / bridge ----
      case "V2NameRegistered":
      case "V2NameRenewed": {
        if (addr !== lower(V2.ethRegistrar) && addr !== lower(V2.renewalBridge)) return false;
        this.preimage(args.label);
        const n = child(ETH_NODE, lh(args.label));
        ev(name === "V2NameRegistered" ? "registration" : "renewal", n, args.owner);
        break;
      }
      // ---- ENSv2 record-id resolvers ----
      case "Linked": {
        this.s.run("INSERT OR IGNORE INTO v2_links (resolver, record_id, node, block, log_index) VALUES (?, ?, ?, ?, ?)",
          addr, BigInt(args.recordId).toString(16), args.node, l.block, l.logIndex);
        try {
          const labels = dnsLabels(args.name);
          this.fullName(labels.join("."), args.node, l.block, ts);
        } catch {}
        ev("record", args.node);
        break;
      }
      case "AddressUpdated":
      case "ContenthashUpdated":
      case "TextUpdated":
      case "NameUpdated": {
        const key = name === "AddressUpdated" ? `addr:${args.coinType}` : name === "TextUpdated" ? `text:${args.key}` : name === "NameUpdated" ? "name" : "contenthash";
        const nodes = this.linkedNodes(addr, args.recordId);
        let who = null;
        if (name === "AddressUpdated" && args.addressBytes.length === 42) who = args.addressBytes;
        for (const n of nodes) {
          this.recordKey(n, addr, key, l);
          if (who) this.rel(n, who, key, l.block);
        }
        ev("record", nodes[0] ?? null, who);
        break;
      }
      default:
        return false;
    }
    this.s.run(
      "INSERT OR IGNORE INTO events (block, log_index, tx_index, tx, block_hash, ts, address, kind, type, node, actor, data) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
      l.block, l.logIndex, raw.transactionIndex, raw.transactionHash, raw.blockHash, ts, addr, name, type, node, actor, json(args ?? {}));
    return true;
  }
}
