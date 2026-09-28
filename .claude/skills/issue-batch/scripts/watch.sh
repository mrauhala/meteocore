#!/bin/zsh
# watch.sh <state-dir>
#
# Polls every PR listed in <state>/prs (one number per line; edit the file
# freely while this runs) and prints one line per event:
#   #N <check>: pass|fail           a required or review check finished
#   #N <id> comment|inline <user>   a new conversation or review comment
#   #N ALL CHECKS DONE head=<sha7> [FAILED: …]   once per head
# Merged or closed PRs are skipped. Run it under a Monitor and re-arm it when
# the monitor expires. Noise (skipped jobs, the non-Rust CodeQL analyses) is
# filtered out.
set -u
state=${1:A}
GH=$(command -v gh) || { echo "gh not found"; exit 1; }
REPO=$(cd "${0:A:h}" && $GH repo view --json nameWithOwner --jq .nameWithOwner) || exit 1
st=$state/watch
mkdir -p "$st"
touch "$state/prs"
noise='Analyze \((actions|python|javascript-typescript)\)|: skipping'
while true; do
  for pr in $(cat "$state/prs"); do
    info=$($GH pr view "$pr" --repo "$REPO" --json state,headRefOid 2>/dev/null) || continue
    [ "$(echo "$info" | jq -r .state)" = OPEN ] || continue
    head=$(echo "$info" | jq -r .headRefOid | cut -c1-7)
    checks=$($GH pr checks "$pr" --repo "$REPO" --json name,bucket 2>/dev/null || echo '[]')
    echo "$checks" | jq -r '.[] | select(.bucket != "pending") | "\(.name): \(.bucket)"' \
      | sort > "$st/$pr.cur"
    touch "$st/$pr.prev"
    comm -13 "$st/$pr.prev" "$st/$pr.cur" | grep -vE "$noise" | grep -v '^$' | sed "s/^/#$pr /"
    mv "$st/$pr.cur" "$st/$pr.prev"
    { $GH api "repos/$REPO/issues/$pr/comments" --paginate \
        --jq '.[] | "\(.id) comment \(.user.login)"' 2>/dev/null
      $GH api "repos/$REPO/pulls/$pr/comments" --paginate \
        --jq '.[] | "\(.id) inline \(.user.login) \(.path)"' 2>/dev/null
    } | sort > "$st/$pr.ccur"
    [ -f "$st/$pr.cseen" ] || cp "$st/$pr.ccur" "$st/$pr.cseen"
    comm -13 "$st/$pr.cseen" "$st/$pr.ccur" | grep -v '^$' | sed "s/^/#$pr /"
    mv "$st/$pr.ccur" "$st/$pr.cseen"
    if echo "$checks" | jq -e 'length > 0 and all(.bucket != "pending")' >/dev/null \
       && [ ! -f "$st/$pr.done.$head" ]; then
      touch "$st/$pr.done.$head"
      fails=$(echo "$checks" | jq -r '[.[] | select(.bucket == "fail") | .name] | join(", ")')
      echo "#$pr ALL CHECKS DONE head=$head ${fails:+FAILED: $fails}"
    fi
  done
  sleep 45
done
