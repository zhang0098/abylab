#!/usr/bin/env bash
# Assert a Linux build only asks glibc for symbols at or below the floor we
# promise.
#
# The release binaries are built inside Debian 11 (bullseye) whose glibc is
# 2.31 — see the container pin in .github/workflows/release.yml. Building on
# anything newer silently raises the floor: a binary built on Ubuntu 24.04
# links `__isoc23_*@GLIBC_2.38+`, and the user on an older distro only sees
# `/lib/aarch64-linux-gnu/libc.so.6: version `GLIBC_2.39' not found`.
#
# usage: scripts/check-glibc-floor.sh <binary> [floor]
set -euo pipefail

bin=${1:-}
FLOOR=${2:-2.31}

die() {
    printf 'check-glibc-floor: %s\n' "$1" >&2
    exit 1
}

[ -n "$bin" ] || die "usage: scripts/check-glibc-floor.sh <binary> [floor]"
[ -f "$bin" ] || die "no such file: $bin"

# Every versioned glibc symbol the binary asks for. objdump reads the dynamic
# symbol table (the precise answer); scanning the file is the fallback where
# binutils is missing — the version strings live in .dynstr either way, and
# `sort -V` (GNU) orders them the way a human reads version numbers.
if command -v objdump >/dev/null 2>&1 && objdump -T "$bin" >/dev/null 2>&1; then
    versions=$(objdump -T "$bin" | grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' || true)
else
    versions=$(grep -aoE 'GLIBC_[0-9]+(\.[0-9]+)+' "$bin" || true)
fi
# shellcheck disable=SC2086  # word splitting is how the list flattens back out
versions=$(printf '%s\n' $versions | sed 's/^GLIBC_//' | sort -uV | sed '/^$/d')
[ -n "$versions" ] || die "$bin asks for no versioned glibc symbols — is it a glibc build?"

highest=$(printf '%s\n' "$versions" | tail -n 1)
# max(floor, highest) == floor exactly when highest <= floor.
if [ "$(printf '%s\n%s\n' "$FLOOR" "$highest" | sort -V | tail -n 1)" != "$FLOOR" ]; then
    printf '::error file=%s::%s needs GLIBC_%s, above the %s floor\n' \
        "$bin" "$bin" "$highest" "$FLOOR" >&2
    printf 'requested:%s\n' "$(printf ' %s' $versions)" >&2
    die "build in the older container (see .github/workflows/release.yml)"
fi

printf 'glibc floor ok: %s needs %s, floor %s\n' "$bin" "$highest" "$FLOOR"
printf 'requested:%s\n' "$(printf ' %s' $versions)"
