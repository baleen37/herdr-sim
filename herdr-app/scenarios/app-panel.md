# app-panel: the Changes panel follows focus, opens diffs wide and comes back

**What this covers**: `src/main.ts` as Herdr runs it in `herdr-sim`: the panel
in the `changes` child (`showing`), the 0px keys block (`sim-prs.toggle`,
`watchDiff`), refreshing on focus change and on coming back into view,
staying idle while hidden, the Uncommitted rows, opening a diff
(`openDiff`) and the panel's return, and the error row for a missing or
too-old herdr-sim. The data itself is `cli-prs-json.md`'s.

## Pre-state
- Herdr running with `herdr-sim` shown and its window drawing (not covered:
  `herdr-app shot` returns the last drawn frame; take two and compare).
- `herdr-app check ~/.config/herdr-app/mods/sim-prs` passes; `herdr-sim` on
  `PATH` is this checkout's release build.
- A worktree of this repo on `fix/sidebar-session-state` (PR #61, merged) at
  `$W`, with one uncommitted edit (`echo x >> $W/README.md`).
- Note the focused terminal (`herdr-app status` → `agent`) to come back to.
- Layout state, the ground truth for open/closed:
  `layouts/.state/herdr-sim.json` → `nodes.panel.hidden`, `nodes.panel-body.active`.

## Steps
1. Toggle: `herdr-app do sim-prs.toggle` three times, from open on Changes; then
   `layout.select {stack: panel-body, child: files}` and toggle once more.
2. New tab in the worktree: `herdr-app do tab.new --folder $W`; wait 5 s; shot.
3. Click the Uncommitted row (by hand); shot; in the diff press `j`, then `q`; wait 2 s; shot.
4. Click #61's Diff (by hand); shot; `q`.
5. Click #61's title: the browser opens the PR.
6. In a diff of `$W`, press `e`; shot; quit the editor; shot.
7. Back to the first terminal; hide the panel and compare
   `$TMPDIR/herdr-sim/app-cache.json`'s mtime over 25 s; show it and wait 5 s.
8. Move `~/.local/bin/herdr-sim` aside, wait 12 s, shot; then a script there that
   prints nothing; wait 12 s, shot; restore the link.

## Expected
1. hidden/active: closed → open on `changes` → closed; from Files → open on `changes`.
   Fails if a toggle opens Files or does nothing.
2. "Uncommitted": `± herdr-sim` with `+1` (the worktree's edit); "This branch":
   `Merged` #61 with a Diff button.
3. The panel closes and a diff of the worktree (`· uncommitted`, README.md)
   fills a split right of the terminal; `j` moves; after `q` the split is gone
   and the panel is open on Changes again. Fails if the panel stays closed.
4. A diff of PR #61 (its files) opens the same way; `q` brings the panel back.
5. github.com/baleen37/herdr-sim/pull/61 in the browser; no diff opens.
6. The editor opens zoomed at the file and line; quitting it shows the diff.
7. Hidden: mtime unchanged over 25 s. Shown: newer within 5 s.
8. A red row naming the missing program; then "herdr-sim doesn't know
   --prs-json: update it"; both clear once the link is back.

## Cleanup
Restore `~/.local/bin/herdr-sim`, close the step-2 tab
(`tab.close --id <tab> --confirmed true`), remove the worktree and its
branch, focus the noted terminal again.

## Sharp edges
- Steps 3–6 need clicks and keys in the window: Herdr has no command that
  clicks a row. A person, or an agent with screen control, does them.
- A linked mod doesn't reload on save: `layout.switch` away and back first.
- `layout.select` right after `layout.switch` can fail ("no stack panel-body"):
  wait a second between them.
