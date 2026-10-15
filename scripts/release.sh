#!/usr/bin/env bash
#
# Releases gitst: bumps the version in Cargo.toml and Cargo.lock, dates the
# changelog's `## Unreleased` section, commits "Release X.Y.Z", tags it, and
# pushes the commit and the tag together. The tag starts the release
# workflow (.github/workflows/release.yml), which builds the binaries,
# publishes the GitHub release, and updates the Homebrew tap.
#
#   scripts/release.sh            minor: 0.1.13 -> 0.2.0
#   scripts/release.sh patch      0.1.13 -> 0.1.14
#   scripts/release.sh major      0.1.13 -> 1.0.0
#   scripts/release.sh 0.3.0      that version exactly
#
# Nothing is changed unless main is checked out, clean, and level with
# origin/main, and the checks CI runs pass. It asks before it commits.

set -euo pipefail

die() {
	echo "release: $*" >&2
	exit 1
}
usage="usage: scripts/release.sh [major|minor|patch|X.Y.Z]"

[ $# -le 1 ] || die "$usage"
bump="${1:-minor}"

cd "$(git rev-parse --show-toplevel)"

branch="$(git symbolic-ref --quiet --short HEAD)" || die "HEAD is detached"
[ "$branch" = main ] || die "on $branch, not main"
[ -z "$(git status --porcelain)" ] || die "the working tree has changes"
git fetch --quiet origin main
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] ||
	die "main is not level with origin/main"

current="$(sed -n 's/^version = "\([0-9]*\.[0-9]*\.[0-9]*\)".*/\1/p' Cargo.toml | head -n 1)"
[ -n "$current" ] || die "no version in Cargo.toml"
IFS=. read -r major minor patch <<<"$current"
case "$bump" in
major) new="$((major + 1)).0.0" ;;
minor) new="$major.$((minor + 1)).0" ;;
patch) new="$major.$minor.$((patch + 1))" ;;
*)
	[[ "$bump" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "$usage"
	new="$bump"
	;;
esac
if [ "$new" = "$current" ] ||
	[ "$(printf '%s\n' "$current" "$new" | sort -V | tail -n 1)" != "$new" ]; then
	die "$new is not after $current"
fi
if git rev-parse --quiet --verify "refs/tags/$new" >/dev/null ||
	git ls-remote --exit-code --tags origin "refs/tags/$new" >/dev/null; then
	die "tag $new already exists"
fi

echo "Running the checks CI runs…"
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked

echo
echo "Release $current -> $new"
if grep -q '^## Unreleased' CHANGELOG.md; then
	dated="## $new ($(date +%F))"
	echo "  CHANGELOG.md: '## Unreleased' becomes '$dated'"
else
	dated=
	echo "  CHANGELOG.md has no '## Unreleased' section, so it stays as it is"
fi
echo "  commit 'Release $new', tag $new, and push both to origin"
read -r -p "Go ahead? [y/N] " answer
case "$answer" in
y | Y | yes) ;;
*) die "stopped; nothing was changed" ;;
esac

# Only the first `version =` line: the [package] one.
perl -pi -e "\$d ||= s/^version = \"[0-9.]+\"/version = \"$new\"/" Cargo.toml
# Cargo.lock too, so release builds can use --locked.
perl -0pi -e "s/(\[\[package\]\]\nname = \"gitst\"\nversion = \")[0-9.]+/\${1}$new/" Cargo.lock
grep -q "^version = \"$new\"" Cargo.toml || die "Cargo.toml was not updated"
cargo metadata --locked --format-version 1 >/dev/null ||
	die "Cargo.lock is out of step with Cargo.toml"
if [ -n "$dated" ]; then
	perl -pi -e "\$d ||= s/^## Unreleased.*/$dated/" CHANGELOG.md
fi

git add Cargo.toml Cargo.lock CHANGELOG.md
git commit --quiet -m "Release $new"
git tag "$new"
# Atomic, so the tag is never on the remote without the commit, or the
# commit without the tag.
if ! git push --atomic origin main "refs/tags/$new"; then
	echo >&2
	echo "release: the push failed. The commit and tag are still here; push" >&2
	echo "them with:  git push --atomic origin main refs/tags/$new" >&2
	echo "or undo them with:  git tag -d $new && git reset --hard HEAD~1" >&2
	exit 1
fi

echo
echo "Released $new. The release workflow is now building it."
url="$(git remote get-url origin)"
case "$url" in
*github.com[:/]*)
	repo="${url#*github.com[:/]}"
	echo "  https://github.com/${repo%.git}/actions"
	;;
esac
