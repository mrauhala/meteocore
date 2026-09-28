#!/bin/zsh
# merger.sh <state-dir>
#
# Merge queue. Each line of <state>/mergeq is "<pr> <verified-head-sha>": a
# PR whose review you have read and accepted AT THAT SHA. Every minute, each
# queued PR is:
#   - merged (squash, branch deleted, --match-head-commit <sha>) once GitHub
#     reports mergeStateStatus CLEAN or UNSTABLE, i.e. the required checks
#     passed (UNSTABLE = only a non-required check such as CodeQL pending);
#   - dropped with a message if its head moved (re-review, then re-queue),
#     a check failed, or it conflicts with main (merge main in, re-queue).
# The repo has auto-merge disabled; this loop stands in for it. Merged PR
# numbers are appended to <state>/merged.
set -u
state=${1:A}
GH=$(command -v gh) || { echo "gh not found"; exit 1; }
REPO=$(cd "${0:A:h}" && $GH repo view --json nameWithOwner --jq .nameWithOwner) || exit 1
q=$state/mergeq
touch "$q" "$state/merged"
drop() { sed -i '' "/^$1 /d" "$q"; }
while true; do
  while read -r n sha; do
    [ -z "$n" ] && continue
    info=$($GH pr view "$n" --repo "$REPO" \
      --json state,headRefOid,mergeStateStatus,statusCheckRollup \
      --jq '[.state, .headRefOid, .mergeStateStatus,
             ([.statusCheckRollup[] | select((.conclusion // .state) == "FAILURE")
               | (.name // .context)] | join(","))] | @tsv' 2>/dev/null) || continue
    IFS=$'\t' read -r st head ms failed <<< "$info"
    if [ "$st" = MERGED ]; then echo "#$n already merged"; drop "$n"; continue; fi
    if [ "$head" != "$sha" ]; then echo "#$n head moved to ${head:0:7}; dropped, re-review"; drop "$n"; continue; fi
    if [ -n "$failed" ]; then echo "#$n check FAILED: $failed; dropped"; drop "$n"; continue; fi
    case $ms in
      CLEAN|UNSTABLE|HAS_HOOKS)
        out=$($GH pr merge "$n" --repo "$REPO" --squash --delete-branch \
          --match-head-commit "$sha" 2>&1 | tail -1)
        if [ "$($GH pr view "$n" --repo "$REPO" --json state --jq .state)" = MERGED ]; then
          echo "#$n MERGED"; echo "$n" >> "$state/merged"
          sed -i '' "/^$n\$/d" "$state/prs" 2>/dev/null
        else
          echo "#$n merge attempt failed: $out"
        fi
        drop "$n" ;;
      DIRTY) echo "#$n CONFLICTS with main; dropped, merge main in"; drop "$n" ;;
    esac
  done < "$q"
  sleep 60
done
