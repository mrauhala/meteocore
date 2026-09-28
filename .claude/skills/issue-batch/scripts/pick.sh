#!/bin/zsh
# pick.sh [--focus L,L] [--priority P,P] [--effort E,E] [--milestone M] [--limit N]
#
# Lists open issues that are candidates for a batch, one per line:
#   <number> <TAB> <priority> <TAB> <effort> <TAB> <type labels> <TAB> <title>
# sorted by priority (high first), then effort (tiny first), then number.
#
# Excluded: epics, and issues an open PR already names with
# Closes/Fixes/Resolves/Refs (listed on stderr so you can see why).
#
# --focus takes label names or aliases: perf → performance, bugs → bug,
# spec → spec-compliance. A value matches if the issue carries ANY of them.
set -u
GH=$(command -v gh) || { echo "gh not found" >&2; exit 1; }
REPO=$($GH repo view --json nameWithOwner --jq .nameWithOwner) || exit 1

focus="" prio="" effort="" milestone="" limit=300
while [ $# -gt 0 ]; do
  case $1 in
    --focus) focus=$2; shift 2 ;;
    --priority) prio=$2; shift 2 ;;
    --effort) effort=$2; shift 2 ;;
    --milestone) milestone=$2; shift 2 ;;
    --limit) limit=$2; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
mapped=""
for f in ${(s:,:)${focus// /}}; do
  case $f in
    perf) f=performance ;;
    bugs) f=bug ;;
    spec) f=spec-compliance ;;
  esac
  mapped+="${mapped:+,}$f"
done
focus=$mapped

# Issues already claimed by an open PR.
claimed=$($GH pr list --repo "$REPO" --state open --limit 100 --json body --jq '.[].body' \
  | grep -oiE '(close[sd]?|fix(e[sd])?|resolve[sd]?|refs?) +#[0-9]+' | grep -oE '[0-9]+' | sort -u | tr '\n' ',')
[ -n "$claimed" ] && echo "skipped, named by an open PR: ${claimed%,}" >&2

$GH issue list --repo "$REPO" --state open --limit "$limit" \
  --json number,title,labels,milestone \
| jq -r --arg focus "$focus" --arg prio "$prio" --arg effort "$effort" \
       --arg ms "$milestone" --arg claimed ",$claimed" '
  def csv($s): if $s == "" then [] else ($s | split(",")) end;
  def tagged($prefix): ([.labels[].name | select(startswith($prefix)) | ltrimstr($prefix)] | first) // "-";
  def rank($v; $order): ($order | index($v)) // ($order | length);
  [ .[]
    | . as $i
    | [.labels[].name] as $names
    | select(($names | index("epic")) | not)
    | select(($claimed | contains("," + ($i.number | tostring) + ",")) | not)
    | select((csv($focus) | length) == 0 or (csv($focus) | any(. as $f | $names | index($f))))
    | select((csv($prio) | length) == 0 or (csv($prio) | index($i | tagged("priority: "))))
    | select((csv($effort) | length) == 0 or (csv($effort) | index($i | tagged("effort: "))))
    | select($ms == "" or (.milestone.title // "") == $ms)
    | { n: .number, t: .title,
        p: tagged("priority: "), e: tagged("effort: "),
        k: ([$names[] | select((startswith("priority: ") or startswith("effort: ")) | not)] | join(",")) }
  ]
  | sort_by(rank(.p; ["high","medium","low"]), rank(.e; ["tiny","small","medium","large"]), .n)
  | .[] | "\(.n)\t\(.p)\t\(.e)\t\(.k)\t\(.t)"'
