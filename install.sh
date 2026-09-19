#!/usr/bin/env bash
#
# abylab installer
#
#   /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/zhang0098/abylab/main/install.sh)"
#
# Downloads the prebuilt `abylab` binary for this machine from GitHub Releases,
# verifies its SHA-256 sum and installs that one file. Everything else happens
# inside a temporary directory that is removed on exit.
#
# Options:
#   -v, --version <tag>   release to install (default: latest, e.g. v0.1.0)
#       --bin-dir <dir>   where to put the binary (default: ~/.local/bin)
#       --force           reinstall even when this version is already present
#       --no-verify       skip SHA-256 verification (not recommended)
#       --dry-run         print the plan, download nothing
#   -h, --help            this text
#
# Environment: ABYLAB_VERSION, ABYLAB_BIN_DIR, ABYLAB_REPO
#
# Bash 3.2 compatible: no associative arrays, no ${var,,}, no mapfile.
set -euo pipefail

REPO="${ABYLAB_REPO:-zhang0098/abylab}"
BIN_NAME="abylab"
VERSION="${ABYLAB_VERSION:-latest}"
BIN_DIR="${ABYLAB_BIN_DIR:-${HOME:?HOME is not set}/.local/bin}"
FORCE=0
VERIFY=1
DRY_RUN=0

# ---------------------------------------------------------------- output ----

if [ -t 2 ] && [ -z "${NO_COLOR:-}" ]; then
    BOLD=$(printf '\033[1m')
    DIM=$(printf '\033[2m')
    RED=$(printf '\033[31m')
    GREEN=$(printf '\033[32m')
    OFF=$(printf '\033[0m')
else
    BOLD='' DIM='' RED='' GREEN='' OFF=''
fi

die() {
    printf '%sinstall.sh: error:%s %s\n' "$RED" "$OFF" "$1" >&2
    exit 1
}

warn() { printf '%sinstall.sh: warning:%s %s\n' "$DIM" "$OFF" "$1" >&2; }

note() { printf '%s->%s %s\n' "$DIM" "$OFF" "$1"; }

have() { command -v "$1" >/dev/null 2>&1 || return 1; }

# The text is embedded rather than sliced out of $0: the documented one-liner
# runs the script through `bash -c`, where $0 is "bash" and no file exists.
usage() {
    cat <<'EOF'
abylab installer

  /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/zhang0098/abylab/main/install.sh)"

Downloads the prebuilt abylab binary for this machine from GitHub Releases,
verifies its SHA-256 sum and installs that one file.

Options:
  -v, --version <tag>   release to install (default: latest, e.g. v0.1.0)
      --bin-dir <dir>   where to put the binary (default: ~/.local/bin)
      --force           reinstall even when this version is already present
      --no-verify       skip SHA-256 verification (not recommended)
      --dry-run         print the plan, download nothing
  -h, --help            this text

Environment: ABYLAB_VERSION, ABYLAB_BIN_DIR, ABYLAB_REPO
EOF
}

# ------------------------------------------------------------ arguments ----

while [ $# -gt 0 ]; do
    case "$1" in
        -v | --version)
            [ $# -ge 2 ] || die "--version needs a value, e.g. --version v0.1.0"
            VERSION="$2"
            shift 2
            ;;
        --bin-dir)
            [ $# -ge 2 ] || die "--bin-dir needs a path"
            BIN_DIR="$2"
            shift 2
            ;;
        --force)
            FORCE=1
            shift
            ;;
        --no-verify)
            VERIFY=0
            shift
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done

case "$BIN_DIR" in
    /*) ;;
    *) die "--bin-dir must be an absolute path (got '$BIN_DIR')" ;;
esac

# ------------------------------------------------------------- download ----

CURL=''
WGET=''
if have curl; then
    CURL=curl
elif have wget; then
    WGET=wget
else
    die "curl or wget is required"
fi

# Fetch a URL to stdout.
http_get() {
    if [ -n "$CURL" ]; then
        "$CURL" -fsSL "$1"
    else
        "$WGET" -qO- "$1"
    fi
}

# Fetch a URL to a file.
http_download() {
    if [ -n "$CURL" ]; then
        "$CURL" -fsSL --retry 3 --retry-delay 1 -o "$2" "$1"
    else
        "$WGET" -q --tries=3 -O "$2" "$1"
    fi
}

# ------------------------------------------------------------- platform ----

# Map uname to a Rust target triple. The release workflow publishes exactly
# these four targets.
detect_target() {
    local os arch os_part arch_part
    os=$(uname -s)
    arch=$(uname -m)
    case "$os" in
        Linux)
            # The Linux binaries link glibc; Alpine-style musl systems cannot
            # run them at all, so fail early with the source-build escape hatch.
            if ldd --version 2>&1 | grep -qi musl; then
                die "musl libc detected — the prebuilt Linux binaries need glibc.
       Build from source instead: cargo install --git https://github.com/${REPO} abylab-tui"
            fi
            os_part="unknown-linux-gnu"
            ;;
        Darwin) os_part="apple-darwin" ;;
        *) die "unsupported OS '$os' — prebuilt binaries cover Linux and macOS" ;;
    esac
    case "$arch" in
        x86_64 | amd64) arch_part="x86_64" ;;
        aarch64 | arm64) arch_part="aarch64" ;;
        *) die "unsupported architecture '$arch' — prebuilt binaries cover x86_64 and aarch64" ;;
    esac
    printf '%s-%s\n' "$arch_part" "$os_part"
}

# -------------------------------------------------------------- version ----

# Resolve the release tag to install. `releases/latest` redirects to the tag
# page, which avoids the unauthenticated API rate limit; the API is the
# fallback (and the only route for wget, which cannot read the redirect).
resolve_tag() {
    local url json tag
    if [ "$VERSION" != "latest" ]; then
        case "$VERSION" in
            v*) printf '%s\n' "$VERSION" ;;
            *) printf 'v%s\n' "$VERSION" ;;
        esac
        return 0
    fi
    if [ -n "$CURL" ]; then
        url=$("$CURL" -fsSLI -o /dev/null -w '%{url_effective}' \
            "https://github.com/${REPO}/releases/latest" 2>/dev/null || true)
        case "$url" in
            */releases/tag/*)
                printf '%s\n' "${url##*/releases/tag/}"
                return 0
                ;;
        esac
    fi
    json=$(http_get "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null) || return 1
    tag=$(printf '%s\n' "$json" |
        sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
    [ -n "$tag" ] || return 1
    printf '%s\n' "$tag"
}

# ------------------------------------------------------------- checksum ----

# Compare the tarball against its SHA256SUMS entry. A missing entry or missing
# hashing tool only warns: the sums file itself is fetched over TLS from the
# same release, so it is a integrity check, not a signature.
verify_sha256() {
    local file="$1" sums="$2" name="$3" expected actual
    # sha256sum writes "hash  name", but "hash  ./name" and the binary-mode
    # "hash *name" both occur; awk normalises all three (and, unlike sed BRE,
    # needs no GNU-only alternation).
    expected=$(awk -v want="$name" '
        {
            got = $2
            sub(/^\*/, "", got)
            sub(/^\.\//, "", got)
            if (got == want) { print $1; exit }
        }
    ' "$sums")
    if [ -z "$expected" ]; then
        warn "no SHA-256 entry for $name in SHA256SUMS"
        return 0
    fi
    if have sha256sum; then
        actual=$(sha256sum "$file" | cut -d' ' -f1)
    elif have shasum; then
        actual=$(shasum -a 256 "$file" | cut -d' ' -f1)
    else
        warn "neither sha256sum nor shasum is available — skipping verification"
        return 0
    fi
    if [ "$actual" != "$expected" ]; then
        die "checksum mismatch for $name
       expected $expected
       got      $actual"
    fi
    note "sha256 verified"
}

# -------------------------------------------------------------- install ----

install_binary() {
    local src="$1" dst="$BIN_DIR/$BIN_NAME"
    if [ ! -d "$BIN_DIR" ]; then
        mkdir -p "$BIN_DIR" || die "cannot create $BIN_DIR"
    fi
    if [ ! -w "$BIN_DIR" ]; then
        die "$BIN_DIR is not writable.
       Pick a directory you own (--bin-dir ~/bin) or install by hand:
       sudo install -m 0755 $src $dst"
    fi
    install -m 0755 "$src" "$dst" || die "could not write $dst"
}

# ---------------------------------------------------------------- main -----

main() {
    local target tag asset base tmp tarball sums bin current installed

    target=$(detect_target)
    tag=$(resolve_tag) || die "no release found for $REPO
       Cut one first (see .github/workflows/release.yml) or build from source:
       cargo install --git https://github.com/${REPO} abylab-tui"

    asset="${BIN_NAME}-${tag}-${target}.tar.gz"
    base="https://github.com/${REPO}/releases/download/${tag}"

    printf '%sabylab%s %s (%s)\n' "$BOLD" "$OFF" "$tag" "$target"
    note "asset    $asset"
    note "target   $BIN_DIR/$BIN_NAME"

    if [ "$FORCE" = 0 ] && [ -x "$BIN_DIR/$BIN_NAME" ]; then
        current=$("$BIN_DIR/$BIN_NAME" --version 2>/dev/null || true)
        if [ "${current##* }" = "${tag#v}" ]; then
            note "already up to date — use --force to reinstall"
            return 0
        fi
    fi

    if [ "$DRY_RUN" = 1 ]; then
        note "would download $base/$asset"
        [ "$VERIFY" = 1 ] && note "would verify it against $base/SHA256SUMS"
        note "dry run — nothing downloaded"
        return 0
    fi

    tmp=$(mktemp -d) || die "cannot create a temporary directory"
    # shellcheck disable=SC2064  # $tmp is expanded now on purpose
    trap "rm -rf '$tmp'" EXIT

    tarball="$tmp/$asset"
    note "downloading…"
    # Keep the downloader's own message (DNS failure, 404, …) and fold it into
    # our error instead of letting a bare "curl: (22)" escape.
    if ! dl_error=$(http_download "$base/$asset" "$tarball" 2>&1); then
        die "download failed: $base/$asset
       ${dl_error:-unknown download error}
       Check that '$tag' has a build for $target:
       https://github.com/${REPO}/releases/tag/${tag}"
    fi

    if [ "$VERIFY" = 1 ]; then
        sums="$tmp/SHA256SUMS"
        if http_download "$base/SHA256SUMS" "$sums" 2>/dev/null; then
            verify_sha256 "$tarball" "$sums" "$asset"
        else
            warn "no SHA256SUMS published for $tag — skipping verification"
        fi
    fi

    if ! tar_error=$(tar -xzf "$tarball" -C "$tmp" 2>&1); then
        die "could not unpack $asset
       ${tar_error:-unknown tar error}"
    fi
    bin=$(find "$tmp" -type f -name "$BIN_NAME" | head -n 1)
    [ -n "$bin" ] || die "no $BIN_NAME binary inside $asset"

    install_binary "$bin"

    installed=$("$BIN_DIR/$BIN_NAME" --version 2>/dev/null || printf '%s' "$BIN_NAME")
    printf '%sinstalled%s %s\n' "$GREEN" "$OFF" "$installed"

    case ":${PATH}:" in
        *":$BIN_DIR:"*) ;;
        *)
            printf '\n%s%s is not on your PATH yet.%s Add it (bash/zsh):\n' \
                "$BOLD" "$BIN_DIR" "$OFF"
            # shellcheck disable=SC2016  # $PATH is literal: the user pastes this line
            printf '\n    export PATH="%s:$PATH"\n' "$BIN_DIR"
            ;;
    esac

    printf '\nNext: export DEEPSEEK_API_KEY=sk-… then run %s%s%s\n' "$BOLD" "$BIN_NAME" "$OFF"
}

main
