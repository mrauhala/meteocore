---
name: issue-batch
description: Autonomous batch run over the meteocore GitHub backlog — pick issues by focus/priority/effort, implement each in its own git worktree, open PRs, drive CI and the claude-review bot to clean, and self-merge until N PRs are merged. Use when the user asks to "work through issues", "do the next N PRs", "run a batch", "self-merge after review", or names a focus such as performance, bugs, spec-compliance, high/medium priority, or small effort.
argument-hint: "[N] [focus: perf|bug|spec|security|reliability|…] [priority: high medium low] [effort: tiny small medium large] [epic:#N] [#issue …] [no-merge]"
---

# Issue batch

Work the backlog in parallel worktrees until **N PRs are merged** (default 10),
each one reviewed clean at its final head before it merges.

Arguments: `$ARGUMENTS`

## 1. Read the arguments

| Words | Meaning | Default |
|---|---|---|
| a bare number | N, the number of PRs to merge | 10 |
| `performance`/`perf`, `bug`/`bugs`, `spec-compliance`/`spec`, `security`, `reliability`, `enhancement`, `architecture`, `operational` | `--focus` label filter; any of them matches | all |
| `high`, `medium`, `low` | `--priority` | all |
| `tiny`, `small`, `large`, `effort:medium` | `--effort`. A bare `medium` means priority; write `effort:medium` for effort | all |
| `epic:#N` | only the epic's open sub-tasks, then its next phase | — |
| `#123 #456` | exactly these issues, no picking | — |
| `no-merge` | stop each PR at "review clean, CI green" and report; don't merge | merge |

Natural phrasing works too ("only high and medium", "small effort bugs").
Invoking this skill **is** the user's authorization to self-merge PRs of this
batch once review is satisfied (unless `no-merge`); it does not extend past
the batch, and production deploys stay with the user.

## 2. Set up

- State dir `<state>`: the session scratchpad. Create `<state>/bodies/` and
  write `<state>/trailers` with the commit attribution lines from this
  session's instructions (`Co-Authored-By: …` and any session line).
- Scripts live next to this file: `scripts/`. They are zsh, macOS-flavoured
  (`sed -i ''`), and find the repo from their own location.
- Worktrees go in `<repo>-wt/<name>` beside the main checkout
  (`~/Code/dataserver-wt/<name>`), each on its own branch from `origin/main`:
  `git worktree add <repo>-wt/<name> -b <type>/<slug> origin/main`.
- The pre-commit guard hook checks the **main checkout's** branch: if it is
  `main`, park it first (`git -C <repo> switch -c wip/worktree-host` or switch
  to that branch) or every worktree commit is blocked. Tell the user.
- All worktrees share `<repo>/target` (`CARGO_TARGET_DIR`), so cargo runs
  serialize: lint one worktree at a time.
- Start the watchers as Monitors (max 30 min each; re-arm on expiry):
  `scripts/watch.sh <state>` (CI checks + comments for `<state>/prs`) and,
  unless `no-merge`, `scripts/merger.sh <state>` (the merge queue).

## 3. Pick issues

`scripts/pick.sh [--focus …] [--priority …] [--effort …]` lists candidates,
highest priority and smallest effort first, without epics or issues an open
PR already names. Read each candidate (`gh issue view N`) and **skip** it when:

- it needs the user's decision: a product or semantics choice, a trade-off the
  issue leaves open ("decide whether…"), a design question;
- it needs access, credentials, a licence, paid data, or a large download (the
  user is often on a metered connection);
- it needs a production deploy or nexus access to verify;
- it is an epic (take its sub-tasks instead), or overlaps another pick's files
  enough to conflict.

Take about 1.3 × N so failures and skips still leave N. An issue whose scope is
too big for one PR can ship a clearly bounded part; its PR then says `Refs #N`
and the issue gets a progress comment (step 7).

## 4. Implement, one worktree per issue

- Read the root and the crate's `CLAUDE.md` and follow them (status-page
  READMEs for EDR/Features, Grafana for new metrics, OpenAPI for new
  parameters, CoverageJSON schema tests, geo/SQL/XML safety rules).
- Verify the issue's claims against the current code first; issues age.
- Add tests that fail without the change. Before pushing a behaviour change,
  grep the crate's tests (and other crates' tests) for fixtures or mocks that
  relied on the old behaviour and update them.
- `scripts/lint.sh <worktree> <crate> …` → must print `LINT OK`.
- Run locally any NEW test that pins values from an external reference
  (coordinates from `cs2cs`, expected colours, parsed fixtures). CI runs the
  rest; local first-runs are slow on macOS. A new pinned test once caught a
  real pre-existing bug only in CI.
- Write `<state>/bodies/<name>.title` (conventional-commit subject) and
  `<state>/bodies/<name>.body`: what changed and why, how it's tested,
  `Closes #N` or `Refs #N`, ending with this session's PR attribution footer.
  The body becomes the squash commit: **no nested parentheses**
  (`scripts/nested_parens.py` checks; `ship.sh` refuses otherwise).
- `scripts/ship.sh <state> <worktree> [new files…]` commits, pushes, opens the
  PR and adds it to the watch list. Name every NEW file explicitly; it never
  stages untracked files on its own.

## 5. Drive each PR to clean

On watch events:

- **Check failed:** `gh run view <run> --log-failed`, fix in the worktree,
  lint, commit (never amend a pushed commit), push.
- **Review posted:** `scripts/review.sh <pr>`. It says whether the summary
  covers the current head and lists inline comments that aren't outdated.
  - Fix every significant finding (correctness, security, a rule in
    CLAUDE.md, a regression, docs that now contradict the code). Reply on each
    inline comment with where it was fixed:
    `gh api repos/<o>/<r>/pulls/<pr>/comments/<id>/replies -f body=…`.
  - Minor style nits: use judgment. Fixing restarts CI and review, so batch
    them with a real fix or leave them. Push back, with reasons, when a
    finding is wrong; say so in the reply.
  - A finding about behaviour that predates the PR and is out of its scope:
    record it on the relevant issue, or open one, instead of widening the PR.
- **Conflict with main:** merge `origin/main` into the branch, resolve,
  lint, push. Never rebase a pushed branch, never force-push.

## 6. Merge

Only when the review summary's `meteocore-review:<sha>` marker **equals the
current head** and nothing significant is open, append `<pr> <head-sha>` to
`<state>/mergeq`. `merger.sh` squash-merges it once the required checks pass
(the repo has auto-merge disabled). It drops the PR from the queue if the head
moves, a check fails or it conflicts, and you handle it and re-queue. Stop
queueing new work once N are merged or queued; let the rest finish.

## 7. Close out

- A `Closes #N` PR closes its issue on merge; check it did.
- For each `Refs #N` issue, comment what landed (PR number) and exactly what
  scope is left.
- `scripts/cleanup.sh` (dry run), then `scripts/cleanup.sh --apply`: removes
  merged worktrees and their branches, keeps anything with uncommitted work.
- Stop the watchers. Save to memory anything learned that the repo doesn't
  record (tool gotchas, decisions the user made, lessons).
- Report: merged PRs, the notable findings fixed along the way, skipped issues
  with the reason (especially the ones waiting on the user's decision), and
  whether the main checkout is still parked on `wip/worktree-host`. Nothing is
  deployed. Send a push notification if the user may be away.
