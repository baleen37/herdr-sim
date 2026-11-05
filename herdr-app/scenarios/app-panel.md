# app-panel: the sim-prs panel follows focus, idles when hidden, and says what's wrong

**What this covers**: `src/main.ts` as Herdr runs it: the PRs button's state
(`showing`), refreshing on focus change and on coming back into view
(`shownContext`), staying idle while hidden (`hidden`), and the error row for a
missing or too-old herdr-sim (`parse`). The data itself is `cli-prs-json.md`'s.

## Pre-state
- Herdr running with `main-sim` shown; `herdr-app check ~/.config/herdr-app/mods/sim-prs` passes.
- `herdr-sim` on `PATH` is this checkout's release build (`~/.local/bin/herdr-sim`).
- A worktree of this repo on `fix/sidebar-session-state` (PR #61, merged) at `$W`.
- Note the focused terminal (`herdr-app status` → `agent`) to come back to.
- Screenshots: `herdr-app shot <file>`; the window must be drawing (not covered).

## Steps
1. Close the panel: `herdr-app do layout.hide --id panel`; shot.
2. Open it on PRs: `layout.select {stack: panel-body, child: sim-prs}`, `layout.show {id: panel}`; shot.
3. New tab in the worktree: `herdr-app do tab.new --folder $W`; wait 5 s; shot.
4. Back to the first terminal: `pane.focus {terminal}`; wait 5 s; shot.
5. Hidden means idle: `layout.hide --id panel`, note `$TMPDIR/herdr-sim/app-cache.json`'s
   mtime, wait 25 s, compare. Then `layout.show` and wait 5 s, compare again.
6. herdr-sim missing: move `~/.local/bin/herdr-sim` aside, wait 12 s; shot.
7. herdr-sim too old: put a script that prints nothing and exits 0 there; wait 12 s; shot.

## Expected
1. The tab bar's PRs pill is unlit (`chrome`); no panel.
2. The pill is lit (`selected`) and the panel shows "Pull requests".
3. "This branch" holds `Merged` "…" `baleen37/herdr-sim#61`, header `fix/sidebar-session-state`.
   Fails if it still shows the previous terminal's branch after 10 s.
4. The panel shows the first terminal's state again (not #61).
5. Hidden: mtime unchanged over 25 s (the 10 s poll would have written twice).
   Shown again: mtime newer within 5 s. Fails either way round.
6. A red error row naming the missing program; no crash, the sections stay.
7. The red row reads "herdr-sim doesn't know --prs-json: update it".

## Cleanup
Restore `~/.local/bin/herdr-sim` (a link to `target/release/herdr-sim`), close the
step-3 tab (`tab.close --id <tab> --confirmed true`), remove the worktree and
its branch, focus the noted terminal again.

## Sharp edges
- A linked mod doesn't reload on save: `layout.switch` away and back first.
- `layout.select` right after `layout.switch` can fail ("no stack panel-body"):
  wait a second between them.
- Steps 6–7 show only after the next poll (10 s) or a focus change.
