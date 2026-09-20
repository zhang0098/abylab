#!/usr/bin/env bash
#
# Stage the directory Cloudflare Pages should publish.
#
#   scripts/stage-site.sh <dest> [tag]
#
# Without a tag it stages `site/` alone. With one it also adds that release's
# tarballs under `downloads/latest/`, using the stable names plus VERSION and
# SHA256SUMS that install.sh asks the mirror for.
#
# Both deployers call this — site.yml for site pushes, release.yml's mirror job
# after a release — because a Pages deployment is a full snapshot of the
# uploaded directory: whichever one runs has to carry the other one's half, or
# it deletes that half from the live site. (Before this existed, site.yml
# deployed `site` on its own and would have dropped /downloads/*.)
#
# Needs `gh` on PATH with GH_TOKEN/GH_REPO set when a tag is given; fetches
# nothing for the site-only mode.
set -euo pipefail

DEST=${1:?usage: stage-site.sh <dest> [tag]}
TAG=${2:-}

if [ ! -d site ]; then
    echo "stage-site.sh: no site/ directory here (run from the repo root)" >&2
    exit 1
fi

mkdir -p "$DEST"
cp -R site/. "$DEST"/

if [ -z "$TAG" ]; then
    echo "staged site/ only, no downloads (no tag given)"
    exit 0
fi

tmp=$(mktemp -d)
# shellcheck disable=SC2064  # $tmp is expanded now on purpose
trap "rm -rf '$tmp'" EXIT

# Fails loudly: silently deploying without the tarballs would delete the mirror
# that install.sh falls back to on networks where GitHub is unreachable.
gh release download "$TAG" --pattern '*.tar.gz' -D "$tmp"

mkdir -p "$DEST/downloads/latest"
printf '%s\n' "$TAG" >"$DEST/downloads/latest/VERSION"

for src in "$tmp"/*.tar.gz; do
    name=${src##*/}
    # abylab-v0.1.2-x86_64-unknown-linux-musl.tar.gz becomes
    # abylab-latest-x86_64-unknown-linux-musl.tar.gz — the name install.sh
    # asks the mirror for, so it survives the next release.
    stable=${name/abylab-${TAG}-/abylab-latest-}
    cp "$src" "$DEST/downloads/latest/$stable"
done

# Sums over the mirrored names (the release's own SHA256SUMS lists the
# versioned ones), so `sha256sum -c` works against this directory.
(
    cd "$DEST/downloads/latest"
    sha256sum -- *.tar.gz >SHA256SUMS
)

echo "staged site/ + downloads/latest/ from $TAG:"
ls -1 "$DEST/downloads/latest"
