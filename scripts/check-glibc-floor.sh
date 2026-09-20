#!/usr/bin/env bash
# Assert what a Linux build promises about the C library it needs.
#
# One script, two modes, because in both cases the point is the same: the
# binary must start on the oldest host we claim to support.
#
#   scripts/check-glibc-floor.sh <binary> [floor]
#       glibc build: every versioned GLIBC_ symbol it asks for must be at or
#       below the floor (default 2.31). Building on anything newer silently
#       raises it — a binary built on Ubuntu 24.04 links
#       `__isoc23_*@GLIBC_2.38+`, and the user on an older distro only sees
#       `/lib/aarch64-linux-gnu/libc.so.6: version `GLIBC_2.39' not found`.
#
#   scripts/check-glibc-floor.sh --static <binary>
#       musl build: the binary must be a static ELF — no dynamic section, no
#       PT_INTERP, no GLIBC_ symbol. That is what lets the one Linux asset run
#       on glibc and musl hosts alike. Linux releases are built this way (see
#       the Alpine container in .github/workflows/release.yml).
#
# usage: scripts/check-glibc-floor.sh [--static] <binary> [floor]
set -euo pipefail

MODE=glibc
if [ "${1:-}" = "--static" ]; then
    MODE=static
    shift
fi

bin=${1:-}
FLOOR=${2:-2.31}

die() {
    printf 'check-glibc-floor: %s\n' "$1" >&2
    exit 1
}

# A failure that is about the binary we were handed, not about the invocation:
# the ::error annotation is what GitHub prints next to the file.
fail() {
    printf '::error file=%s::%s: %s\n' "$bin" "$bin" "$1" >&2
    die "$1"
}

[ -n "$bin" ] || die "usage: scripts/check-glibc-floor.sh [--static] <binary> [floor]"
[ -f "$bin" ] || die "no such file: $bin"

# ELF magic first: pointing this at a Mach-O build should fail loudly instead
# of proving something about the wrong file.
if [ "$(head -c 4 "$bin" | od -An -tx1 | tr -d ' \n')" != "7f454c46" ]; then
    fail "is not an ELF binary"
fi

# Every versioned glibc symbol the binary asks for. objdump reads the dynamic
# symbol table (the precise answer); scanning the file is the fallback where
# binutils is missing — the version strings live in .dynstr either way, and
# `sort -V` (GNU) orders them the way a human reads version numbers.
glibc_symbols() {
    if command -v objdump >/dev/null 2>&1 && objdump -T "$bin" >/dev/null 2>&1; then
        objdump -T "$bin" | grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' || true
    else
        grep -aoE 'GLIBC_[0-9]+(\.[0-9]+)+' "$bin" || true
    fi
}

if [ "$MODE" = static ]; then
    # readelf sees the section headers and the program headers, which is where
    # "static" is decided; objdump -p is the fallback for a binutils without
    # readelf. One of the two has to be there: without them a passing check
    # would mean nothing.
    if command -v readelf >/dev/null 2>&1; then
        if readelf -d "$bin" 2>/dev/null | grep -q 'Dynamic section'; then
            fail "has a dynamic section — this is not a static build"
        fi
        if readelf -l "$bin" 2>/dev/null | grep -q 'Requesting program interpreter'; then
            fail "has a PT_INTERP header — the runtime loader would have to exist on the host"
        fi
    elif command -v objdump >/dev/null 2>&1; then
        if objdump -p "$bin" 2>/dev/null | grep -q 'NEEDED'; then
            fail "links a shared library (NEEDED entry) — this is not a static build"
        fi
    else
        die "neither readelf nor objdump is installed — cannot tell a static build from a dynamic one"
    fi

    versions=$(glibc_symbols)
    if [ -n "$versions" ]; then
        printf 'requested:%s\n' "$(printf ' %s' $versions)" >&2
        fail "asks for GLIBC_ symbols — this is not the musl build"
    fi

    printf 'static link ok: %s has no dynamic section and no GLIBC_ symbols\n' "$bin"
    exit 0
fi

versions=$(glibc_symbols)
# shellcheck disable=SC2086  # word splitting is how the list flattens back out
versions=$(printf '%s\n' $versions | sed 's/^GLIBC_//' | sort -uV | sed '/^$/d')
[ -n "$versions" ] || die "$bin asks for no versioned glibc symbols — is it a glibc build?"

highest=$(printf '%s\n' "$versions" | tail -n 1)
# max(floor, highest) == floor exactly when highest <= floor.
if [ "$(printf '%s\n%s\n' "$FLOOR" "$highest" | sort -V | tail -n 1)" != "$FLOOR" ]; then
    printf 'requested:%s\n' "$(printf ' %s' $versions)" >&2
    fail "needs GLIBC_$highest, above the $FLOOR floor — build in the older container (see .github/workflows/release.yml)"
fi

printf 'glibc floor ok: %s needs %s, floor %s\n' "$bin" "$highest" "$FLOOR"
printf 'requested:%s\n' "$(printf ' %s' $versions)"
