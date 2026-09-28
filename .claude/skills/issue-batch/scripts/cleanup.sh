#!/bin/zsh
# cleanup.sh [--apply]
#
# Removes worktrees whose work is merged, and their local branches. Dry run
# unless --apply. A worktree is removed only when it has no uncommitted or
# untracked files AND either
#   - its branch's PR is merged and the branch has no commit outside that PR's
#     head and origin/main (a local "merge main in" commit is fine), or
#   - it is detached at a commit already in origin/main.
# Entries whose directory is gone are pruned; their branches get the same
# merged-PR test before deletion. Everything else is listed as kept, with why.
set -u
apply=${1:-}
GH=$(command -v gh) || { echo "gh not found"; exit 1; }
main=$(cd "$(git -C "${0:A:h}" rev-parse --path-format=absolute --git-common-dir)/.." && pwd)
cd "$main" || exit 1
REPO=$($GH repo view --json nameWithOwner --jq .nameWithOwner) || exit 1
git fetch -q origin

merged_pr() {  # <branch> → "<n> <sha>" when its PR merged and nothing extra is on the branch
  local br=$1 pr n sha
  pr=$($GH pr list --repo "$REPO" --head "$br" --state merged --json number,headRefOid \
    --jq 'sort_by(.number) | last | "\(.number) \(.headRefOid)"' 2>/dev/null)
  [ -z "$pr" ] || [ "$pr" = "null null" ] && return 1
  n=${pr%% *}; sha=${pr#* }
  git cat-file -e "$sha^{commit}" 2>/dev/null || return 1
  [ -z "$(git log --no-merges --oneline "$br" --not "$sha" origin/main | head -1)" ] || return 1
  echo "$n $sha"
}

git worktree list --porcelain | awk '
  /^worktree /{w=substr($0,10)} /^branch /{b=substr($0,19); print w "\t" b}
  /^detached/{print w "\t-"}' | tail -n +2 |
while IFS=$'\t' read -r wt br; do
  if [ ! -d "$wt" ]; then
    echo "stale entry: $wt [$br]"
    if [ -n "$apply" ]; then
      git worktree unlock "$wt" 2>/dev/null; git worktree prune
      [ "$br" != - ] && merged_pr "$br" >/dev/null && git branch -D "$br" >/dev/null \
        && echo "  deleted branch $br (PR merged)"
    fi
    continue
  fi
  if [ -n "$(git -C "$wt" status --porcelain | head -1)" ]; then
    echo "KEEP (uncommitted or untracked files): $wt [$br]"; continue
  fi
  if [ "$br" = - ]; then
    if git merge-base --is-ancestor "$(git -C "$wt" rev-parse HEAD)" origin/main; then
      echo "remove (detached, in main): $wt"
      [ -n "$apply" ] && git worktree remove "$wt"
    else
      echo "KEEP (detached, not in main): $wt"
    fi
    continue
  fi
  if pr=$(merged_pr "$br"); then
    echo "remove (PR #${pr%% *} merged): $wt [$br]"
    [ -n "$apply" ] && git worktree remove "$wt" && git branch -D "$br" >/dev/null
  else
    echo "KEEP (no merged PR, or commits outside it): $wt [$br]"
  fi
done
[ -n "$apply" ] || echo "(dry run: pass --apply to remove)"
