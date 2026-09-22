#!/usr/bin/env bash
#
# abylab installer
#
#   /bin/bash -c "$(curl -fsSL https://abylab.ai/install.sh)"
#
# Downloads the prebuilt `abylab` binary for this machine from GitHub Releases,
# verifies its SHA-256 sum and installs that one file. Everything else happens
# inside a temporary directory that is removed on exit.
#
# A plain "latest" install first tries the mirror on https://abylab.ai/downloads
# (GitHub Releases is slow or unreachable on some networks) and falls back to
# GitHub Releases when it is missing or fails.
#
# Options:
#   -v, --version <tag>   release to install (default: latest, e.g. v0.1.0;
#                        only the newest release ships assets)
#       --bin-dir <dir>   where to put the binary (default: ~/.local/bin)
#       --path-file <f>   startup file to put that directory on PATH in
#                         (default: the one your shell actually reads)
#       --no-path         never touch a startup file; only print the line
#       --force           reinstall even when this version is already present
#       --no-verify       skip SHA-256 verification (not recommended)
#       --mirror <url>    mirror base URL to download from first
#       --no-mirror       skip the mirror and use GitHub Releases only
#       --dry-run         print the plan, download nothing
#   -h, --help            this text
#
# Environment: ABYLAB_VERSION, ABYLAB_BIN_DIR, ABYLAB_REPO, ABYLAB_MIRROR
# (ABYLAB_MIRROR='' disables the mirror, like --no-mirror)
#
# Bash 3.2 compatible: no associative arrays, no ${var,,}, no mapfile.
set -euo pipefail

REPO="${ABYLAB_REPO:-zhang0098/abylab}"
BIN_NAME="abylab"
VERSION="${ABYLAB_VERSION:-latest}"
BIN_DIR="${ABYLAB_BIN_DIR:-${HOME:?HOME is not set}/.local/bin}"
# `${VAR-default}`, not `${VAR:-default}`: an explicitly empty ABYLAB_MIRROR is
# how a caller opts out of the mirror.
MIRROR="${ABYLAB_MIRROR-https://abylab.ai/downloads}"
MIRROR_ON=1
MODIFY_PATH=1
PATH_FILE=''
PATH_WROTE=0
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

  /bin/bash -c "$(curl -fsSL https://abylab.ai/install.sh)"

Downloads the prebuilt abylab binary for this machine, verifies its SHA-256 sum
and installs that one file. A plain "latest" install tries the mirror on
abylab.ai first and falls back to GitHub Releases.

Options:
  -v, --version <tag>   release to install (default: latest, e.g. v0.1.0;
                        only the newest release ships assets)
      --bin-dir <dir>   where to put the binary (default: ~/.local/bin)
      --path-file <f>   startup file to put that directory on PATH in
                        (default: the one your shell actually reads)
      --no-path         never touch a startup file; only print the line
      --force           reinstall even when this version is already present
      --no-verify       skip SHA-256 verification (not recommended)
      --mirror <url>    mirror base URL to download from first
      --no-mirror       skip the mirror and use GitHub Releases only
      --dry-run         print the plan, download nothing
  -h, --help            this text

Unless --no-path is given, that directory is also added to your shell startup
file, marked with a comment so the line is easy to find and remove.

Environment: ABYLAB_VERSION, ABYLAB_BIN_DIR, ABYLAB_REPO, ABYLAB_MIRROR
(ABYLAB_MIRROR='' disables the mirror, like --no-mirror)
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
        --path-file)
            [ $# -ge 2 ] || die "--path-file needs a path"
            PATH_FILE="$2"
            shift 2
            ;;
        --no-path)
            MODIFY_PATH=0
            shift
            ;;
        --force)
            FORCE=1
            shift
            ;;
        --no-verify)
            VERIFY=0
            shift
            ;;
        --mirror)
            [ $# -ge 2 ] || die "--mirror needs a base URL, e.g. --mirror https://abylab.ai/downloads"
            # Trailing slash would double up when the path is appended.
            MIRROR="${2%/}"
            MIRROR_ON=1
            shift 2
            ;;
        --no-mirror)
            MIRROR_ON=0
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
            # One Linux asset for every distribution. The binary is linked
            # statically against musl, so there is no host libc to match:
            # glibc 2.x (however old), Alpine, and distroless-style images all
            # install the same file.
            os_part="unknown-linux-musl"
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

# The tag the mirror carries at downloads/latest/, or non-zero when the mirror
# cannot serve this install. Only a plain "latest" can come from the mirror: it
# holds a single version (a Pages deployment replaces the whole site), so an
# explicit --version goes to GitHub Releases — which keeps the tags but only
# the newest release, because release.yml prunes the older ones after every
# publish. A pinned install therefore works for whatever is current.
mirror_tag() {
    local tag
    [ "$MIRROR_ON" = 1 ] || return 1
    [ -n "$MIRROR" ] || return 1
    [ "$VERSION" = "latest" ] || return 1
    tag=$(http_get "$MIRROR/latest/VERSION" 2>/dev/null | tr -d '[:space:]') || return 1
    case "$tag" in
        '') return 1 ;;
        v*) printf '%s\n' "$tag" ;;
        *) printf 'v%s\n' "$tag" ;;
    esac
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

# ----------------------------------------------------------------- PATH ----

# Startup file(s) that put a directory on PATH for the shells this user will
# run abylab from, one absolute path per line. The first entry is created when
# missing; the rest are only touched when they already exist.
rc_files() {
    if [ -n "$PATH_FILE" ]; then
        printf '%s\n' "$PATH_FILE"
        return 0
    fi
    case "${SHELL##*/}" in
        zsh)
            printf '%s\n' "$HOME/.zshrc"
            ;;
        bash)
            printf '%s\n' "$HOME/.bashrc"
            # Login shells read .bash_profile instead. Only touch an existing
            # one: creating it would shadow ~/.profile.
            if [ -f "$HOME/.bash_profile" ]; then
                printf '%s\n' "$HOME/.bash_profile"
            fi
            ;;
        fish)
            printf '%s\n' "$HOME/.config/fish/config.fish"
            ;;
        *)
            printf '%s\n' "$HOME/.profile"
            ;;
    esac
}

# fish spells this differently. The login shell decides, except when
# --path-file points at a fish config.
fish_syntax() { # file
    case "${SHELL##*/}" in
        fish) return 0 ;;
    esac
    case "$1" in
        *.fish) return 0 ;;
    esac
    return 1
}

path_line() { # dir file
    if fish_syntax "$2"; then
        printf 'fish_add_path "%s"' "$1"
    else
        # shellcheck disable=SC2016  # $PATH is literal: it is written to the file
        printf 'export PATH="%s:$PATH"' "$1"
    fi
}

# Append the PATH line to one startup file unless the directory is mentioned
# there already — re-running the installer, or a hand-written line, must not
# stack duplicates. Sets PATH_WROTE when it actually adds something.
add_path_to() { # file dir
    local file="$1" dir="$2" parent
    if [ -f "$file" ] && grep -qF -- "$dir" "$file" 2>/dev/null; then
        note "PATH     $dir already in $file"
        return 0
    fi
    parent="${file%/*}"
    [ "$parent" = "$file" ] || mkdir -p "$parent" 2>/dev/null || true
    if ! printf '\n# added by the abylab installer\n%s\n' "$(path_line "$dir" "$file")" >>"$file" 2>/dev/null; then
        warn "could not write $file — add this line yourself:
       $(path_line "$dir" "$file")"
        return 1
    fi
    note "PATH     $dir added to $file"
    PATH_WROTE=1
}

# Put BIN_DIR on PATH for future shells. Never fatal: a failed edit only
# downgrades to printing the line.
ensure_path() { # dir
    local file
    if [ "$(id -u)" = 0 ]; then
        warn "running as root — leaving shell startup files alone; add $1 to PATH yourself"
        return 0
    fi
    while IFS= read -r file; do
        [ -n "$file" ] || continue
        add_path_to "$file" "$1" || true
    done <<EOF
$(rc_files)
EOF
    if [ "$PATH_WROTE" = 1 ]; then
        printf 'Restart your shell to pick up the new PATH.\n'
    fi
    return 0
}

# Report (and unless --no-path, arrange) how BIN_DIR reaches PATH. $1 is the
# dry-run flag, so nothing is written during a dry run.
wire_path() { # dry_run
    case ":${PATH}:" in
        *":$BIN_DIR:"*)
            note "PATH     $BIN_DIR is already on \$PATH"
            return 0
            ;;
    esac
    if [ "$MODIFY_PATH" = 0 ]; then
        printf '\n%s%s is not on your PATH.%s Add it:\n' "$BOLD" "$BIN_DIR" "$OFF"
        # shellcheck disable=SC2016  # $PATH is literal: the user pastes this line
        printf '\n    export PATH="%s:$PATH"\n' "$BIN_DIR"
        return 0
    fi
    if [ "$1" = 1 ]; then
        note "PATH     would add $BIN_DIR to $(rc_files | tr '\n' ' ')"
        return 0
    fi
    ensure_path "$BIN_DIR"
}

# ---------------------------------------------------------------- main -----

main() {
    local target tag asset base tmp tarball sums bin current installed dl_error mirrored

    target=$(detect_target)

    # The mirror carries only the latest release; everything else — an explicit
    # --version, --no-mirror, a mirror that is down or does not have the file —
    # is served by GitHub Releases, which keeps every tag.
    tag=''
    mirrored=0
    if tag=$(mirror_tag); then
        mirrored=1
        asset="${BIN_NAME}-latest-${target}.tar.gz"
        base="$MIRROR/latest"
    else
        tag=$(resolve_tag) || die "no release found for $REPO
       Cut one first (see .github/workflows/release.yml) or build from source:
       cargo install --git https://github.com/${REPO} abylab-tui"
        asset="${BIN_NAME}-${tag}-${target}.tar.gz"
        base="https://github.com/${REPO}/releases/download/${tag}"
    fi

    printf '%sabylab%s %s (%s)\n' "$BOLD" "$OFF" "$tag" "$target"
    note "asset    $asset"
    note "source   $base"
    note "target   $BIN_DIR/$BIN_NAME"

    if [ "$FORCE" = 0 ] && [ -x "$BIN_DIR/$BIN_NAME" ]; then
        current=$("$BIN_DIR/$BIN_NAME" --version 2>/dev/null || true)
        if [ "${current##* }" = "${tag#v}" ]; then
            note "already up to date — use --force to reinstall"
            wire_path "$DRY_RUN"
            return 0
        fi
    fi

    if [ "$DRY_RUN" = 1 ]; then
        note "would download $base/$asset"
        [ "$VERIFY" = 1 ] && note "would verify it against $base/SHA256SUMS"
        wire_path 1
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
        # A mirror that is down or has dropped the file must not cost the user
        # the install: GitHub Releases is the source of record.
        if [ "$mirrored" = 1 ]; then
            note "mirror download failed — trying GitHub Releases"
            rm -f "$tarball"
            tag=$(resolve_tag) || die "mirror download failed: $MIRROR/latest/$asset
       ${dl_error:-unknown download error}"
            asset="${BIN_NAME}-${tag}-${target}.tar.gz"
            base="https://github.com/${REPO}/releases/download/${tag}"
            tarball="$tmp/$asset"
            dl_error=$(http_download "$base/$asset" "$tarball" 2>&1) || true
        fi
        [ -s "$tarball" ] || die "download failed: $base/$asset
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

    # The binary is in place — but "in place" is not "runnable": a truncated
    # download, an asset for the wrong architecture or an unsupported (kernel,
    # vDSO, seccomp) host would otherwise scroll by under a cheerful "installed
    # abylab". Say what happened and where to go instead, and exit non-zero.
    # The Linux builds are static, so a missing libc is no longer on this list.
    if ! installed=$("$BIN_DIR/$BIN_NAME" --version 2>&1); then
        warn "$BIN_DIR/$BIN_NAME is installed but will not start on this system:"
        printf '       %s\n' "$installed"
        note "       build from source instead: cargo install --git https://github.com/${REPO} abylab-tui"
        return 1
    fi
    printf '%sinstalled%s %s\n' "$GREEN" "$OFF" "$installed"

    wire_path 0

    # abylab never reads the environment for credentials: the key is typed into
    # the app (/login) and lands in $ABYLAB_HOME/.credentials.yaml.
    printf '\nNext: run %s%s%s, then enter %s/login sk-…%s to store your DeepSeek API key\n' \
        "$BOLD" "$BIN_NAME" "$OFF" "$BOLD" "$OFF"
}

main
