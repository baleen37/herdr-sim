# herdr-sim

A [Herdr](https://herdr.dev) plugin that marks each Claude Code or Codex pane with
the pull requests its session touched — open, merged, or closed — as icons in the
sidebar. It works across repositories: a session that opened PRs in two repos
shows both. Each Space (workspace) also gets the PR of the branch it has checked
out, like a branch row in a Git client.

```
● search 
  SEARCH-14527 진행 상황
● search-data-batch 3
  배치 마이그레이션
```

## How it finds PRs

| Agent | Source |
| --- | --- |
| Claude Code | (A pane whose conversation was parked as a background job is followed to that job's session.) `pr-link` records in the session transcript (`~/.claude/projects/*/<session>.jsonl`). Claude Code writes one when the session creates, pushes to, edits, or comments on a PR. |
| Codex | PR URLs printed by `gh pr create/edit/comment/ready/close/merge/checkout` in the session rollout (`~/.codex/sessions/**/rollout-*<session>.jsonl`). |

A Space's PR is the one `gh pr view <branch>` finds for the branch checked out
in its first pane that is inside a git repository.

## Load

A finished turn only revisits its own pane and workspace: five short process
calls when nothing changed (`herdr agent list`, `herdr pane list`, one
`git rev-parse` each for the pane and its Space, and `git diff --shortstat` for
the Space), and no `report-metadata` call unless a token changed. A started turn only recolours headings by state (one `herdr agent list`). Runs never overlap: one that finds another in
progress leaves a note, and the running one does a single extra pass. `gh` calls
are cached as below and killed after 10 seconds.

States come from `gh pr view`. Open PRs are re-checked at most once a minute;
merged and closed PRs are never re-checked. A branch without a PR is re-checked
once a minute too. Herdr runs the plugin at server
startup and whenever an agent's status changes.

## Install

Requires Herdr 0.9.0+, its Claude Code / Codex integrations
(`herdr integration install claude`, `... codex`), `gh` signed in, and a
[Nerd Font](https://www.nerdfonts.com/) for the icons.

```sh
herdr plugin install baleen37/herdr-sim
```

The install step downloads the prebuilt binary for the manifest's version from
GitHub Releases (macOS arm64, Linux x86_64/arm64) and checks its sha256.

With Nix:

```sh
herdr plugin link "$(nix build github:baleen37/herdr-sim --no-link --print-out-paths)"
```

From source, put the binary where the manifest expects it and link the checkout:

```sh
cargo build --release
mkdir -p bin && cp target/release/herdr-sim bin/
herdr plugin link .
```

## Changes panel

Claude Code's "Changes" panel as a right-hand split: the PRs the focused agent's
session touched, newest first — state icon, title (a link), repo and number,
lines changed, comments and checks. It starts on the pane the key was pressed
in and then follows focus between agent panes. Open PRs are refetched every 30
seconds while it is open, merged and closed ones every 10 minutes.

```toml
[[keys.command]]
key = "cmd+p"
type = "plugin_action"
command = "baleen37.herdr-sim.toggle-changes"
description = "changes panel on/off"
```

## Sidebar config

The plugin only sets tokens, on agent panes and on workspaces; add them to the
Agent and Space rows in Herdr's `config.toml`:

```toml
[ui]
agent_panel_sort = "spaces"

[ui.sidebar.agents]
rows = [
  [{ token = "$tree_header", fg = "#9399b2", bold = true }],
  [{ token = "$tree_split", dim = true }, "state_icon", { token = "$heading_working", fg = "#fab387", bold = true }, { token = "$heading_blocked", fg = "#f38ba8", bold = true }, { token = "$heading_done", fg = "#a6e3a1" }, { token = "$heading_idle_fresh", fg = "#cdd6f4" }, { token = "$heading_idle", fg = "#a6adc8" }, { token = "$heading_idle_stale", fg = "#6c7086" }, { token = "$heading_unknown", fg = "#cba6f7" }],
  [{ token = "$age", dim = true }, { token = "$pr", rules = [{ starts_with = "\uF407", fg = "#a6e3a1" }, { starts_with = "\uF419", fg = "#cba6f7" }, { starts_with = "\uF4DC", dim = true }] }],
]

[ui.sidebar.spaces]
rows = [
  ["state_icon", { token = "$tree_header", fg = "#9399b2", bold = true }, { token = "$agent_claude", fg = "#d97757" }, { token = "$agent_codex", fg = "#cdd6f4" }],
  ["git_status", { token = "$diff", dim = true }, { token = "$ci", rules = [{ starts_with = "\uF42E", fg = "#a6e3a1" }, { starts_with = "\uF467", fg = "#f38ba8" }, { starts_with = "\uF444", fg = "#f9e2af" }] }, { token = "$pr", rules = [{ starts_with = "\uF407", fg = "#a6e3a1" }, { starts_with = "\uF419", fg = "#cba6f7" }, { starts_with = "\uF4DC", dim = true }] }],
]
```

| Token | Value |
| --- | --- |
| `$pr` | All PRs as one icon, like Claude Code's session label: the icon of the state that most needs attention (open, then merged, then closed), then the total when there is more than one — ` 3`. Colour it with `starts_with` rules on the icon. |
| `$pr_open` | `` (git-pull-request), followed by a count when more than one (agents only) |
| `$pr_merged` | `` (git-merge) |
| `$pr_closed` | `` (git-pull-request-closed) |
| `$project` | The repository the pane or Space works in. A linked worktree names its main repository (`search`, not `search-14527-status`); a directory outside git is named after itself, `~` for home. |
| `$agent_claude`, `$agent_codex` | On workspaces: that agent's logo when it runs there, plus the count when more than one does (` 2`). Separate tokens so each can have its own colour. |
| `$branch` | On agent panes: the branch checked out where the agent works (Herdr's own `branch` token is for Space rows only). Unset outside git or on a detached HEAD. |
| `$tree_header` | A one-line header: `<project>  <branch icon> <branch>`. On the first agent of each checkout, and on every workspace, so the Agents list and the Spaces panel read alike. Use `agent_panel_sort = "spaces"`. |
| `$tree_pr` | The PR state icon of a checkout's branch, on the same agent as `$tree_worktree`, the way Orca puts the PR on the worktree card. Shares the branch PR cache with the Space rows, so it costs no extra `gh` call. |
| `$tree_project`, `$tree_worktree` | A project → checkout tree over the Agents list (structure after [herdr-radar](https://github.com/hhdebb/herdr-radar), layout after Orca): the repo icon and project name on the first agent of each project, `<branch icon> <branch>` on the first agent of each checkout, main checkout included. A project's later checkouts get `│` in `$tree_project` so every branch header sits at the same indent; style it with an `equals = "│"` rule. Use `agent_panel_sort = "spaces"`. |
| `$age` | On agent panes: how long ago the session last had a message (the latest `timestamp` of a user/assistant record in the Claude transcript, or of a response item in the Codex rollout; file times are not used, since Claude Code rewrites old records) — `now`, `10m`…`50m`, `1h`…, `1d`…. Coarse so it changes at most six times an hour; refreshed on every pass, so it pauses while no agent has events. |
| `$heading_working`, `$heading_blocked`, `$heading_done`, `$heading_idle_fresh`, `$heading_idle`, `$heading_idle_stale`, `$heading_unknown` | `$heading` in the one token matching the agent's state, so titles can be coloured by state as in herdr-radar. Idle splits by session age: under 15 minutes fresh, over 2 hours stale. As in herdr-radar, a finished turn keeps `$heading_done` until you focus the pane (Herdr turns `done` into `idle` within seconds), and a question keeps `$heading_blocked` until the agent works again. Those two also get herdr-radar's mark between logo and title: `✓` and `?`. Updated on every status change, including a turn's start (one `herdr agent list`, then only changed tokens). |
| `$heading` | On agent panes: the logo and the title as one value (` Fix login`), or the logo alone before there is a title. Herdr puts ` · ` between tokens, so one token reads like a line in Claude Code's own session list; the cost is one colour for both. |
| `$tree_group_project`, `$tree_group`, `$tree_gap` | A project › branch › session tree after [herdr-radar](https://github.com/hhdebb/herdr-radar), one line per session. `$tree_group_project` is the project name on its first agent; `$tree_group` is `├─  <branch>` (`└─` for the project's last) on each checkout's first agent; `$tree_gap` is a zero-width space on a project's last agent, so a row holding only it reads as a blank line between projects. A header whose sessions have all gone stale moves to `$tree_group_project_stale` / `$tree_group_stale`, so it can fade with them. Session rows are indented to sit under their branch, so leave Herdr's `state_icon` out of that row: the indent rides on its first cell. Put the rows in that order — project, branch, session, gap — and use `agent_panel_sort = "spaces"`. |
| `$tree_split` | `├─` on the second and later agents of a split tab, `└─` on the last, so a split screen reads as one unit under its first agent. |
| `$sort_rank` | The agent's position in the activity order below; not meant for display. |
| `$diff` | On workspaces: uncommitted changes in the checkout, `+12 -3` (`git diff --shortstat HEAD`; untracked files are not counted). Unset when clean. |
| `$ci` | On workspaces: the checks of the branch's open PR as one icon: `` passed, `` failed, `` still running. Fetched with the PR state in the same `gh` call. |
| `$title` | The session's title on agent panes: for Claude Code the latest `/rename` (`custom-title`), else the latest generated `ai-title`; for Codex the latest `thread_name` in `~/.codex/session_index.jsonl`. Until Claude Code titles the session, its first real question (not a slash command or command output, cut to 60 characters) stands in; unset before that. Unlike the terminal title, it is right for resumed sessions and for Codex, which sets none. |
| `$agent_icon` | `` (cod-claude) for Claude Code, `` (cod-openai) for Codex. Agents only; needs a Nerd Font with Codicons 0.0.36+. |

A token with no PRs is cleared, so its separator disappears too.

## Activity order

By default the plugin orders the Agents list by activity, the way
[herdr-radar](https://github.com/hhdebb/herdr-radar) does: the project whose
agent changed state most recently comes first, then within it its checkouts
(main checkout and worktrees alike) and their split tabs, each by their latest
change. A tab whose sessions have all gone stale sinks to the bottom of its checkout. The tree
headers follow that order. Agents move when a turn ends or asks for input, not
when one starts.

Herdr turns off its own order toggle while a plugin's order is on. Switch back
and forth with the `toggle-order` action, for example on a key:

```toml
[[keys.command]]
key = "prefix+a"
type = "plugin_action"
command = "baleen37.herdr-sim.toggle-order"
description = "agents: activity order on/off"
```

Only one plugin can order the Agents list at a time, so turn this off if
herdr-radar's order is on.

## Caveats

- Claude Code's `pr-link` record is not a documented API and may change.
- A PR a session only inspected can still show up: in testing, Claude Code linked
  a PR whose URL appeared in the output of read-only commands.

## License

MIT or Apache-2.0, at your option.
