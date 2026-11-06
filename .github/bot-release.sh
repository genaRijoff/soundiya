#!/usr/bin/env bash
# Puts our patches (the commits of the current branch that upstream does not have) on top of an
# upstream release tag, in a separate worktree. Files under .github/ are never carried over.
#   bash .github/bot-release.sh <upstream-git-url> <upstream-tag> <worktree-dir>
# Exit 0: the worktree is ready. Exit 1: a patch does not apply (it needs a manual update).
set -Eeuo pipefail

url=$1 up=$2 work=$3
[[ $up =~ ^v[0-9]+(\.[0-9]+){1,3}$ ]] || { echo "unexpected upstream tag: $up" >&2; exit 1; }

git fetch --quiet --no-tags "$url" "+refs/tags/$up:refs/upstream/$up"
upc=$(git rev-parse --verify "refs/upstream/$up^{commit}")
base=$(git merge-base HEAD "$upc") || { echo "no common history with $up" >&2; exit 1; }
mapfile -t patches < <(git rev-list --reverse --no-merges "$base..HEAD" -- . ':(exclude).github')
(( ${#patches[@]} > 0 )) || { echo "no patches on top of $base" >&2; exit 1; }

git worktree add --quiet --detach "$work" "$upc"
cd "$work"
for c in "${patches[@]}"; do
    subject=$(git log -1 --format=%s "$c")
    if git cherry-pick -x "$c" >/dev/null 2>&1; then
        echo "applied: $subject"
    elif git diff --quiet && git diff --cached --quiet; then
        # Nothing left to change: upstream already has this patch.
        git cherry-pick --skip
        echo "already upstream: $subject"
    else
        git diff --name-only --diff-filter=U | sed 's/^/conflict: /' >&2
        git cherry-pick --abort
        echo "patch does not apply on $up: $subject" >&2
        exit 1
    fi
done

# A release tag carrying workflow files could not be pushed with the Actions token.
if ! git diff --quiet "$upc" HEAD -- .github; then
    echo "a patch touches .github/: keep workflow changes in their own commits" >&2
    exit 1
fi
