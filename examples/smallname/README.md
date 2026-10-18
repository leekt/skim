# smallname

**bigname, but small.** The same kind of ENS API ([bigname](https://github.com/ensdomains/bigname)'s `/v1` shape, Sepolia), with skim serving current state and a small SQLite index handling discovery and history, instead of a full application-side state projection.

Smaller infrastructure and implementation (about 1,700 lines of JavaScript, one SQLite file, one skim node), but not feature-for-feature parity yet: see [Routes](#routes) and the history limits below.

```bash
../target/release/skim run --config bigname/skim.yaml --history local   # terminal 1: skim, ENS kept warm, post-fork history on disk
SKIM_RPC=http://localhost:8545 npm run smallname -- run          # terminal 2: index + API on :8787
```
Open the test page at `/` on port 8787, or call the API:
```bash
curl localhost:8787/v1/names/nick.eth
curl localhost:8787/v1/addresses/0x179A862703a4adfb29896552DF9e307980D19285/names
```

## The split
smallname avoids maintaining a complete current-state projection. Each kind of answer comes from one place:

| | bigname | smallname |
|---|---|---|
| **Current values** (owner, manager, expiry, status, resolver, records, primary name) | projected from events | **read live from skim** at one block hash per response (`meta.as_of`), through the registries, NameWrapper and the Universal Resolver |
| **Lists and history** (subnames, names by address, search, expiry lists, timelines) | PostgreSQL | an SQLite index of events, label preimages, the name tree, and candidates: relationships, expiries and record keys. Every listed name is re-checked live before it's returned |
| **History** | every block from 0 (`eth_getBlockByNumber` for each block, plus logs) | **from Glamsterdam on**, through skim's `eth_getLogs` |
| **Chain access** | an archive / log provider | skim only |

**What this guarantees, and what it doesn't:**
- **Returned values are current and consistent:** every value in a response comes from contract state at the block in `meta.as_of`.
- **A stale or over-inclusive candidate** is dropped by the live check, so it can't put a wrong row in a list.
- **Lists aren't guaranteed complete.** If Alice transfers a name to Bob and the index hasn't processed that block yet, a lookup of the name shows Bob, Alice's list drops it, and Bob's list doesn't have it yet. Every response that uses the index reports `meta.index.indexed_block` and `lag_blocks`, next to `meta.as_of` (where values were read). Coverage before `history_from_block` is a separate limit, marked as described below.

**ENS rules come from the chain.** The Universal Resolver applies ENSv1/ENSv2 resolution. For ownership, smallname follows bigname's [ADR 0007](https://github.com/ensdomains/bigname) ("follow the chain"):
1. A registered ENSv2 entry decides.
2. Otherwise a live ENSv1 holding decides: the registry, the `.eth` registrar lease, or the NameWrapper. Fuses and wrapper expiry apply.
3. A released ENSv2 registration stays ENSv2.

## History: what skim can still fetch, then everything indexed
History comes from what skim can fetch when the index starts, and then the index keeps everything it indexed:
- **Never before Glamsterdam** (Sepolia block 11,856,337). skim keeps headers only from the fork on and answers `-32004` before it.
- **Never before what peers still serve.** Receipts come from peers, and each peer announces the oldest block it still serves (eth/69 `earliest_block`). skim reports the result in `skim_status.history.logs_from` and refuses older ranges.
  - On 2026-10-18, every connected peer reported `0` (full history), so the index reached back to the fork.
  - Once peers expire history (a couple of weeks), a new index starts later than the fork.
- **After that, the index keeps it.** Indexed events stay indexed however old they get.

**skim does the block work.** smallname reads skim's block stream (`skim_streamNext`): logs matching its filters, batch by batch from its cursor, with `rewind` after a reorg and `gap` where skim can no longer serve logs. Catch-up over history, the head, retries and reorg detection all live in skim; smallname only turns ENS events into index rows. Each batch is applied in one SQLite transaction together with the new cursor, so a crash re-delivers a batch (applying is idempotent) and never skips one.

Anything earlier is marked, not guessed:
- **History, events and lists:** `meta.completeness: "partial"` and `history_from_block`, which is where this index starts. `unsupported_reason` is `pre_glamsterdam_history` when the index starts at the fork, or `history_before_index_start` when it starts later.
- **Name detail:** `registered_at` / `created_at` are `null` and listed in `meta.unsupported_fields` when the registration predates the index.
- **`GET /v1/namespaces/ens`:** `name_history` and `subnames` are `partial`.

A name created before the index starts still resolves and has a full profile; the profile is live. It shows up in lists once something about it happens after that.

**Labels:**
- **Unknown labels:** a name whose label text was never seen in an event (`NewOwner` carries only its hash) is listed as `[labelhash].parent.eth`, as bigname does.
- **Wrapped ancestors:** when an ancestor predates the fork, smallname reads its full name from the NameWrapper (`names(node)`, live, checked against the node) and records it.
- **Unwrapped ancestors:** a pre-fork ancestor that isn't wrapped can't be named from state. Such rows are left out, under the same `partial` marker, unless a label list names them (below).
- **Label lists (optional):** `smallname import-labels <file>` reads a newline-separated list of labels, such as a published ENS label list (smallname downloads nothing). Each label is used only where its hash already occurs:
  - it fills in `[labelhash]` rows;
  - as `label.eth`, it names a `.eth` parent the index only knows by node.
  A list can't put a wrong name in the index, only fill gaps.

**Measured** (Sepolia, all post-fork history: 78k blocks, from 11,856,337 to 11,934,300):

| | |
|---|---|
| Index | 15,404 events, 6,866 names, 2,430 label preimages: **20 MB** |
| Catch-up through skim | 26 s per 2,048-block range with batched receipts (19k blocks in 242 s, so about 16 min for the whole post-fork history). The first run, with one receipt request per block, took 89 min |
| skim's header store for that history | 134 MB (`history.redb`), built once by walking headers back from peers (46 s) |

## Routes
| Route | Source |
|---|---|
| `GET /v1/names/{name}` (`include=counts`) | live; record keys from the index |
| `GET /v1/names/{name}/records` (`keys`, `include=inventory`, `values=false`) | live (Universal Resolver) |
| `GET /v1/names/{name}/subnames` (`q`, `match`, `include_expired`) | index → live check |
| `GET /v1/names/{name}/history`, `/v1/addresses/{a}/history`, `/v1/events` (`type`, `exclude_type`, `order`, `include=data,raw,total_count`, block and time ranges) | index |
| `GET /v1/addresses/{a}/names` (`relation=owner\|manager\|any\|resolves_to`, `coin_type`) | index → live check |
| `GET /v1/addresses/{a}/primary-name` (`coin_type`) | live (`UniversalResolver.reverse`, forward-verified) |
| `GET /v1/search` (`q`, `match=prefix\|contains`) | index → live check |
| `GET /v1/names?namespace=ens&expires_after=&expires_before=` | index → live check |
| `POST /v1/lookup` (`inputs`, `profile=feed\|detail`) | live |
| `GET /v1/resolvers/{chain_id}/{address}` | what the resolver is (declared ENSv1/ENSv2 kinds), and the names currently using it: index → live check |
| `GET /v1/resolvers/{chain_id}/{address}/records?name=` (`keys`) | live, direct calls to that resolver (not the Universal Resolver); known keys from the index |
| `GET /v1/registries/{chain_id}/{address}` | an ENSv2 registry's place in the tree (`confirmed`, `mounted` or `unmounted`) and label count: index |
| `GET /v1/registries/{chain_id}/{address}/labels` (`owner`, `exclude_owner`) | index → live (`getState`, `ownerOf`) |
| `GET /v1/resolvers/{chain_id}/{address}/links` | index: an ENSv2 record-id resolver's `Linked` record per node (latest) |
| `GET /v1/resolvers/{chain_id}/{address}/roles` | index: EAC role grants on that resolver (as `/v1/permissions`) |
| `GET /v1/permissions` (`name`, `address`, `registry=chain:addr`, `contract_address`) | index: the latest `EACRolesChanged` bitmap per (contract, resource, account); revoked grants (zero) left out. By `name`: the resource ids its registry tokens were assigned (`TokenResource`) |
| `GET /v1/status`, `GET /v1/namespaces/ens` | index position, finalized block, skim head |

- **Response shapes:** these follow bigname's: `{data, page, meta}`, opaque cursors, Unix-second strings, `{error: {code, message, details}}`.
- **Out of scope** (`422 unsupported`, on purpose):
  - `registration_id` anchors: smallname has no registration-identity model (bigname mints one per anchor change); names are its identity
  - diagnostics: bigname's views of its own pipeline (coverage, manifests); smallname has no such pipeline
  - Basenames (`*.base.eth`): another chain, which skim doesn't follow
- **Simplified shapes:** permission rows are `{contract, resource, account, roles, updated_block, updated_at}`, without bigname's lineage and effective-grant expansion.
- **Migration history:** `is_migrated` and `migrated_at` are set on ENSv2 names, from a completed ENSv1 → ENSv2 migration: the ENSv1 token sent to the Graveyard and the ENSv2 registration in the same transaction, as bigname defines it. `registered_at` skips the migration's registration, so it keeps meaning the original registration (null when that predates the index).
- **Primary names:** answered `verified` only. The `indexed` answer is `unsupported`, because smallname doesn't keep a stored claim.

## Finality
Each stream response carries the block skim's light client verified as finalized. smallname records it (`finalized_block` in `/v1/status` and `meta.index`). It keeps reorg bookkeeping (the block hashes it sends as `recent`) only above it, plus one anchor at or below it.

**Irreversible exports:** an app that exports from the index (to another database, a queue, an email) should export only rows at or below `finalized_block`. Rows above it can still be rolled back by a `rewind`. Rows at or below it can't: a finalized block isn't reorged out, which is what the light client verifies. Without a light client (`finalized_block: null`), nothing is final, and such exports should wait.

## Deploying
Service units, a tunnel config and sizes: [`deploy/`](deploy).

## Files
| File | |
|---|---|
| `contracts.mjs` | Sepolia ENS contracts and events, taken from bigname's Sepolia manifests |
| `ingest.mjs` | logs → index rows: preimages (hash-checked), name tree, history, candidates |
| `sync.mjs` | a consumer of skim's block stream (`skim_streamNext`): applies `apply` / `rewind` / `gap` events with the cursor in the same SQLite transaction |
| `live.mjs` | live reads at a block hash: name state, records, primary names |
| `api.mjs` | the `/v1` routes |
| `store.mjs` | SQLite schema; every row carries its block, so a reorg is one `DELETE … WHERE block > n` per table |
| `page.html` | the test page |

Env:
- `SKIM_RPC` (default `http://localhost:8545`)
- `DB` (default `smallname.db`)
- `PORT` (default `8787`); `HOST` to bind one interface (e.g. `127.0.0.1` behind a proxy)
- `FROM_BLOCK`: start a new index later than the fork

Node 23+, run with `--experimental-sqlite` (the npm script passes it).

## Latency
Same machine as skim, after the catch-up. **Cold** is the first request after skim restarted, when skim fetches that state from peers; **warm** is the best of the next two.

| Route | Rows | Cold | Warm |
|---|---|---|---|
| `GET /v1/names/nick.eth` | | 1,007 ms | 16 ms |
| `GET /v1/names/gregskril.eth/records?include=inventory` | | 695 ms | 15 ms |
| `GET /v1/addresses/0x179A…9285/primary-name` | | 553 ms | 13 ms |
| `GET /v1/names/arti.eth/subnames` | 3 | 1,013 ms | 18 ms |
| `GET /v1/names/nick.eth/history` | 0 (no post-fork events) | 15 ms | 14 ms |
| `GET /v1/events?type=registration` | 50 | 15 ms | 14 ms |
| `GET /v1/search?q=test` | 8 | 857 ms | 27 ms |
| `GET /v1/names?namespace=ens&expires_after=…` (30 days) | 20 | 731 ms | 34 ms |
| `POST /v1/lookup` (3 names, 1 address) | | 299 ms | |

## Open questions
- **List routes under load are unmeasured.** The latencies above are single requests. Search, names by address and expiry lists check candidates live, so a page with many stale candidates checks more names than it returns. That needs an end-to-end benchmark: concurrent users, cold and warm state, and candidate sets with many rejections.
- **The read architecture isn't tied to skim.** The same live reads would work against any RPC; what skim changes is the infrastructure needed to run it.

## Compared with bigname on a local node
bigname was also set up locally, with Nethermind for live reads. It didn't index:
- **Intake:** it reads every block from 0, twice each (about 24M `eth_getBlockByNumber` calls on Sepolia). A rate-limited provider returned HTTP 429 on nearly every call.
- **Nethermind:** it had no pre-fork history to offer (`Pruned history unavailable`).

smallname's history is one `eth_getLogs` range scan through skim, from the fork.
