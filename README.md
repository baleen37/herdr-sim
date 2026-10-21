# herdr-sim

A [Herdr](https://herdr.dev) plugin that marks each Claude Code or Codex pane with
the pull requests its session touched — open, merged, or closed — as icons in the
sidebar. It works across repositories: a session that opened PRs in two repos
shows both. Each Space (workspace) also gets the PR of the branch it has checked
out, like a branch row in a Git client.

It also adds a Changes panel (Claude Code's PR list in a right-hand split),
herdr-radar's workspace › worktree › session tree for the Agents list, and its
activity order. Everything is drawn through Herdr's sidebar tokens, so you
choose what shows in `config.toml`.

```
● search 
  SEARCH-14527 진행 상황
● search-data-batch 3
  배치 마이그레이션
```

## Install

Requires Herdr 0.9.0+, its Claude Code / Codex integrations
(`herdr integration install claude`, `... codex`), `gh` signed in, and a
[Nerd Font](https://www.nerdfonts.com/) for the icons.

```sh
herdr plugin install baleen37/herdr-sim
```

The install step downloads the prebuilt binary for the manifest's version from
GitHub Releases (macOS arm64, Linux x86_64/arm64) and checks its sha256. On
other platforms (Intel macOS, for one) build from source as below.

With Nix:

```sh
herdr plugin link "$(nix build github:baleen37/herdr-sim --no-link --print-out-paths)"
```

The flake exposes only `packages.<system>.default` (aarch64-darwin,
x86_64-linux, aarch64-linux), no overlay or module. Its output is the plugin
root: `bin/herdr-sim`, wrapped with `gh` on `PATH`, beside `herdr-plugin.toml`.

From source, put the binary where the manifest expects it and link the checkout:

```sh
cargo build --release
mkdir -p bin && cp target/release/herdr-sim bin/
herdr plugin link .
```

## Quick start

1. Install the plugin (above).
2. Add `$pr` to the Agent rows in Herdr's `config.toml`:

   ```toml
   [ui.sidebar.agents]
   rows = [
     ["state_icon", { token = "$heading" }],
     [{ token = "$pr", rules = [{ starts_with = "\uF407", fg = "#a6e3a1" }, { starts_with = "\uF419", fg = "#cba6f7" }, { starts_with = "\uF4DC", dim = true }] }],
   ]
   ```

3. Bind the Changes panel to a key (see [Changes panel](#changes-panel)).

[Sidebar config](#sidebar-config) has a fuller layout and every token.

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
once a minute too. When GitHub's GraphQL budget drops below 1000 points, or a
call is refused for the rate limit, every `gh` call stops until the budget
resets (or for 5 minutes), leaving the rest for your own `gh` and agents.

Herdr runs the plugin at server startup, whenever an agent's status changes,
when a pane is focused or closed, and when a workspace or worktree is created
or opened.

## Changes panel

Claude Code's "Changes" panel as a right-hand split.

- **Rows.** Above, a `± repo +12 -3` row for each checkout the session worked
  in that has uncommitted changes. Below, the PRs the session touched, newest
  first: state icon, title, repo and number, lines changed, comments, checks.
  An approved open PR shows a green check. Running checks show a pulsing grey
  dot and `8/11`.
- **Focus.** It starts on the pane the key was pressed in, then follows the
  focused agent in its own workspace.
- **Keys.** `j`/`k` move between rows, `gg`/`G` to the ends. `Enter` opens
  the row, `p` its PR, `c` its latest comment, `d` its diff.
- **Mouse.** A click opens what it is on: the repo, the PR, its files,
  comments, or checks. The wheel scrolls. A drag across rows copies their
  text.
- **Diff.** `d` swaps the panel for a diff on the right half of the tab, laid
  out like reviewr: the selected file's diff (line numbers, changed lines
  barred and tinted, highlighted for the file's language, CSV columns in
  colour) beside a tree of the changed files. Vim keys move (`j`/`k`,
  `Ctrl-d`/`Ctrl-u`, `gg`/`G`, `]`/`[` for hunks, `f`/`F` for files); `Tab`
  switches box, `z` hides the tree, `e` opens the file in `$EDITOR` at the
  line, `q` brings the panel back.
- **Freshness.** Open PRs are refetched every 30 seconds, merged and closed ones
  every 10 minutes, all in one GraphQL call. The last details are saved, so a
  reopened panel draws at once. Calls back off when GitHub's rate limit is
  near.

The panel opens about 42 columns wide and focused; the same key closes it.

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
row_gap = 0
rows = [
  [{ token = "$tree_group_parent", fg = "#8b8e9c", bold = true }],
  [{ token = "$tree_group", fg = "#a8abbd", bold = true }, { token = "$tree_group_stale", fg = "#8b8e9c", bold = true }],
  [{ token = "$label_working", fg = "#fab387", bold = true }, { token = "$label_blocked", fg = "#f38ba8", bold = true }, { token = "$label_done", fg = "#a6e3a1" }, { token = "$label_idle_fresh", fg = "#95bba2" }, { token = "$label_idle", fg = "#a99e92" }, { token = "$label_idle_stale", fg = "#8b8e9c" }, { token = "$label_unknown", fg = "#cba6f7" }, { token = "machine", fg = "#89b4fa", rules = [{ equals = "Local", hide = true }] }],
  [{ token = "$tree_gap" }],
]


[ui.sidebar.spaces]
row_gap = 0
rows = [
  [{ token = "$header_working", fg = "#fab387", bold = true }, { token = "$header_blocked", fg = "#f38ba8", bold = true }, { token = "$header_done", fg = "#a6e3a1", bold = true }, { token = "$header_idle", fg = "#cdd6f4", bold = true }, { token = "$header_unknown", fg = "#cba6f7", bold = true }, { token = "$header_none", fg = "#6c7086" }],
  [{ token = "$space_title", fg = "#a8abbd" }, { token = "$pr", rules = [{ starts_with = "\uF407", fg = "#a6e3a1" }, { starts_with = "\uF419", fg = "#cba6f7" }, { starts_with = "\uF4DC", hide = true }] }],
]
```

| Token | Value |
| --- | --- |
| `$pr` | All PRs as one icon, like Claude Code's session label: the icon of the state that most needs attention (open, then merged, then closed), then the total when there is more than one — ` 3`. Colour it with `starts_with` rules on the icon. |
| `$pr_open` | `` (git-pull-request), followed by a count when more than one (agents only) |
| `$pr_merged` | `` (git-merge) |
| `$pr_closed` | `` (git-pull-request-closed) |
| `$project` | The repository the pane or Space works in. A linked worktree names its main repository (`search`, not `search-14527-status`); a directory outside git is named after itself, `~` for home. |
| `$agent_claude`, `$agent_codex` | On workspaces: that agent's logo and name (` claude`) when it runs there, as in herdr-radar's Spaces list. Separate tokens so each can have its own colour. |
| `$branch` | On agent panes: the branch checked out where the agent works (Herdr's own `branch` token is for Space rows only). Unset outside git or on a detached HEAD. |
| `$tree_header` | A one-line header: `<project>  <branch icon> <branch>`. On the first agent of each checkout, and on every workspace, so the Agents list and the Spaces panel read alike. Use `agent_panel_sort = "spaces"`. |
| `$tree_project`, `$tree_worktree` | A project → checkout tree over the Agents list (structure after [herdr-radar](https://github.com/hhdebb/herdr-radar), layout after Orca): the repo icon and project name on the first agent of each project, `<branch icon> <branch>` on the first agent of each checkout, main checkout included. A project's later checkouts get `│` in `$tree_project` so every branch header sits at the same indent; style it with an `equals = "│"` rule. Use `agent_panel_sort = "spaces"`. |
| `$age` | On agent panes: how long ago the session last had a message (the latest `timestamp` of a user/assistant record in the Claude transcript, or of a response item in the Codex rollout; file times are not used, since Claude Code rewrites old records) — `now`, `10m`…`50m`, `1h`…, `1d`…. Coarse so it changes at most six times an hour; refreshed on every pass, so it pauses while no agent has events. |
| `$heading_working`, `$heading_blocked`, `$heading_done`, `$heading_idle_fresh`, `$heading_idle`, `$heading_idle_stale`, `$heading_unknown` | `$heading` in the one token matching the agent's state, so titles can be coloured by state as in herdr-radar. Idle splits by session age: under 15 minutes fresh, over 2 hours stale. As in herdr-radar, a finished turn keeps `$heading_done` until you focus the pane (Herdr turns `done` into `idle` within seconds), and a question keeps `$heading_blocked` until the agent works again. Each starts with the state's mark — `◐` working, `✓` done, `?` asking, `○` idle, none when unknown — so leave Herdr's `state_icon` out of that row. Updated on every status change, including a turn's start (one `herdr agent list`, then only changed tokens). |
| `$icon_claude`, `$icon_codex`, `$icon_stale` | The logo alone, as in herdr-radar: in its own token so it keeps the vendor's colour while the title beside it, in `$label_*`, takes the state's. `$icon_stale` replaces the other two on a session idle over 2 hours, so the whole row can dim. Optional: the row's indent in the `$tree_group` tree leads `$label_*`. |
| `$label_working`, `$label_blocked`, `$label_done`, `$label_idle_fresh`, `$label_idle`, `$label_idle_stale`, `$label_unknown` | `$heading_*` without the logo: the state's mark, then the title (`✓ Fix login`). For rows that start with `$icon_*`. While an agent works, a braille spinner turns in place of `◐`, and while it asks, the `?` pulses, as in herdr-radar: a short-lived `--animate` process, started when an agent works or asks, rewrites `$label_working` / `$label_blocked` (and a Space's `$header_working` / `$header_blocked`) over Herdr's socket every 150 ms, and exits once none does. Leads with the row's indent in the `$tree_group` tree. |
| `$heading` | On agent panes: the logo and the title as one value (` Fix login`), or the logo alone before there is a title. Herdr puts ` · ` between tokens, so one token reads like a line in Claude Code's own session list; the cost is one colour for both. |
| `$tree_group`, `$tree_group_stale`, `$tree_group_parent`, `$tree_gap` | [herdr-radar](https://github.com/hhdebb/herdr-radar)'s tree over the Agents list, a group per Herdr workspace. `$tree_group` is the workspace's name on its first session, `$tree_group_stale` instead once every session in it is idle over two hours. A linked git worktree (Herdr's `worktree` on the workspace) hangs under its main checkout's group as `├─  <name>` / `└─  <name>`; when the checkout has no session on the list, `$tree_group_parent` puts the repository's name on a row of its own above it. In Herdr's own order a worktree not right under its family loses the corner. `$tree_gap` is a zero-width space on a group's last session (none between a checkout and its worktrees), so a row holding only it reads as a blank line. Each session row is indented to sit under its header, the indent leading `$label_*`, so leave Herdr's `state_icon` out of that row. Put the rows in that order — parent, group, session, gap — with `agent_panel_sort = "spaces"`. |
| `$sort_rank` | The agent's position in the activity order below; not meant for display. |
| `$diff` | On workspaces: uncommitted changes in the checkout, `+12 -3` (`git diff --shortstat HEAD`; untracked files are not counted). Unset when clean. |
| `$ci` | On workspaces: the checks of the branch's open PR as one icon: `` passed, `` failed, `` still running. Fetched with the PR state in the same `gh` call. |
| `$title` | The session's title on agent panes: for Claude Code the latest `/rename` (`custom-title`), else the latest generated `ai-title`; for Codex the latest `thread_name` in `~/.codex/session_index.jsonl`. Until Claude Code titles the session, its first real question (not a slash command or command output, cut to 60 characters) stands in; unset before that. Unlike the terminal title, it is right for resumed sessions and for Codex, which sets none. |
| `$header_working`, `$header_blocked`, `$header_done`, `$header_idle`, `$header_unknown`, `$header_none` | On workspaces: the workspace's name behind the state's mark, in the one token for the state of the Space's agents — whichever most needs you: a question, then work, a finished turn (held until seen, as on agent rows), idle — or `$header_none` (`◦`) without an agent. So the Spaces list colours like the Agents list; leave Herdr's `state_icon` out of that row. A working Space's mark turns with the spinner. |
| `$space_title`, `$space_age` | On workspaces: the title of the session behind the Space's state (the one whose state its `$header_*` shows), and how long ago it last had a message, as `$age`. So a Space's second row reads like an Agents row. While the Space works the title moves to `$space_title_working` behind a spinner, the loading on the session's line as in the Agents list, and the name above keeps the still `◐`; put both in the row. (`$space_title_text` holds the bare title for the animator.) |
| `$space_pr`, `$group_pr` | The PRs a workspace's sessions touched (as the Changes panel lists them, each once) as `$pr`'s one icon, plus the count when more than one: `$space_pr` on the Space, `$group_pr` on the agent heading its group in the Agents list. Put it last in the header's row: when `ui.sidebar_width` is set, the header is padded with braille blanks so the icon lands at the row's right end (and cut with `…` when too long). |
| `$tree_header_idle` | `$tree_header` on a Space with no agent in it, in its own token so it can be dimmed. |
| `$agent_icon` | `` (cod-claude) for Claude Code, `` (cod-openai) for Codex. Agents only; needs a Nerd Font with Codicons 0.0.36+. |

A token with no PRs is cleared, so its separator disappears too.

## Activity order

Off by default: the Agents list keeps Herdr's own sort (`agent_panel_sort`,
for example `priority`, which puts sessions waiting for you first). Turned
on, the plugin orders the Agents list in herdr-radar's tree
([herdr-radar](https://github.com/hhdebb/herdr-radar)), what needs you first:
a question, then a finished turn not yet seen, then work, then idle; within
each, by when the session last had a message. A worktree family (a checkout
and its linked worktrees) ranks by its most urgent session, the checkout
first; within it each workspace the same way; then each tab as one unit, the
pane it was split from first; then each pane. Sessions that never worked come
last. The tree headers follow that order. Rows move when a turn ends or asks
for input, not when one starts.

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

## Development

`dev/session.sh` builds this checkout and opens a named Herdr session,
`sim-dev`, whose plugin hooks run that build (`HERDR_SIM_BIN`, which every
manifest command honours) and whose config is `~/.config/herdr/dev.toml` (a
copy of the live one, made on first run). The default session keeps the
installed version and its own config. The server reads both paths when it
starts, so `herdr session stop sim-dev` before a run that changes them; a
rebuilt binary is picked up at the next event.

Each named session keeps its plugin state under `sessions/<name>` of the
plugin's state directory, since pane ids repeat across sessions.

`dev/sidebar.py [--color] [--session sim-dev]` prints the sidebar as the
running server draws it, to check a change without looking at the screen.

## License

MIT or Apache-2.0, at your option.
