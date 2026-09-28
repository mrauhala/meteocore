#!/bin/zsh
# review.sh <pr>
#
# What the automated reviewer said about the PR's CURRENT head:
#   - whether the latest summary's `meteocore-review:<sha>` marker is the head
#     (only then does "no issues" count; an older summary is stale). Only
#     comments by the review bot count: anyone can post a comment containing
#     the marker, and the merge queue trusts this script;
#   - the summary text;
#   - every inline comment that is not outdated (line != null), with how many
#     replies it has, so a finding you already answered is visible as such.
# Outdated inline comments (their code changed since) are omitted.
set -u
pr=$1
GH=$(command -v gh) || { echo "gh not found"; exit 1; }
REPO=$(cd "${0:A:h}" && $GH repo view --json nameWithOwner --jq .nameWithOwner) || exit 1
head=$($GH pr view "$pr" --repo "$REPO" --json headRefOid --jq .headRefOid)
BOT=github-actions[bot]
summary=$($GH api "repos/$REPO/issues/$pr/comments" --paginate \
  | jq -rs --arg bot "$BOT" '[add // [] | .[]
      | select(.user.login == $bot and .user.type == "Bot")
      | select(.body | contains("meteocore-review:"))] | last | .body // ""')
marker=$(echo "$summary" | grep -oE 'meteocore-review:[0-9a-f]{40}' | cut -d: -f2)
if [ -z "$summary" ]; then
  echo "NO REVIEW YET (head ${head:0:7})"
elif [ "$marker" = "$head" ]; then
  echo "REVIEW COVERS HEAD ${head:0:7}"
else
  echo "REVIEW STALE: summary covers ${marker:0:7}, head is ${head:0:7}"
fi
# The bot may put the marker on the same line as its text: strip the marker,
# never the line.
echo "$summary" | sed -E 's/<!-- *meteocore-review:[0-9a-f]+ *-->//'

echo
echo "--- open inline comments (not outdated):"
$GH api "repos/$REPO/pulls/$pr/comments" --paginate \
| jq -rs 'add // [] | . as $all
  | [.[] | select(.in_reply_to_id == null and .line != null)]
  | .[] | . as $c
  | ($all | map(select(.in_reply_to_id == $c.id)) | length) as $replies
  | "[\(.id)] \(.path):\(.line) by \(.user.login), created on \(.original_commit_id[0:7]), replies: \($replies)\n\(.body)\n"'
