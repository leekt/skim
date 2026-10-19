# Deploying skim + smallname

A reference setup: one machine runs skim and smallname as services, and only smallname's API and test page are public. skim's RPC listens on localhost.

## Size
Measured on Sepolia, Linux aarch64, `--history local`, with an indexer:

| | Memory |
|---|---|
| skim, following the head | 147 MB at start, 281 MB at 3 h, 346 MB at 9 h, flat after ~7 h (a build that indexed every transaction; a 27 h run on the current `--tx-index pinned` default is pending) |
| skim, catching up the whole post-fork history | earlier builds reached ~500 MB transiently; the current peak has not been re-measured |
| smallname (node) | ~90-160 MB |

A 1 GB machine runs it after the first catch-up but swaps during it. 2 GB or more is comfortable.

These are observations from different runs, not memory ceilings. See [hardware requirements](../../../docs/guide/hardware.md#memory) for conditions and the full curves. The service/config below is for Sepolia ENS contracts; custom networks also need their own app contracts and node `--network` configuration.

**Disk:** skim's `history.redb` holds post-fork headers (about 130 MB) plus the stream matches smallname asked for (small). smallname's index is about 20 MB.

## Steps
1. **Install:**
   - skim: a release binary (`skim-<tag or branch>-<target>.tar.gz` from the release workflow; aarch64 and x86_64 Linux) in `/opt/skim`, with `examples/bigname/skim.yaml` as `/opt/skim/skim.yaml`.
   - smallname: `examples/smallname`, `examples/package.json` and `examples/package-lock.json` in `/opt/smallname`, then `npm ci --omit=dev` there.
2. **Services:** copy [`skim.service`](skim.service) and [`smallname.service`](smallname.service) to `/etc/systemd/system/`, adjust users and paths, then run `systemctl enable --now skim smallname`.
   - Create the service user and writable data directories first (`/var/lib/skim` and `/var/lib/smallname` in these units); smallname needs Node 22.13+ with `node:sqlite`.
   - smallname starts its index at the fork (or as far back as peers still serve logs), catches up through skim's block stream, then follows the head.
   - Check progress at `curl localhost:8787/v1/status`.
3. **Public endpoint:** a tunnel ([`cloudflared.yml`](cloudflared.yml)) or any reverse proxy that forwards only `/`, `/v1/*` and `/healthz` to `127.0.0.1:8787`.
4. **Grafana (optional):** skim runs with `--metrics 127.0.0.1:9465` ([`skim.service`](skim.service)). Copy the repo's [`metrics/`](../../../metrics) stack plus [`grafana/`](grafana) (a compose override and a datasource file) into one directory, put `GRAFANA_ADMIN_PASSWORD` (and `GRAFANA_ROOT_URL`, the public URL) in its `.env`, then run `docker compose up -d` there.
   - The override puts Prometheus (`127.0.0.1:9091`) and Grafana (`127.0.0.1:3001`) on the host network, bound to loopback, so a host firewall that blocks inbound ports needs no change.
   - The tunnel's Grafana hostname forwards to `127.0.0.1:3001`. Anonymous visitors can view the dashboards only; changing anything needs the admin login.

## Updating
- **skim:** swap the binary and restart it.
- **smallname:** restart it after skim. It resumes from the cursor in its database.
- **Moving to a new machine:** copy `smallname.db` along, and the index carries over. skim refetches what it needs.
