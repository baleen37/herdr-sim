# herdr-sim PRs

A side panel beside Files and Changes that lists the pull requests of the focused terminal: the PR of the branch checked out there, and, for a Claude Code terminal, every PR its session touched.

![herdr-sim PRs](layouts/main-sim.svg)

## What you see

- **The PRs button**, at the right end of the tab bar after Files and Changes: a 26px pill reading "PRs", `selected` while its panel shows, `chrome` otherwise, like the tab bar's own panel buttons. Its block is 56px wide.
- **The panel**, in the side panel column (368px by default) as a card: 10px radius, 1px `border`, `background`.
  - A 42px header: "Pull requests" semibold, the focused terminal's branch (11px, `dim`, truncated, right-aligned) and a 12px spinner (`working`) while it looks.
  - "This branch" (11px semibold `dim` heading) and one row, or "No PR for this branch" / "Not on a git branch".
  - "This session" and its rows, newest first, without the branch's PR. When empty it says why: not a Claude Code terminal, no session found, or no PR yet.
  - A row: a state pill in GitHub's fixed colours with white 10px text (Open `#238636`, Draft `#6e7681`, Merged `#8957e5`, Closed `#da3633`), the title wrapping beside it; below, `owner/repo#N` (`dim`), `+additions` (`ansi2`), `−deletions` (`ansi1`) and "✓ Approved" (`ansi2`) when the review decision is approved.

## How it behaves

- The panel follows the focused terminal (`focused().terminal`, its pane's `cwd` and `branch`). A focused mod pane, such as this panel as a split, keeps the last terminal.
- Clicking a row opens the PR in the browser (`xdg-open`, else `open`; `explorer` on Windows).
- While it is the shown side panel it looks again every 10 seconds and whenever focus moves; hidden, it does nothing, and showing it looks at once.
- Each look is one `herdr-sim --prs-json [--pid <claude pid>] [--cwd <folder>]`, which prints `{ session, branch, prs }`. herdr-sim keeps PR details between runs by its Changes pane's rule (`fresh`: open PRs 30 s, merged and closed 10 min, a failed fetch 10 min) in `$TMPDIR/herdr-sim/app-prs.json`, and finds the branch PR as its sidebar does (`Cache::branch_state`, keyed `<checkout>#<branch>`: an open PR or none rechecked after a minute, a merged or closed one never) in `app-cache.json`, which also remembers how far each transcript was read. Most looks make no GitHub call; the PRs come in one GraphQL request.
- A Claude Code terminal (agent `app` `claude-code`) passes its foreground pid (`terminals()[i].foreground.pid`). herdr-sim reads `~/.claude/sessions/<pid>.json`, follows a `/background` job to its session, and lists the transcript's `pr-link` records, newest first.
- A terminal on another machine (`machine/t1`) shows nothing: its transcripts and checkout are there. A Codex terminal shows only the branch's PR (the app doesn't expose Codex's session id).
- The button adds `sim-prs` to shared `workspace.panels`, so the tab bar remembers it per space like Files and Changes.

## Built from

- **Layout** (`layouts/main-sim.json`): Herdr's `main` 0.1.13 with `tabs` moved into a 42px row `tab-bar` beside a 56px block `sim-prs-button` (`{ mod: "sim-prs", props: { button: true } }`, `window_drag`), and a third child `sim-prs` (`{ mod: "sim-prs" }`) in the `panel-body` stack. Root `preview` is `layouts/main-sim.svg`.
- **Mod** `mods/sim-prs/`, built by `tsc` from `src/` (the built `.js` is committed, since Herdr loads it): `main.ts` is the panel and the button; `links.ts` holds the `--prs-json` types and the pure helpers (`repoOf`, `stateOf`), tested under node (`--experimental-strip-types`) in `tests/`.
- **herdr-sim** `--prs-json` (`changes::app_json`): `claude_session_for_pid`, then the Changes pane's `claude_session`, `fetch_all` and saved-details format.
- **herdr**: `agents`, `panes`, `focused`, `layout`, `theme`, `terminals`, `exec`, `run` (`layout.select`, `layout.show`, `layout.hide`), `shared` / `setShared` (`workspace.panels`), `props` (`button`; `bin`, the herdr-sim to run, default `herdr-sim` on PATH), `window`.
- **Programs**: `herdr-sim` (which runs `gh pr view --json url` and `gh api graphql`), `xdg-open` / `open`.

## Watch out

- The mod folder is linked, not copied (`scripts/install.sh`). Herdr doesn't draw a mod's own SVGs or images through a link, so the mod ships none; the spinner is the shipped `mods/workspace-chrome/spinner.svg`. Herdr doesn't dock a linked mod either: `place` is `none` and the layout places it.
- `hc get` run from a mod (`hc()` or `exec("hc")`) never returns, which left the panel on "Looking…": take the pid from `terminals()`, which carries `foreground`.
- `fs/promises` may only touch the mod's own folder, and a linked mod's folder is the checkout: anything else goes through `readTextFile` or herdr-sim.
- herdr-sim fails on a flag it doesn't know, so a binary older than the panel says so. Builds from before that ignored unknown flags and printed nothing: the panel reports output it can't parse as "update it", and `scripts/install.sh` checks for the JSON.
- `theme()` is typed `Record<string, string>` while gpui-kit wants `` `#${string}` ``: `main.ts` casts it once. `npm run check` needs the app's own type files, copied by `npm run types`; CI builds with `--noCheck` and fails when the committed `.js` differs from `src/`'s build.
- `init`'s first argument isn't the block's props: read `button` with `props()`.
- Saving a file in the checkout doesn't reload a linked mod; switch layouts (`layout.switch` away and back) to load the new code.
- Herdr writes `gpui-kit.d.ts` and `jsconfig.json` into every mod folder; `.gitignore` keeps them out.
- `MOD` in `main.ts` is the folder name, the layout's block id and the stack child. Renaming the folder means changing all three and the layout's `mod` values.
- Claude Code's `pr-link` record is not a documented API and may change.

## Done when

- `npm run check`, `npm test` and `npm run build` (no diff) pass in `herdr-app/`, and `herdr-app check ~/.config/herdr-app/mods/sim-prs` passes.
- `herdr-sim --prs-json --pid <a claude pid>` prints that session's PRs.
- The tab bar shows Files, Changes and PRs; PRs opens and closes the panel.
- In a Claude Code terminal whose session ran `gh pr create`, that PR is under "This session" within 10 seconds.
- In a terminal on a branch with a PR, it is under "This branch"; a click opens it on github.com.
