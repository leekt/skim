# Installing skim

skim is a single binary, `skim`. Install a prebuilt release or build it from source.

> **No release has been published yet.** Prebuilt binaries appear once a `v*` tag is pushed (the [release workflow](../../.github/workflows/release.yml) builds and publishes them). Until then, [build from source](#build-from-source).

## Platforms

| Platform | Release target | Notes |
|---|---|---|
| Linux x86_64 | `x86_64-unknown-linux-gnu` | glibc 2.35 or newer (Ubuntu 22.04, Debian 12 and later) |
| Linux aarch64 | `aarch64-unknown-linux-gnu` | glibc 2.35 or newer |
| macOS Apple silicon | `aarch64-apple-darwin` | |
| macOS Intel | `x86_64-apple-darwin` | cross-compiled; less tested |

Other platforms (musl-based Linux such as Alpine, Windows, other architectures) have no prebuilt binary; build from source. The [Dockerfile](../../Dockerfile) builds a Linux image, and tagged releases also publish one at `ghcr.io/leekt/skim`.

## Install script

```sh
curl -fsSL https://raw.githubusercontent.com/leekt/skim/main/install.sh | sh
```

The script detects the OS and architecture, downloads the latest release's tarball and its `.sha256` file from GitHub, verifies the checksum, and copies `skim` to `~/.local/bin`. It doesn't use `sudo`. If the checksum doesn't match, it installs nothing.

| Variable | Default | Meaning |
|---|---|---|
| `SKIM_VERSION` | latest release | a release tag, e.g. `v0.1.0` |
| `SKIM_INSTALL_DIR` | `~/.local/bin` | where `skim` goes |
| `SKIM_BASE_URL` | `https://github.com/leekt/skim/releases/download` | download base; files are fetched from `$SKIM_BASE_URL/<tag>/<file>` |

```sh
curl -fsSL https://raw.githubusercontent.com/leekt/skim/main/install.sh | SKIM_VERSION=v0.1.0 SKIM_INSTALL_DIR=/usr/local/bin sh
```

It needs `curl` or `wget`, `tar`, and `sha256sum` or `shasum`. If the install directory isn't on your `PATH`, the script prints the line to add. To read the script before running it, download it first: `curl -fsSLO https://raw.githubusercontent.com/leekt/skim/main/install.sh`.

## Manual download

Each release has, per target, `skim-<tag>-<target>.tar.gz` and `skim-<tag>-<target>.tar.gz.sha256`. The tarball holds a `skim-<tag>-<target>/` directory with the `skim` binary, `README.md` and `example-config/` (the [Uniswap V3 example](../../examples/uniswap-v3)).

```sh
tag=v0.1.0
target=aarch64-apple-darwin          # see the platform table
base=https://github.com/leekt/skim/releases/download/$tag
curl -fLO "$base/skim-$tag-$target.tar.gz"
curl -fLO "$base/skim-$tag-$target.tar.gz.sha256"

sha256sum -c "skim-$tag-$target.tar.gz.sha256"      # Linux
shasum -a 256 -c "skim-$tag-$target.tar.gz.sha256"  # macOS

tar -xzf "skim-$tag-$target.tar.gz"
mkdir -p ~/.local/bin && install -m 755 "skim-$tag-$target/skim" ~/.local/bin/skim
```

The check must print `OK`. Binaries are not code-signed beyond the linker's ad-hoc signature on macOS. Files downloaded with `curl` aren't quarantined; if you download with a browser, macOS may block the binary until you run `xattr -d com.apple.quarantine skim`.

## Build from source

The toolchain is pinned in [`rust-toolchain.toml`](../../rust-toolchain.toml) (Rust 1.97.0); `rustup` installs it on first build. A C compiler is needed for native dependencies.

```sh
cargo install --git https://github.com/leekt/skim skim-cli --locked
```

This installs `skim` to `~/.cargo/bin`. Or from a checkout:

```sh
git clone https://github.com/leekt/skim && cd skim
cargo build --release --locked -p skim-cli
./target/release/skim --help
```

On Linux the binary uses mimalloc as its allocator; on macOS it uses the system allocator ([memory and latency](memory-and-latency.md)).

## Next steps

```sh
skim        # a Sepolia node with no config: JSON-RPC on 127.0.0.1:8545, state fetched on demand
```

Point a wallet or any Ethereum tool at `http://127.0.0.1:8545`. Nothing needs pinning to start; subscriptions in a `--config` file are a latency optimization for state you read every block.

Continue with [getting started](getting-started.md) for subscriptions and a first call, and [configuration](configuration.md) for flags and `skim.yaml`.

## Publishing a release (maintainers)

Push a tag that starts with `v`:

```sh
git tag v0.1.0 && git push origin v0.1.0
```

The release workflow builds the four targets, then creates the GitHub Release with the tarballs and `.sha256` files attached, and pushes the container image. Tags with a hyphen (`v0.2.0-rc.1`) are published as prereleases, which the install script's default "latest" skips. Running the workflow manually (`workflow_dispatch`) builds the same tarballs as workflow artifacts without publishing anything.
