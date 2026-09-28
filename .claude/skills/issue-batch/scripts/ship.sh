#!/bin/zsh
# ship.sh <state-dir> <worktree-dir> [new-file ...]
#
# Commits a finished worktree and opens its PR against main:
#   - stages tracked changes (`git add -u`) plus each NEW file named on the
#     command line — never `git add -A`, the repo is full of untracked data;
#   - commit message = <state>/bodies/<name>.title, a blank line, the PR body
#     without its trailing "🤖 Generated with" footer, then <state>/trailers;
#   - pushes, opens the PR with the full body file, appends the PR number to
#     <state>/prs for watch.sh.
# <name> is the worktree directory's basename.
set -u
here=${0:A:h}
state=${1:A}; wt=${2:A}; shift 2
GH=$(command -v gh) || { echo "gh not found"; exit 1; }
name=${wt:t}
title_f=$state/bodies/$name.title
body_f=$state/bodies/$name.body
for f in $title_f $body_f $state/trailers; do
  [ -s "$f" ] || { echo "missing or empty: $f"; exit 1; }
done
python3 "$here/nested_parens.py" "$body_f" || exit 1

cd "$wt" || exit 1
branch=$(git branch --show-current)
[ "$branch" = main ] && { echo "refusing: $name is on main"; exit 1; }
repo=$($GH repo view --json nameWithOwner --jq .nameWithOwner) || exit 1

git add -u
for f in "$@"; do git add -- "$f"; done
title=$(cat "$title_f")
body=$(sed '/^🤖 Generated with/,$d' "$body_f")
printf '%s\n\n%s\n\n%s\n' "$title" "$body" "$(cat "$state/trailers")" > "$state/commitmsg.$name"
git commit -q -F "$state/commitmsg.$name" || { echo "COMMIT FAILED $name"; exit 1; }
git push -q -u origin "$branch" 2>&1 | grep -v '^remote:'

url=$(cd /tmp && $GH pr create --repo "$repo" --base main --head "$branch" \
  --title "$title" --body-file "$body_f" 2>&1 | tail -1)
echo "$name -> $url"
n=${url##*/}
[[ $n == <-> ]] && echo "$n" >> "$state/prs"
