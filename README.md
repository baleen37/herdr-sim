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

A finished turn only revisits its own pane and workspace: four short process
calls when nothing changed (`herdr agent list`, `herdr pane list`, and one
`git rev-parse` each for the pane and its Space), and no `report-metadata` call
unless a token changed. A
started turn exits at once. Runs never overlap: one that finds another in
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

## Sidebar config

The plugin only sets tokens, on agent panes and on workspaces; add them to the
Agent and Space rows in Herdr's `config.toml`:

```toml
[ui]
agent_panel_sort = "spaces"

[ui.sidebar.agents]
rows = [
  [{ token = "$tree_header", fg = "#cdd6f4", bold = true }],
  ["state_icon", { token = "$heading", fg = "#cdd6f4" }],
  [{ token = "$age", dim = true }, { token = "$pr", rules = [{ starts_with = "\uF407", fg = "#a6e3a1" }, { starts_with = "\uF419", fg = "#cba6f7" }, { starts_with = "\uF4DC", dim = true }] }],
]

[ui.sidebar.spaces]
rows = [
  ["state_icon", { token = "$project", bold = true }, { token = "$agent_claude", fg = "#d97757" }, { token = "$agent_codex", fg = "#cdd6f4" }],
  ["branch", "git_status", { token = "$pr", rules = [{ starts_with = "\uF407", fg = "#a6e3a1" }, { starts_with = "\uF419", fg = "#cba6f7" }, { starts_with = "\uF4DC", dim = true }] }],
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
| `$tree_header` | A one-line header per checkout, on its first agent: `<repo icon> <project>  <branch icon> <branch>`. Every header is an entry's first row, so a flat two-level list lines up without spacer rows. Use `agent_panel_sort = "spaces"`. |
| `$tree_project`, `$tree_worktree` | A project → checkout tree over the Agents list (structure after [herdr-radar](https://github.com/hhdebb/herdr-radar), layout after Orca): the repo icon and project name on the first agent of each project, `<branch icon> <branch>` on the first agent of each checkout, main checkout included. A project's later checkouts get `│` in `$tree_project` so every branch header sits at the same indent; style it with an `equals = "│"` rule. Use `agent_panel_sort = "spaces"`. |
| `$age` | On agent panes: how long ago the session's transcript (Claude) or rollout (Codex) last changed — `now`, `10m`…`50m`, `1h`…, `1d`…. Coarse so it changes at most six times an hour; refreshed on every pass, so it pauses while no agent has events. |
| `$heading` | On agent panes: the logo and the title as one value (` Fix login`), or the agent's name (Claude, Codex) before there is a title. Herdr puts ` · ` between tokens, so one token reads like a line in Claude Code's own session list; the cost is one colour for both. |
| `$title` | The session's title on agent panes: for Claude Code the latest `/rename` (`custom-title`), else the latest generated `ai-title`; for Codex the latest `thread_name` in `~/.codex/session_index.jsonl`. Unset until the agent has titled the session. Unlike the terminal title, it is right for resumed sessions and for Codex, which sets none. |
| `$agent_icon` | `` (cod-claude) for Claude Code, `` (cod-openai) for Codex. Agents only; needs a Nerd Font with Codicons 0.0.36+. |

A token with no PRs is cleared, so its separator disappears too.

## Caveats

- Claude Code's `pr-link` record is not a documented API and may change.
- A PR a session only inspected can still show up: in testing, Claude Code linked
  a PR whose URL appeared in the output of read-only commands.

## License

MIT or Apache-2.0, at your option.
