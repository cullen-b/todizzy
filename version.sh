#!/usr/bin/env bash
set -euo pipefail

# Tag and push a new release. CI runs the build on tag push.

cd "$(dirname "$0")"

echo "→ Fetching tags from origin..."
git fetch --tags origin

latest=$(git tag --list 'v*' --sort=-v:refname | head -1)
echo "  Latest version: ${latest:-(none)}"

if [[ "${latest:-}" =~ ^v([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
    major="${BASH_REMATCH[1]}"
    minor="${BASH_REMATCH[2]}"
    patch="${BASH_REMATCH[3]}"
    echo ""
    echo "  patch bump: v${major}.${minor}.$((patch+1))"
    echo "  minor bump: v${major}.$((minor+1)).0"
    echo "  major bump: v$((major+1)).0.0"
fi

echo ""
read -p "New version (vX.Y.Z): " new_version

if [[ ! "$new_version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "Error: version must match vX.Y.Z" >&2
    exit 1
fi

if git rev-parse "$new_version" >/dev/null 2>&1; then
    echo "Error: tag $new_version already exists" >&2
    exit 1
fi

branch=$(git rev-parse --abbrev-ref HEAD)
if [[ "$branch" != "main" ]]; then
    echo "Warning: on branch '$branch', not main."
    read -p "Continue? [y/N] " yn
    [[ "$yn" =~ ^[Yy]$ ]] || exit 1
fi

if [[ -n "$(git status --porcelain)" ]]; then
    echo ""
    echo "Uncommitted changes:"
    git status --short
    echo ""
    echo "Tagging now would skip these changes (CI builds from the tagged commit)."
    read -p "Continue anyway? [y/N] " yn
    [[ "$yn" =~ ^[Yy]$ ]] || exit 1
fi

unpushed=$(git log "origin/${branch}..HEAD" --oneline 2>/dev/null || true)
if [[ -n "$unpushed" ]]; then
    echo ""
    echo "Unpushed commits on $branch:"
    echo "$unpushed"
    read -p "Push to origin/$branch first? [Y/n] " yn
    if [[ ! "$yn" =~ ^[Nn]$ ]]; then
        git push origin "$branch"
    fi
fi

sha=$(git rev-parse --short HEAD)
echo ""
echo "Will tag $new_version → $sha and push to origin."
read -p "Proceed? [y/N] " yn
[[ "$yn" =~ ^[Yy]$ ]] || exit 1

git tag "$new_version"
git push origin "$new_version"

echo ""
echo "✓ Pushed $new_version"
echo "  CI:      https://github.com/cullen-b/todizzy/actions"
echo "  Release: https://github.com/cullen-b/todizzy/releases/tag/$new_version"
