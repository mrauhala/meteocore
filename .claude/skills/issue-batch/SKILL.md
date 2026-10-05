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
  (`sed -i ''`), and find the repo from their own location. The shell is zsh
  too: an unquoted `$var` holding several arguments is NOT split, so write
  `${=var}` (e.g. `cargo test ${=t}` in a loop over test selections).
- Worktrees go in `<repo>-wt/<name>` beside the main checkout
  (`~/Code/dataserver-wt/<name>`), each on its own branch from `origin/main`:
  `git worktree add <repo>-wt/<name> -b <type>/<slug> origin/main`.
- The pre-commit guard hook checks the **main checkout's** branch: if it is
  `main`, park it first (`git -C <repo> switch -c wip/worktree-host` or switch
  to that branch) or every worktree commit is blocked. Tell the user.
- Every worktree builds in its own target dir, `<repo>/target-<name>`:
  `lint.sh` defaults to it, and every other cargo command for that worktree
  (agents' `cargo test`, your own) sets
  `CARGO_TARGET_DIR=<repo>/target-<name>` too. One shared `target/`
  serialized every cargo run on its lock and let one worktree reuse another's
  build of a crate it didn't edit. Use one more dir, e.g. `target-ship`, for
  your own pre-ship checks, and never run two jobs on one dir at once: a
  rebuild from another tree between test binaries gives errors naming code
  that isn't there. Each dir grows to roughly 6–11 GB; check `df -h` before
  launching many agents (the disk once reached 99 %), and `cleanup.sh --apply`
  removes a merged worktree's dir.
- On macOS every freshly linked test binary waits in a security scan, one at
  a time, for minutes. Agents run only the test binaries they add or change;
  CI runs the suites.
- Run the watchers from a checkout that stays put for the whole batch, e.g.
  the main checkout or a dedicated worktree: a Monitor executes the script
  file, so switching that checkout's branch mid-run changes or removes it.
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

**Delegate the implementation, keep the judgment.** Create the worktrees, then
launch one background general-purpose subagent per issue (all in one message
so they run in parallel). Give each: its worktree path and branch, the issue
number and the exact scope, the rules below, the lint and scoped-test
commands, where to write the PR title and body, and "do not commit, stage,
push or open a PR; stop and report if the issue is already done or needs a
maintainer decision". When a report arrives, read the diff yourself, check
anything it flags (public-type changes, deviations from the issue, "needs
your call"), then ship. Main moves while agents work: if files an agent
touched changed on main, `git stash -u`, merge `origin/main`, `git stash
pop`, re-lint, then ship. The stash list is shared by every worktree: when a
conflicted pop keeps the entry, check that `stash@{0}` is this branch's
("WIP on <branch>") before dropping it.

- Read the root and the crate's `CLAUDE.md` and follow them (status-page
  READMEs for EDR/Features, Grafana for new metrics, OpenAPI for new
  parameters, CoverageJSON schema tests, geo/SQL/XML safety rules).
- Verify the issue's claims against the current code first; issues age.
- Add tests that fail without the change. Before pushing a behaviour change,
  grep the crate's tests (and other crates' tests) for fixtures or mocks that
  relied on the old behaviour and update them.
- `scripts/lint.sh <worktree> <crate> …` → must print `LINT OK`. With
  `server` in the list it also checks `server --features icechunk`, which
  the Docker build uses and which a plain clippy run can miss.
- Run locally any NEW test that pins values from an external reference
  (coordinates from `cs2cs`, expected colours, parsed fixtures). CI runs the
  rest; local first-runs are slow on macOS. A new pinned test once caught a
  real pre-existing bug only in CI.
- Write `<state>/bodies/<name>.title` (conventional-commit subject) and
  `<state>/bodies/<name>.body`: what changed and why, how it's tested,
  `Closes #N` or `Refs #N`, ending with this session's PR attribution footer.
  The body becomes the squash commit: **no nested parentheses**
  (`scripts/nested_parens.py` checks; `ship.sh` refuses otherwise).
- Titles, bodies, commit messages, issues and comments are public: never
  name client sites or deployment hosts. Grep the body, title and the
  branch's commit messages for them before shipping and again before queueing.
- When an issue lists open questions, or an agent's report says it needs a
  call (semantics, naming, a download, a behaviour change existing clients
  will see), ask the user with the options and a recommendation; don't let
  the agent guess. Agents check the requirement texts a change relies on,
  and a class or capability the server declares is implemented, not dropped.
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
  lint, push. Never rebase a pushed branch, never force-push. Hand a hard
  semantic merge back to the PR's own agent (SendMessage): say what main
  added and which interaction it must decide, e.g. a new output format against
  a query type the other PR added, and tell it not to push.
- **claude-review fails with "did not post the required summary":** the
  reviewer ran out of allowed tools or turns, which is common on large PRs.
  It is not a clean review. `gh run rerun <run> --failed` (the next push
  also re-runs it), and meanwhile read the key paths yourself.
- **CodeQL `rust/cleartext-logging` on a test:** usually a value
  interpolated into an assert message. Drop the interpolation and reply on
  the alert.
- **CI breaks on every PR after a new stable Rust:** fix it in its own `ci:`
  PR (CI clippy runs stable with `-D warnings`), merge it, then merge main
  into the open PRs. If the fix needs a newer Rust than the Dockerfile's
  `rust:<x.y>-slim-trixie` image, bump the image in its own PR too.

## 6. Merge

Only when the review summary's `meteocore-review:<sha>` marker **equals the
current head** and nothing significant is open, append `<pr> <head-sha>` to
`<state>/mergeq`. `merger.sh` squash-merges it once the required checks pass
(the repo has auto-merge disabled). It drops the PR from the queue if the head
moves, a check fails or it conflicts, and you handle it and re-queue. Stop
queueing new work once N are merged or queued; let the rest finish.

`merger.sh` checks only textual conflicts and the head's own checks, so a PR
whose CI passed against an older main can still break main. Before queueing a
head that doesn't contain the current `origin/main`:

1. `git merge-tree --write-tree --name-only origin/main <head>` lists any
   textual conflicts.
2. Look at what main gained since the head's base for the semantic breaks
   that merged cleanly before:
   - a new field on a struct the PR's new tests build as literals;
   - a new trait method that a wrapper must forward (the derived-wind and
     nowcast wrappers forward every `MapEngine`/`EdrEngine` method);
   - a new parameter or output format that changes another query type.
3. If nothing conflicts or changed, trial-merge in the worktree
   (`git merge --no-commit --no-ff origin/main`, lint, run the affected test
   binaries, `git merge --abort`) and queue the reviewed head as it is.
   Otherwise merge main into the branch, fix, lint and push, which restarts CI
   and review.

PRs that all touch one large file conflict pairwise, so each merge sends the
rest back for another merge and review. Merge them one at a time, and start
the next one's update as soon as the previous lands.

If a green, queued PR is still open a few minutes after its last check
passed, the merger may be stuck: `pgrep -f merger.sh`, stop it and start one
again (never two on one queue).

## 7. Close out

- A `Closes #N` PR closes its issue on merge; check it did.
- For each `Refs #N` issue, comment what landed (PR number) and exactly what
  scope is left.
- `scripts/cleanup.sh` (dry run), then `scripts/cleanup.sh --apply`: removes
  merged worktrees, their branches and their `target-<name>` dirs, keeps
  anything with uncommitted work, and lists build dirs left without a
  worktree, such as `target-ship`.
- Check main's CI on the last merge commit
  (`gh api repos/<o>/<r>/commits/<sha>/check-runs`), not `gh run list`.
- Stop the watchers. Save to memory anything learned that the repo doesn't
  record (tool gotchas, decisions the user made, lessons).
- Report: merged PRs, the notable findings fixed along the way, skipped issues
  with the reason (especially the ones waiting on the user's decision), and
  whether the main checkout is still parked on `wip/worktree-host`. Nothing is
  deployed. Send a push notification if the user may be away.
