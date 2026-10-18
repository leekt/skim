// The index: an SQLite file (node:sqlite). It holds only what chain state can't answer. Every
// row carries the block that produced it, so a reorg is one `DELETE ... WHERE block > n` per
// table. Label preimages are the exception: they're checked against their hash, so they hold on
// any fork.
import { DatabaseSync } from "node:sqlite";

const SCHEMA = `
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT);
-- Followed blocks (recent ones, for reorg detection).
CREATE TABLE IF NOT EXISTS blocks (number INTEGER PRIMARY KEY, hash TEXT NOT NULL, parent TEXT, ts INTEGER);
-- Event history (the history and events routes).
CREATE TABLE IF NOT EXISTS events (
  block INTEGER NOT NULL, log_index INTEGER NOT NULL, tx_index INTEGER, tx TEXT, block_hash TEXT, ts INTEGER,
  address TEXT NOT NULL, kind TEXT NOT NULL, type TEXT NOT NULL, node TEXT, actor TEXT, data TEXT,
  PRIMARY KEY (block, log_index));
CREATE INDEX IF NOT EXISTS events_node ON events (node, block);
CREATE INDEX IF NOT EXISTS events_actor ON events (actor, block);
CREATE INDEX IF NOT EXISTS events_type ON events (type, block);
-- Label preimages (hash-checked) and the name tree (node = keccak(parent, labelhash)).
CREATE TABLE IF NOT EXISTS labels (labelhash TEXT PRIMARY KEY, label TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS names (node TEXT PRIMARY KEY, parent TEXT NOT NULL, labelhash TEXT NOT NULL, block INTEGER NOT NULL, ts INTEGER);
CREATE INDEX IF NOT EXISTS names_parent ON names (parent);
CREATE INDEX IF NOT EXISTS names_labelhash ON names (labelhash);
-- Candidates: addresses that ever held a relation to a node. Responses check them live.
CREATE TABLE IF NOT EXISTS rel (node TEXT NOT NULL, address TEXT NOT NULL, relation TEXT NOT NULL, block INTEGER NOT NULL,
  PRIMARY KEY (node, address, relation));
CREATE INDEX IF NOT EXISTS rel_address ON rel (address, relation);
-- Expiries as emitted (ENSv1 lease, wrapper, ENSv2 entry), for expiry-ordered lists.
CREATE TABLE IF NOT EXISTS expiry (node TEXT NOT NULL, source TEXT NOT NULL, expires INTEGER NOT NULL, block INTEGER NOT NULL, log_index INTEGER NOT NULL,
  PRIMARY KEY (node, source, block, log_index));
CREATE INDEX IF NOT EXISTS expiry_expires ON expiry (expires);
-- Record keys ever written per (node, resolver), and version resets: the record inventory.
CREATE TABLE IF NOT EXISTS record_keys (node TEXT NOT NULL, resolver TEXT NOT NULL, key TEXT NOT NULL, block INTEGER NOT NULL, log_index INTEGER NOT NULL,
  PRIMARY KEY (node, resolver, key));
CREATE TABLE IF NOT EXISTS versions (node TEXT NOT NULL, resolver TEXT NOT NULL, block INTEGER NOT NULL, log_index INTEGER NOT NULL,
  PRIMARY KEY (node, resolver, block, log_index));
-- ENSv2: registries (and the node they hold names under), token -> node, record-id links.
CREATE TABLE IF NOT EXISTS registries (address TEXT PRIMARY KEY, parent_node TEXT, block INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS v2_tokens (registry TEXT NOT NULL, label_id TEXT NOT NULL, labelhash TEXT NOT NULL, node TEXT, block INTEGER NOT NULL,
  PRIMARY KEY (registry, label_id));
CREATE TABLE IF NOT EXISTS v2_resolvers (address TEXT PRIMARY KEY, block INTEGER NOT NULL);
-- Completed ENSv1 -> ENSv2 migrations: the ENSv1 token sent to the Graveyard and the ENSv2
-- registration in the same transaction (bigname's rule). History only; authority is read live.
CREATE TABLE IF NOT EXISTS migrations (node TEXT NOT NULL, block INTEGER NOT NULL, ts INTEGER, tx TEXT,
  PRIMARY KEY (node, block));
CREATE TABLE IF NOT EXISTS v2_links (resolver TEXT NOT NULL, record_id TEXT NOT NULL, node TEXT NOT NULL, block INTEGER NOT NULL, log_index INTEGER NOT NULL,
  PRIMARY KEY (resolver, node, block, log_index));
`;

const REORG_TABLES = ["blocks", "events", "names", "rel", "expiry", "record_keys", "versions", "registries", "v2_tokens", "v2_resolvers", "v2_links", "migrations"];

export class Store {
  constructor(path) {
    this.db = new DatabaseSync(path);
    this.db.exec("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;");
    this.db.exec(SCHEMA);
    // names.label: the preimage, copied in when it's learned, so children list in label order
    // from an index (older index files get the column and are filled once).
    if (!this.db.prepare("SELECT 1 FROM pragma_table_info('names') WHERE name = 'label'").get()) {
      this.db.exec("ALTER TABLE names ADD COLUMN label TEXT");
      this.db.exec("UPDATE names SET label = (SELECT label FROM labels l WHERE l.labelhash = names.labelhash)");
    }
    // Unknown labels sort and display as `[labelhash]` until their preimage is learned.
    this.db.exec("UPDATE names SET label = '[' || substr(labelhash, 3) || ']' WHERE label IS NULL");
    this.db.exec("CREATE INDEX IF NOT EXISTS names_parent_label ON names (parent, label)");
    // registries.fixed: the registry's place is settled (seeded, or its own parent claim was
    // confirmed by the parent's mount); later mounts elsewhere don't move it.
    if (!this.db.prepare("SELECT 1 FROM pragma_table_info('registries') WHERE name = 'fixed'").get()) {
      this.db.exec("ALTER TABLE registries ADD COLUMN fixed INTEGER NOT NULL DEFAULT 0");
    }
    // Index files from before the migrations table: derive it once from the stored events.
    if (!this.db.prepare("SELECT 1 FROM migrations LIMIT 1").get() && !this.db.prepare("SELECT 1 FROM meta WHERE k = 'migrations_built'").get()) {
      this.db.exec(`INSERT OR IGNORE INTO migrations (node, block, ts, tx)
        SELECT g.node, g.block, g.ts, g.tx FROM events g JOIN events r ON r.tx = g.tx AND r.node = g.node
        WHERE g.type = 'migration' AND r.kind = 'LabelRegistered' AND r.address = '0xd4ebcbbdf463c9c45784603db0ddd499bc44a8b4'`);
      this.db.exec("INSERT OR REPLACE INTO meta (k, v) VALUES ('migrations_built', '1')");
    }
    this.stmts = new Map();
  }

  q(sql) {
    let s = this.stmts.get(sql);
    if (!s) {
      s = this.db.prepare(sql);
      this.stmts.set(sql, s);
    }
    return s;
  }
  all(sql, ...a) {
    return this.q(sql).all(...a);
  }
  get(sql, ...a) {
    return this.q(sql).get(...a);
  }
  run(sql, ...a) {
    return this.q(sql).run(...a);
  }

  tx(fn) {
    this.db.exec("BEGIN");
    try {
      const r = fn();
      this.db.exec("COMMIT");
      return r;
    } catch (e) {
      this.db.exec("ROLLBACK");
      throw e;
    }
  }

  meta(k, v) {
    if (v === undefined) return this.get("SELECT v FROM meta WHERE k = ?", k)?.v;
    this.run("INSERT INTO meta (k, v) VALUES (?, ?) ON CONFLICT (k) DO UPDATE SET v = excluded.v", k, String(v));
  }

  /** Forgets everything blocks above `n` produced (a reorg replaced them). */
  rollback(n) {
    this.tx(() => {
      for (const t of REORG_TABLES) this.run(`DELETE FROM ${t} WHERE ${t === "blocks" ? "number" : "block"} > ?`, n);
      this.meta("indexed_block", n);
      this.meta("indexed_hash", this.get("SELECT hash FROM blocks WHERE number = ?", n)?.hash ?? "");
    });
  }

  /**
   * Reorg bookkeeping (followed-block hashes, sent as `recent` for rewinds) is only needed above
   * finality: a verified finalized block can't be rewound past. Below it, one anchor is kept (the
   * newest row at or below finality). Without a finalized block, the last `keep` blocks are kept.
   */
  pruneBlocks(head, finalized = null, keep = 512) {
    if (finalized != null) {
      const anchor = this.get("SELECT max(number) n FROM blocks WHERE number <= ?", finalized)?.n;
      if (anchor != null) this.run("DELETE FROM blocks WHERE number < ?", anchor);
      return;
    }
    this.run("DELETE FROM blocks WHERE number < ?", head - keep);
  }
}
