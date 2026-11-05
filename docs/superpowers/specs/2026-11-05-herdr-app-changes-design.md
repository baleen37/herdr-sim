# Herdr app: herdr-sim's Changes panel in place of the app's own

## Intent

The person uses herdr-sim's Changes panel in the terminal Herdr mostly to
**open a PR** and to **read a diff with `d`**. They want the same in the Herdr
desktop app (`herdr-app`), shaped like the Claude Code desktop app's changes
view: a side panel listing what the focused agent's session changed, and a
wide, keyboard-driven diff beside the terminal.

Today's `sim-prs` panel (branch PR + session PRs, built on
`herdr-sim --prs-json`) is the starting point. This grows it into a Changes
panel and swaps it in for the app's shipped Changes panel.

## Goals

1. One panel, two sections, following the focused terminal:
   - **Changes**: one row per checkout the session worked in that has
     uncommitted changes, as herdr-sim's Changes pane does (`± <repo> +12 −3`;
     a linked worktree's row names its branch). One line per checkout, not
     per file.
   - **Pull requests**: the branch's PR and the session's PRs, as now.
2. **Open a PR**: clicking a PR row opens it in the browser (as now).
3. **Diff, wide and keyboard-driven**: a Changes row, and a PR row's diff
   action, open herdr-sim's existing diff view (`--diff-pane`) in a terminal
   split to the right of the focused terminal. Its keys work as in the
   terminal Herdr: `j`/`k`, `Ctrl-d`/`Ctrl-u`, `gg`/`G`, `]`/`[`, `f`/`F`,
   `Tab`, `z`, `e`, `q`.
4. **Replace the shipped Changes**: in the `main-sim` layout, the tab bar's
   Changes button (the shipped `tabs` mod's) opens this panel. The separate
   PRs button goes.
5. **`cmd-shift-c` toggles the panel** while `main-sim` shows: closed, or open
   on another child (Files) → open on Changes; open on Changes → close.
6. **The diff takes the panel's room**, as in the terminal Herdr: opening a
   diff closes the side panel; leaving the diff (`q`, or its pane closing
   any other way) opens the panel again on Changes.

## Non-goals

- Rebuilding the diff view natively in the app (mouse-first). The person chose
  the keyboard; the Rust diff view is reused as is.
- Per-file rows in the panel, or opening the diff at a given file.
- Committing from the panel (the shipped Changes panel's commit box).
- Codex sessions and terminals on other machines (unchanged: branch PR only,
  or nothing).
- Changing the shipped `tabs` mod or the `main` layout.

## Anti-goals

- A diff that opens but ignores the keyboard, or needs the mouse to leave.
- `e` silently doing nothing in the app (today it calls the terminal Herdr's
  pane API, which the app doesn't have).
- Touching the user's other layouts or shared state beyond what the panel
  needs (`workspace.panels` is shared by every layout).

## Constraints (found while building sim-prs; see herdr-app/RECIPE.md)

- Herdr app mods are JavaScript; ours is TypeScript in `herdr-app/src/`,
  built to `herdr-app/mods/sim-prs/` and committed.
- From a mod, `hc get` never returns; mod `fs` only reaches its own folder;
  `init`'s first argument isn't the block's props (`props()` is); a linked
  mod reloads only on a layout switch.
- `pane.split` takes `argv` but no environment: pass the diff source through
  the command, e.g. `["env", "HERDR_SIM_DIFF_SOURCE=pr:<url>", "herdr-sim", "--diff-pane"]`
  (sources are `pr:<url>` and `checkout:<top>`, `diff::SOURCE`).
- `--diff-pane` sizes itself (`fit_half`) and returns to the Changes pane on
  `q` (`reopen`) only when `HERDR_PANE_ID` / `HERDR_SIM_CHANGES_FOR` are set,
  i.e. in the terminal Herdr. In the app those are unset: it doesn't resize,
  and `q` just exits, which closes the pane. Mouse input is plain terminal
  mouse reporting and works.
- herdr-sim fails on unknown flags; `--diff-pane` is a known one.

## Decided

- **Panel content** comes from `herdr-sim --prs-json`, extended with a
  `changes` list: for each checkout among the session's working directories
  (and the terminal's own folder) that has uncommitted changes,
  `{ top, repo, branch?, added, removed }` (or the Changes pane's `+a −d`
  text). Reuse the Changes pane's `checkout_of`, `local_diff`,
  `worktree_branch`, `repo_name`. The terminal's folder is already passed as
  `--cwd`; the session's directories come from `claude_session(...).dirs`.
- **Rows and actions**:
  - Changes row click → diff of `checkout:<top>`.
  - PR row click → browser (unchanged); a separate diff control on the row
    (an icon or "diff" label on hover) → diff of `pr:<url>`.
- **Where the diff opens**: `layout.hide {id: "panel"}`, then `pane.split` to
  the right of the focused terminal's pane, focused, so the keys go to it at
  once. `q` exits it, which closes the pane.
- **Coming back** (the person's call): the panel remembers the diff pane it
  opened (from `pane.split`'s result, or by finding the new pane in `panes()`)
  and, once that pane is gone from `panes()`, runs `layout.select {stack:
  "panel-body", child: "changes"}` and `layout.show {id: "panel"}`. While the
  panel is hidden its copy stays alive (AGENTS.md: a hidden block's copy keeps
  running), and `render` reruns on live-data changes such as `panes()`; if
  that proves untrue, the always-present tab bar block below can watch
  instead.
- **`e` in the app**: when `HERDR_PANE_ID` is unset, run the editor inline in
  the diff's own terminal (leave the alternate screen, run `$VISUAL`/`$EDITOR`
  as `edit_pane` resolves it, wait, re-enter and re-read the diff), instead of
  asking the terminal Herdr for a zoomed pane.
- **Replacing Changes**: in `layouts/main-sim.json`, the `panel-body` stack's
  child with id `changes` gets `content: { mod: "sim-prs" }`; the shipped
  Changes button (which selects child `changes`) then shows this panel. Drop
  the `sim-prs` child; the `sim-prs-button` block becomes the 0 px key block
  above.
- **Shared state** (my call): the old button added `"sim-prs"` to the shared
  `workspace.panels`; the new mod removes that one entry once (it put it
  there; left in, the tab bar tries to restore a child that no longer exists).
  It adds nothing: `changes` is already there.
- **Upgrading the installed layout** (my call): `scripts/install.sh` replaces
  an installed `main-sim.json` only when it equals a version this repo
  committed (`git log -p` / `git show <rev>:herdr-app/layouts/main-sim.json`),
  so a person's edits are never lost; otherwise it keeps it and says how to
  diff. On this machine the installed copy is an earlier committed version.
- **Toggle key**: `keys.json` section `layout:main-sim`,
  `"cmd-shift-c": "sim-prs.toggle"`. A `<mod>.<command>` reaches shown copies
  only, and a copy starts only once shown (AGENTS.md), so the hidden panel
  can't receive it. My call: keep a `sim-prs` block in the tab bar row,
  `size: { px: 0 }`, `props: { keys: true }`, drawing nothing; it is always
  shown and handles `toggle` from `nextCommand()` (open on `changes`, or hide).
  `scripts/install.sh` merges the key without replacing one the person
  already bound.

## Left to the builder

- The JSON field names and how `--prs-json` is renamed, if at all (a new flag
  such as `--changes-json` is fine; keep `--prs-json` working or drop it in
  the same change, since the only caller is this mod).
- Row styling, the diff control's look, empty states' wording.
- Whether the panel title reads "Changes" (it replaces the app's Changes; my
  call: "Changes").
- Tests: unit tests for the new Rust pieces, scenario cards in
  `herdr-app/scenarios/` updated for the new rows, the diff split and the key.
- Updating README, RECIPE and the layout preview SVG.
