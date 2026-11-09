#!/bin/sh
# Cut a release: scripts/release.sh v1.2.3 [-y] [--dry-run]
#
# Checks the tree, runs vet and tests, creates an annotated tag vX.Y.Z on the current
# commit and pushes it. The tag push starts .github/workflows/release.yml, which builds
# the binaries and publishes the GitHub Release.
set -e
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

usage() { echo "Usage: scripts/release.sh vX.Y.Z [-y] [--dry-run]" >&2; exit 2; }
die()   { echo "error: $*" >&2; exit 1; }

[ $# -ge 1 ] || usage
VERSION=$1; shift
ASSUME_YES=; DRY_RUN=
for arg in "$@"; do
    case "$arg" in
        -y) ASSUME_YES=1 ;;
        --dry-run) DRY_RUN=1 ;;
        *) usage ;;
    esac
done

echo "$VERSION" | grep -Eq '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' \
    || die "version must look like vX.Y.Z (got '$VERSION')"

BRANCH=$(git rev-parse --abbrev-ref HEAD)
[ "$BRANCH" = main ] || die "release from main (you are on '$BRANCH')"
[ -z "$(git status --porcelain)" ] || die "working tree is not clean; commit or stash first"
git rev-parse -q --verify "refs/tags/$VERSION" >/dev/null && die "tag $VERSION already exists"

echo "Fetching origin..."
git fetch --quiet origin main --tags
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] \
    || die "HEAD is not origin/main; push or pull first"

echo "Running vet and tests..."
(cd app && go vet ./... && go test ./...)

COMMIT=$(git rev-parse --short HEAD)
echo "Release $VERSION from $COMMIT"

if [ -n "$DRY_RUN" ]; then
    echo "Dry run: nothing tagged or pushed."
    exit 0
fi

if [ -z "$ASSUME_YES" ]; then
    printf 'Create tag %s and push it to origin? [y/N] ' "$VERSION"
    read -r answer </dev/tty || answer=
    case "$answer" in y|Y|yes) ;; *) echo "Aborted."; exit 1 ;; esac
fi

git tag -a "$VERSION" -m "Release $VERSION"
git push origin "$VERSION"
echo "Pushed $VERSION. Follow the build at: gh run watch (or the Actions tab)."
