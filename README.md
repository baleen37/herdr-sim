# herdr-sim

A [Herdr](https://herdr.dev) plugin that marks each Claude Code or Codex pane with
the pull requests its session touched — open, merged, or closed — as icons in the
sidebar. It works across repositories: a session that opened PRs in two repos
shows both.

```
● search 
  SEARCH-14527 진행 상황
● search-data-batch 3
  배치 마이그레이션
```

## How it finds PRs

| Agent | Source |
| --- | --- |
| Claude Code | `pr-link` records in the session transcript (`~/.claude/projects/*/<session>.jsonl`). Claude Code writes one when the session creates, pushes to, edits, or comments on a PR. |
| Codex | PR URLs printed by `gh pr create/edit/comment/ready/close/merge/checkout` in the session rollout (`~/.codex/sessions/**/rollout-*<session>.jsonl`). |

States come from `gh pr view`. Open PRs are re-checked at most once a minute;
merged and closed PRs are never re-checked. Herdr runs the plugin at server
startup and whenever an agent's status changes.

## Install

Requires Herdr 0.9.0+, its Claude Code / Codex integrations
(`herdr integration install claude`, `... codex`), `gh` signed in, and a
[Nerd Font](https://www.nerdfonts.com/) for the icons.

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

The plugin only sets tokens; add them to an Agent row in Herdr's `config.toml`:

```toml
[ui.sidebar.agents]
rows = [
  ["state_icon", { token = "workspace", bold = true }, { token = "$pr_open", fg = "#a6e3a1" }, { token = "$pr_merged", fg = "#cba6f7" }, { token = "$pr_closed", dim = true }],
  ["terminal_title_stripped"],
]
```

| Token | Value |
| --- | --- |
| `$pr_open` | `` (git-pull-request), followed by a count when more than one |
| `$pr_merged` | `` (git-merge) |
| `$pr_closed` | `` (git-pull-request-closed) |

A token with no PRs is cleared, so its separator disappears too.

## Caveats

- Claude Code's `pr-link` record is not a documented API and may change.
- A PR a session only inspected can still show up: in testing, Claude Code linked
  a PR whose URL appeared in the output of read-only commands.

## License

MIT or Apache-2.0, at your option.
