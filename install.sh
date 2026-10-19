#!/bin/sh
# Install the skim binary from a GitHub Release.
#
#   curl -fsSL https://raw.githubusercontent.com/leekt/skim/main/install.sh | sh
#
# Environment:
#   SKIM_VERSION      release tag to install, e.g. v0.1.0 (default: the latest release)
#   SKIM_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
#   SKIM_BASE_URL     download base; files are fetched from $SKIM_BASE_URL/<tag>/<file>
#                     (default: https://github.com/leekt/skim/releases/download)
#
# Supported: Linux x86_64 and aarch64 (glibc), macOS arm64 and x86_64. Anything else:
#   cargo install --git https://github.com/leekt/skim skim-cli --locked

set -eu

REPO="leekt/skim"
SOURCE_HINT="cargo install --git https://github.com/$REPO skim-cli --locked"

say() { printf '%s\n' "$*"; }
err() { printf 'skim install: %s\n' "$*" >&2; exit 1; }

# fetch URL DEST
fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl --proto '=https,file,http' -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -q "$1" -O "$2"
    else
        err "need curl or wget"
    fi
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        err "need sha256sum or shasum to verify the download"
    fi
}

unsupported() {
    err "no prebuilt binary for $1. Build from source instead (Rust toolchain needed):
    $SOURCE_HINT"
}

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os" in
        Linux)
            if ldd --version 2>&1 | grep -qi musl; then
                unsupported "Linux with musl libc"
            fi
            case "$arch" in
                x86_64 | amd64) echo x86_64-unknown-linux-gnu ;;
                aarch64 | arm64) echo aarch64-unknown-linux-gnu ;;
                *) unsupported "Linux $arch" ;;
            esac
            ;;
        Darwin)
            # A shell running under Rosetta reports x86_64 on Apple silicon.
            if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
                arch=arm64
            fi
            case "$arch" in
                arm64 | aarch64) echo aarch64-apple-darwin ;;
                x86_64) echo x86_64-apple-darwin ;;
                *) unsupported "macOS $arch" ;;
            esac
            ;;
        *) unsupported "$os $arch" ;;
    esac
}

latest_version() {
    api="https://api.github.com/repos/$REPO/releases/latest"
    fetch "$api" "$tmp/latest.json" 2>/dev/null ||
        err "could not find the latest release at $api (none published yet, or rate limited). Set SKIM_VERSION=vX.Y.Z, or build from source:
    $SOURCE_HINT"
    tag=$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$tmp/latest.json" | head -n 1)
    [ -n "$tag" ] || err "could not read the latest release tag from $api"
    echo "$tag"
}

main() {
    target=$(detect_target)
    install_dir="${SKIM_INSTALL_DIR:-$HOME/.local/bin}"
    base_url="${SKIM_BASE_URL:-https://github.com/$REPO/releases/download}"

    tmp=$(mktemp -d 2>/dev/null || mktemp -d -t skim)
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 1' HUP INT TERM

    version="${SKIM_VERSION:-}"
    if [ -z "$version" ]; then
        version=$(latest_version)
    fi

    name="skim-$version-$target"
    say "Installing skim $version ($target) to $install_dir"

    fetch "$base_url/$version/$name.tar.gz" "$tmp/$name.tar.gz" ||
        err "download failed: $base_url/$version/$name.tar.gz"
    fetch "$base_url/$version/$name.tar.gz.sha256" "$tmp/$name.tar.gz.sha256" ||
        err "download failed: $base_url/$version/$name.tar.gz.sha256"

    expected=$(cut -d ' ' -f 1 "$tmp/$name.tar.gz.sha256")
    actual=$(sha256_of "$tmp/$name.tar.gz")
    if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
        err "checksum mismatch for $name.tar.gz (expected ${expected:-nothing}, got $actual); not installing"
    fi

    tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
    [ -f "$tmp/$name/skim" ] || err "archive does not contain $name/skim"

    mkdir -p "$install_dir"
    # Copy next to the destination, then rename, so a running skim is not overwritten in place.
    cp "$tmp/$name/skim" "$install_dir/.skim.tmp.$$"
    chmod 755 "$install_dir/.skim.tmp.$$"
    mv -f "$install_dir/.skim.tmp.$$" "$install_dir/skim"

    if ! "$install_dir/skim" --help >/dev/null 2>&1; then
        err "installed $install_dir/skim, but it does not run on this system (on Linux, an older glibc is the usual cause). Build from source instead:
    $SOURCE_HINT"
    fi

    say "Installed $install_dir/skim"
    case ":${PATH:-}:" in
        *":$install_dir:"*) skim_cmd=skim ;;
        *)
            skim_cmd="$install_dir/skim"
            say ""
            say "$install_dir is not on your PATH. Add it, e.g. in ~/.profile or your shell's rc file:"
            say "    export PATH=\"$install_dir:\$PATH\""
            ;;
    esac
    say ""
    say "Next steps:"
    say "    $skim_cmd            # a Sepolia node, no config needed; JSON-RPC on 127.0.0.1:8545"
    say "    $skim_cmd --help"
    say "Docs: https://github.com/$REPO/blob/main/docs/guide/install.md"
}

main "$@"
