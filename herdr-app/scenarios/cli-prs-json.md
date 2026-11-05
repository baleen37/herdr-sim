# cli-prs-json: `herdr-sim --prs-json` finds a session's PRs and a branch's PR

**What this covers**: `--prs-json` (`changes::app_json`), `claude_session_for_pid`,
the strict flag check in `main`, the app cache (incremental transcript scan,
branch PR via `Cache::branch_state`) and the details cache (`fresh`, failed
fetches). If the panel's data is wrong, one of these steps catches it.

## Pre-state
- A release build of this checkout: `cargo build --release`; `B=target/release/herdr-sim`.
- `gh` signed in (`gh auth status`); network to github.com.
- A hermetic home and state dir, so nothing real is read or written:
  ```sh
  T=$(mktemp -d); export GH_TOKEN=$(gh auth token) HOME=$T/home HERDR_PLUGIN_STATE_DIR=$T/state
  ```
- A fake Claude Code process file and transcript (`s1` touched #58, then #61):
  `$HOME/.claude/sessions/4242.json` = `{"sessionId":"s1"}` and
  `$HOME/.claude/projects/p/s1.jsonl` with two `pr-link` records for
  `https://github.com/baleen37/herdr-sim/pull/58` then `/61`, plus a half-written line.

## Steps
1. Unknown flag: `$B --nope`.
2. Nothing to look at: `$B --prs-json`.
3. The session: `$B --prs-json --pid 4242`.
4. Cached details: run step 3 again with `gh` off `PATH`.
5. New work: append a `pr-link` for `/60` to the transcript; `$B --prs-json --pid 4242`.
6. A parked conversation: `4343.json` = `{"sessionId":"pane","parkedJobId":"j1"}`,
   `.claude/jobs/j1/state.json` = `{"sessionId":"s1"}`; `$B --prs-json --pid 4343`.
7. Not a pid: `$B --prs-json --pid ../4242`.
8. A PR the token can't see: a session `s2` linking `https://github.com/baleen37/no-such-repo-xyz/pull/1`.
9. Branch PR: a worktree of this repo on `fix/sidebar-session-state` (PR #61);
   `$B --prs-json --cwd <worktree>`, then a folder outside git, then a detached HEAD.

## Expected
1. Exit 1, stderr `unknown flag --nope`. Fails if it exits 0 (a plugin pass ran).
2. `{"session":null,"branch":null,"prs":[]}`.
3. `session` `s1`; `prs` numbers `["61","58"]` (newest first), both `MERGED` with
   titles. Fails on the other order, a missing title, or the half line crashing it.
4. Same output as 3 (no `gh` needed). Fails if `prs` empties: the cache didn't hold.
5. `prs` numbers `["60","61","58"]`. Fails if #60 is missing (the scan didn't
   resume) or appears twice.
6. Same `prs` as 5. Fails with `session` `pane` or no PRs.
7. `session` null.
8. `prs` empty, and `app-prs.json` holds the URL with an `at` and no `title`.
9. Worktree: `branch.url` `https://github.com/baleen37/herdr-sim/pull/61`,
   `MERGED`. Outside git and detached: `branch` null.

## Cleanup
`git worktree remove --force <worktree>`, delete its branch, `rm -rf $T`.

## Sharp edges
- With the token in the macOS keychain, `gh` finds it through `~/Library`: overriding `HOME` logs it out even with `GH_CONFIG_DIR` set. Pass `GH_TOKEN` (`gh auth token`, read before `HOME` changes).
- A failed first fetch is recorded and waits 10 minutes (step 8's rule): if step 3 fails, steps 4–6 fail too without retrying. Fix the cause and start from a fresh `$T`.
- Step 9's worktree branch must keep the PR's head name (`git worktree add -b fix/sidebar-session-state`): `gh pr view <branch>` looks the PR up by that name, as the sidebar does.
- Step 4 must strip only `gh`'s directory from `PATH`, not `git`'s, or step 9's
  checkout lookup fails for an unrelated reason.
