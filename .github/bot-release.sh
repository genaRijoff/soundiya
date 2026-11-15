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
    git cherry-pick --no-commit "$c" >/dev/null 2>&1 || true
    # A commit that also changed a workflow file: only its other files are carried over
    # (the worktree is a throwaway checkout of the upstream tag, cleaned here).
    git reset --quiet HEAD -- .github 2>/dev/null || true
    git checkout --quiet HEAD -- .github 2>/dev/null || true
    git clean -fdq -- .github
    if [[ -n $(git diff --name-only --diff-filter=U) ]]; then
        git diff --name-only --diff-filter=U | sed 's/^/conflict: /' >&2
        echo "patch does not apply on $up: $subject" >&2
        exit 1
    fi
    git cherry-pick --quit 2>/dev/null || true
    if git diff --cached --quiet; then
        # Nothing left to change: upstream already has this patch.
        git reset --quiet --hard HEAD
        echo "already upstream: $subject"
    else
        git commit --quiet --no-verify -C "$c"
        git commit --quiet --amend --no-verify -m "$(git log -1 --format=%B "$c")" -m "(cherry picked from commit $c)"
        echo "applied: $subject"
    fi
done

# A release tag carrying workflow files could not be pushed with the Actions token.
if ! git diff --quiet "$upc" HEAD -- .github; then
    echo "a patch touches .github/: keep workflow changes in their own commits" >&2
    exit 1
fi
