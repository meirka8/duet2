# Duet — instructions for Claude Code sessions

The project playbook (subagent roster, git and PR policy, architectural
boundaries) lives in `AGENTS.md` and applies in full:

@AGENTS.md

The points below are the ones that have been missed in practice and are
therefore restated here, where every session reads them.

## Record reasoning in CVC (mandatory)

CVC (Cognitive Version Control) is wired into this repository: git hooks
link recorded thoughts to the commits they precede and publish them with
`git push`. Thoughts recorded through its MCP tools are the only durable
record of *why* something was done; PR bodies and commit messages are
not a substitute.

- **At the start of a task**, call `read_history` (after `sync_history`
  if the checkout may be behind) before assuming a blank slate.
- **After every meaningful step**, call `commit_thought`: a plan formed,
  a non-trivial decision, an approach rejected, a subtask finished. Do
  it *before* the commit the reasoning belongs to, so the post-commit
  hook can link it. One entry per decision; concise, not exhaustive.
- Reference the WBS task id (`T-x.y.z`) and the branch in the entry.
- When working in a git worktree, pass the worktree path as `cwd`.
- If a CVC tool reports missing storage or hooks, call `setup_cvc`.
- Publication to the remote is consent-gated (`cvc privacy status`);
  never change that setting without being asked.

PRs #58–#71 (September 2026) carry no thoughts because those sessions
never called `commit_thought`. Don't repeat that.

## Working pattern that this repository relies on

- Small feature branches off `main`, one PR each. Do not stack PRs: a
  stacked PR merges into its base branch, not `main`. Never delete a
  remote branch, merged or not; they are kept as a record.
- The project owner UATs and merges every PR. Never merge; verify a
  merge with `gh pr view <n> --json state,mergedAt,mergeCommit` before
  syncing `main`.
- Before pushing, the whole gate must be green locally: `cargo build`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo fmt --all --check`, `cargo nextest run --workspace`,
  `scripts/check-gpui-isolation.sh`,
  `scripts/check-gpui-component-facade.sh`, and a short smoke run of
  the binary when the UI changed. Watch CI on the PR and report it.
- Every WBS row touched gets its Status cell updated in
  `documentation/task.md`; deviations from an AC are written there and,
  when user-visible, in `documentation/known_issues.md`.
- The UI thread does no I/O (`duet_vfs::local::mark_ui_thread()` is
  armed in `duet_ui::run`); anything that reads the filesystem runs on
  the core Tokio runtime and comes back through a channel.
- `gpui_component::` may only be named inside `crates/duet-widgets`
  (R-G7); other crates must not depend on `gpui` outside `duet-ui` and
  `duet-widgets` (ADR-002). The lint scripts enforce both.
