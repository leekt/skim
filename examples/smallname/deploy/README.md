# Deploying skim + smallname

A reference setup: one machine runs skim and smallname as services, and only smallname's API and test page are public. skim's RPC listens on localhost.

## Size
Measured on Sepolia with the ENS config (`examples/bigname/skim.yaml`), `--history local`:

| | Memory |
|---|---|
| skim, following the head | ~110-125 MB RSS |
| skim, catching up the whole post-fork history | up to ~500 MB while it runs |
| smallname (node) | ~90-160 MB |

A 1 GB machine runs it after the first catch-up but swaps during it. 2 GB or more is comfortable.

**Disk:** skim's `history.redb` holds post-fork headers (about 130 MB) plus the stream matches smallname asked for (small). smallname's index is about 20 MB.

## Steps
1. **Install:**
   - skim: a release binary (`skim-main-<target>.tar.gz` from the release workflow; aarch64 and x86_64 Linux) in `/opt/skim`, with `examples/bigname/skim.yaml` as `/opt/skim/skim.yaml`.
   - smallname: `examples/smallname`, `examples/package.json` and `examples/package-lock.json` in `/opt/smallname`, then `npm ci --omit=dev` there.
2. **Services:** copy [`skim.service`](skim.service) and [`smallname.service`](smallname.service) to `/etc/systemd/system/`, adjust users and paths, then run `systemctl enable --now skim smallname`.
   - smallname starts its index at the fork (or as far back as peers still serve logs), catches up through skim's block stream, then follows the head.
   - Check progress at `curl localhost:8787/v1/status`.
3. **Public endpoint:** a tunnel ([`cloudflared.yml`](cloudflared.yml)) or any reverse proxy that forwards only `/`, `/v1/*` and `/healthz` to `127.0.0.1:8787`.

## Updating
- **skim:** swap the binary and restart it.
- **smallname:** restart it after skim. It resumes from the cursor in its database.
- **Moving to a new machine:** copy `smallname.db` along, and the index carries over. skim refetches what it needs.
