#!/usr/bin/env bash
# Cut a release: bump, changelog, commit, tag, push.
#
#   scripts/release.sh 0.1.11 [--dry-run]
#
# What it does, in the order the release workflow expects:
#
#   1. preflight — you are on an up-to-date main, the tag is free, the three
#      crates agree on the previous release, and `[Unreleased]` has entries.
#   2. bump — the three crate versions, Cargo.lock, and both changelogs
#      (`[Unreleased]` becomes `[VERSION] - today`, plus the compare links).
#   3. verify — `cargo metadata --locked` (the lock agrees with the manifests)
#      and `abylab --version` (the string release.yml's tag gate compares).
#   4. commit `Release <version>`, whose body inlines the English changelog
#      section this version just closed.
#   5. publish — push main with the release deploy key, then tag the merged
#      commit. Fall back to a `release/<version>` branch plus a PR (waiting for
#      its checks) when the key is missing or cannot push. Either way the
#      annotated tag `v<version>` is what starts the release workflow, so
#      nothing ships before this step.
#
# The split this script assumes: main's ruleset requires a pull request and the
# five CI checks, and its only bypass actor is the repository's deploy key —
# not a human. So ordinary work goes through a PR and cannot skip CI, while a
# release (step 5) pushes straight to main with the key. Registering that key
# once, per clone or machine:
#
#   ssh-keygen -t ed25519 -N "" -C abylab-release -f ~/.ssh/abylab-release
#   gh api -X POST repos/<owner>/<repo>/keys \
#       -f title="abylab release (ruleset bypass)" \
#       -f key="$(cat ~/.ssh/abylab-release.pub)" -F read_only=false
#
# Override the path with ABYLAB_RELEASE_KEY. The tag push itself is not
# protected, so it uses the ordinary remote.
#
# usage: scripts/release.sh <version> [--dry-run|--no-publish]
set -euo pipefail

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

VERSION="${1-}"
DRY_RUN=0
NO_PUBLISH=0
for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=1 ;;
        --no-publish) NO_PUBLISH=1 ;;
        --help|-h) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    esac
done

[[ "$VERSION" =~ ^v?[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "usage: scripts/release.sh <version> [--dry-run|--no-publish] (e.g. 0.1.11)"
VERSION="${VERSION#v}"
TAG="v$VERSION"

cd "$(git rev-parse --show-toplevel)"
say "== release $TAG"

# ------------------------------------------------------------- preflight ---

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
[ "$BRANCH" = "main" ] || die "release from main, not $BRANCH"
git diff-index --quiet HEAD -- || die "uncommitted tracked changes; commit or stash them first"
git fetch origin --tags --quiet
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || die "local main is not origin/main; run git pull first"

git rev-parse -q --verify "refs/tags/$TAG" >/dev/null && die "tag $TAG already exists locally"
[ -z "$(git ls-remote --tags origin "refs/tags/$TAG")" ] || die "tag $TAG already exists on origin"

TUI_TOML=crates/abylab-tui/Cargo.toml
OLD="$(grep -m1 '^version = ' "$TUI_TOML" | cut -d'"' -f2)"
LATEST="$(git tag --list 'v*' --sort=-v:refname | head -n1)"
say "   previous release: ${LATEST:-none}; crates are at $OLD"
[ "$OLD" = "${LATEST#v}" ] || die "crates are at $OLD but the newest tag is ${LATEST:-none}; resync before releasing"
[ "$VERSION" != "$OLD" ] || die "$VERSION is already the current version"

for file in CHANGELOG.md CHANGELOG.en.md; do
    awk -v file="$file" '
        /^## \[Unreleased\]$/ { in_section = 1; next }
        /^## \[/ { in_section = 0 }
        in_section && /^[-*] / { found = 1 }
        END { exit (found ? 0 : 1) }
    ' "$file" || die "$file has nothing under [Unreleased] to release"
done

if [ "$DRY_RUN" = 1 ]; then
    say "   would bump $OLD -> $VERSION, close [Unreleased] as [$VERSION] - $(date +%F),"
    say "   commit, then push and tag $TAG"
    exit 0
fi

# ----------------------------------------------------------------- bump ---

for toml in crates/abycore/Cargo.toml crates/abylab-backend/Cargo.toml "$TUI_TOML"; do
    [ "$(grep -c "^version = \"$OLD\"$" "$toml")" = 1 ] || die "$toml does not hold exactly one version = \"$OLD\""
    sed "s/^version = \"$OLD\"$/version = \"$VERSION\"/" "$toml" > "$toml.tmp"
    mv "$toml.tmp" "$toml"
done

awk -v ver="$VERSION" '
    /^\[\[package\]\]$/ { package = "" }
    /^name = "(abycore|abylab-backend|abylab-tui)"$/ { package = $0 }
    package != "" && /^version = / { print "version = \"" ver "\""; package = ""; next }
    { print }
' Cargo.lock > Cargo.lock.tmp
mv Cargo.lock.tmp Cargo.lock
# The three modules above are not enough to prove the lock is right: cargo is.
cargo metadata --locked --format-version 1 --no-deps >/dev/null ||
    die "Cargo.lock disagrees with the bumped manifests"

TODAY="$(date +%F)"
for file in CHANGELOG.md CHANGELOG.en.md; do
    old_link="[Unreleased]: https://github.com/zhang0098/abylab/compare/v$OLD...HEAD"
    [ "$(grep -cF "$old_link" "$file")" = 1 ] || die "$file does not carry the v$OLD compare link"
    sed -e "s|^## \[Unreleased\]\$|## [Unreleased]\n\n## [$VERSION] - $TODAY|" \
        -e "s|^$(printf '%s' "$old_link" | sed 's/[][\.*^$/]/\\&/g')$|[Unreleased]: https://github.com/zhang0098/abylab/compare/v$VERSION...HEAD\n[$VERSION]: https://github.com/zhang0098/abylab/compare/v$OLD...v$VERSION|" \
        "$file" > "$file.tmp"
    mv "$file.tmp" "$file"
done

# --------------------------------------------------------------- verify ---

say "   crates now at $(grep -m1 '^version = ' "$TUI_TOML" | cut -d'"' -f2)"
BUILT="$(cargo run -q --locked -p abylab-tui -- --version)" || die "abylab --version failed"
say "   $BUILT"
[ "$BUILT" = "abylab $VERSION" ] || die "abylab --version printed \"$BUILT\", expected \"abylab $VERSION\""

# --------------------------------------------------------------- commit ---

notes="$(awk -v ver="$VERSION" '
    $0 == "## [" ver "]" || $0 ~ "^## \\[" ver "\\] " { in_section = 1; next }
    in_section && /^## \[/ { exit }
    in_section && /^### / { next }
    in_section { print }
' CHANGELOG.en.md)"
[ -n "$notes" ] || die "could not read the [$VERSION] section out of CHANGELOG.en.md"

git add "$TUI_TOML" crates/abycore/Cargo.toml crates/abylab-backend/Cargo.toml Cargo.lock CHANGELOG.md CHANGELOG.en.md
git commit -q -F - <<MSG
Release $VERSION

Bump the three crates in lockstep so the compiled-in version matches the tag.
release.yml's \`check\` gate enforces the same invariant, since install.sh
compares \`abylab --version\` against the release tag to decide whether an
upgrade is needed.

What this release carries since v$OLD:

$notes
MSG
say "   committed $(git log --oneline -1)"

# -------------------------------------------------------------- publish ---

if [ "$NO_PUBLISH" = 1 ]; then
    say "   --no-publish: stopping before the push (the commit and $TAG are yours to inspect)"
    say "   finish with: GIT_SSH_COMMAND=\"ssh -i ${ABYLAB_RELEASE_KEY:-$HOME/.ssh/abylab-release} -o IdentitiesOnly=yes\" git push git@github.com:<owner>/<repo>.git HEAD:main"
    say "   then: git tag -a $TAG -m $TAG && git push origin $TAG"
    exit 0
fi

# The release deploy key is the ruleset's only bypass actor, so this push is
# the one write to main that does not need a pull request. Derive the SSH URL
# from the ordinary remote: the key works over git@github.com regardless of
# whether the clone itself is HTTPS.
RELEASE_KEY="${ABYLAB_RELEASE_KEY:-$HOME/.ssh/abylab-release}"
REMOTE_URL="$(git remote get-url origin)"
case "$REMOTE_URL" in
    https://github.com/*) SLUG="${REMOTE_URL#https://github.com/}" ;;
    git@github.com:*) SLUG="${REMOTE_URL#git@github.com:}" ;;
    *) SLUG="" ;;
esac
SLUG="${SLUG%.git}"

pushed=0
TAGGED=""
if [ -n "$SLUG" ] && [ -f "$RELEASE_KEY" ]; then
    if GIT_SSH_COMMAND="ssh -i $RELEASE_KEY -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=accept-new" \
        git push "git@github.com:$SLUG.git" HEAD:main; then
        pushed=1
        say "   pushed to main with $RELEASE_KEY (ruleset bypass)"
        # That push named an explicit URL, so the clone's remote-tracking ref
        # still points at the previous main: refresh it before anything reads
        # `origin/main`, and tag the commit we just pushed. Tagging the stale
        # ref put v0.1.11 on the pre-release commit and the workflow's version
        # gate failed ten seconds later with nothing built.
        git fetch origin main --quiet
        TAGGED="$(git rev-parse HEAD)"
    else
        say "   the release key could not push to main"
    fi
else
    say "   no release key at $RELEASE_KEY (see the header to register one)"
fi

if [ "$pushed" = 0 ]; then
    say "   going through a pull request instead"
    git switch -c "release/$VERSION"
    git push -u origin "release/$VERSION"
    pr="$(gh pr create --base main --head "release/$VERSION" --title "Release $VERSION" \
        --body "Version bump in lockstep (three crates + lockfile), the \`[Unreleased]\` entries closed as \`[$VERSION] - $TODAY\` in both changelogs, and the compare links moved up.

\`abylab --version\` prints \`abylab $VERSION\`, which is what release.yml's tag gate compares against \`$TAG\`. Merging this is what the tag goes on; the tag push is what publishes the release." |
        tail -n1)"
    say "   $pr"
    # `gh pr checks --watch` exits 1 with "no checks reported" when it runs
    # before the workflow registers its jobs, which is a coin flip right after
    # `gh pr create`. Wait for a row with a tab-separated state first.
    for _ in $(seq 1 40); do
        gh pr checks "$pr" 2>/dev/null | grep -q "$(printf '\t')" && break
        sleep 5
    done
    gh pr checks "$pr" --watch --interval 20
    gh pr merge "$pr" --merge --delete-branch
    git switch main
    git fetch origin main --quiet
    TAGGED="$(git rev-parse origin/main)"
fi

# The tag has to land on a commit whose crates carry this version: release.yml
# opens with that comparison, so a tag on any other commit fails ten seconds in
# with every build skipped and no hint about which commit was wrong.
[ -n "$TAGGED" ] || die "no commit to tag"
git show "$TAGGED:crates/abylab-tui/Cargo.toml" | grep -q "^version = \"$VERSION\"$" ||
    die "commit $TAGGED does not carry version $VERSION; refusing to tag it"
say "   tagging $TAGGED ($(git log --oneline -1 --format=%h "$TAGGED"))"
git tag -a "$TAG" -m "$TAG" "$TAGGED"
git push origin "$TAG"
say "== $TAG pushed; the release workflow is building it"
say "   gh run list --workflow Release --limit 1"
