# smallname

A self-hosted ENS read API for Sepolia, with local contract execution through skim and a small index for discovery and history. It serves the routes an ENS frontend uses, in the shape of [bigname](https://github.com/ensdomains/bigname)'s `/v1` API: about 1,700 lines of JavaScript and one SQLite file.

It's a reference app for skim, not a replacement for a full indexer. bigname covers more: all history, more naming systems, verified reads, and replayable, auditable projections. What smallname shows is a different split. Current values are read from contract state at one block hash per response; an index is kept only to find names and to answer history questions.

```bash
../target/release/skim run --config bigname/skim.yaml          # terminal 1: skim, ENS kept warm
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
- **After that, the index keeps it.** smallname catches up in 2,048-block ranges, then follows the head block by block, by block hash, rolling back on reorgs. Indexed events stay indexed however old they get.

Anything earlier is marked, not guessed:
- **History, events and lists:** `meta.completeness: "partial"` and `history_from_block`, which is where this index starts. `unsupported_reason` is `pre_glamsterdam_history` when the index starts at the fork, or `history_before_index_start` when it starts later.
- **Name detail:** `registered_at` / `created_at` are `null` and listed in `meta.unsupported_fields` when the registration predates the index.
- **`GET /v1/namespaces/ens`:** `name_history` and `subnames` are `partial`.

A name created before the index starts still resolves and has a full profile; the profile is live. It shows up in lists once something about it happens after that.

**Labels:**
- **Unknown labels:** a name whose label text was never seen in an event (`NewOwner` carries only its hash) is listed as `[labelhash].parent.eth`, as bigname does.
- **Wrapped ancestors:** when an ancestor predates the fork, smallname reads its full name from the NameWrapper (`names(node)`, live, checked against the node) and records it.
- **Unwrapped ancestors:** a pre-fork ancestor that isn't wrapped can't be named from state. Such rows are left out, under the same `partial` marker.

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
| `GET /v1/status`, `GET /v1/namespaces/ens` | index position, skim head |

- **Response shapes:** these follow bigname's: `{data, page, meta}`, opaque cursors, Unix-second strings, `{error: {code, message, details}}`.
- **Not implemented** (`422 unsupported`): permissions, resolver and registry views, diagnostics, and Basenames (`*.base.eth`, another chain).
- **Primary names:** answered `verified` only. The `indexed` answer is `unsupported`, because smallname doesn't keep a stored claim.

## Files
| File | |
|---|---|
| `contracts.mjs` | Sepolia ENS contracts and events, taken from bigname's Sepolia manifests |
| `ingest.mjs` | logs → index rows: preimages (hash-checked), name tree, history, candidates |
| `sync.mjs` | catch-up and head following through skim, with reorg rollback |
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
