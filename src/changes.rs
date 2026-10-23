//! The Changes pane: Claude Code's "Changes" panel in a Herdr split. It lists
//! the PRs the focused agent's session touched — state, title, repo and
//! number, comments, checks, and lines changed — and follows focus between
//! agent panes. `--toggle-changes` opens it on the right or closes it.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex, mpsc};
use std::time::{Duration, Instant};

use herdr_sim::{
    Cache, Session, claude_session, codex_session, diff_text, header_value, parse_utc,
    resolve_claude_session, shortstat_counts,
};
use serde_json::{Value, json};

/// How long a checkout's `+a -d` stands without a file-system event saying
/// it changed, in case one was missed.
const LOCAL_TTL: Duration = Duration::from_secs(30);
/// The least time between two reads of a checkout that keeps changing.
const LOCAL_GAP: Duration = Duration::from_secs(1);
/// How often the pane looks at focus and the session again.
const TICK: Duration = Duration::from_secs(3);
/// How long fetched PR details are reused: open PRs change (checks,
/// comments); merged and closed ones barely do.
const OPEN_TTL: Duration = Duration::from_secs(30);
const DONE_TTL: Duration = Duration::from_secs(600);

const PLUGIN: &str = "baleen37.herdr-sim";
/// The pane the panel starts on, set by the toggle.
pub(crate) const FOR_PANE: &str = "HERDR_SIM_CHANGES_FOR";
const ENTRYPOINT: &str = "changes";

pub(crate) struct Pr {
    url: String,
    repo: String,
    number: String,
    title: String,
    state: String,
    draft: bool,
    /// Reviewers approved it.
    approved: bool,
    additions: u64,
    deletions: u64,
    comments: usize,
    /// The latest comment, to open with `c`.
    comment_url: Option<String>,
    /// Checks passed, failed, and in all.
    checks: Option<(usize, usize, usize)>,
}

/// Opens the Changes pane at the right edge of the tab, or closes it if it
/// is already open in this tab. The panel is found by its process, so panes
/// closed by hand or opened before an upgrade are handled too.
pub fn toggle(herdr: &str, state_dir: &Path) -> anyhow::Result<()> {
    let focused = context_id("HERDR_PANE_ID", "focused_pane_id");
    // Without a tab in the context, the focused pane's: never the whole
    // workspace, whose other tabs have panels of their own.
    let tab = tab_id().or_else(|| focused.as_deref().and_then(|p| tab_of(herdr, p)));
    let Ours {
        live: mut open_now,
        dead,
        ..
    } = our_panes(herdr, workspace_id().as_deref(), tab.as_deref());
    // The panel's own note of its pane id, in case its process list was busy
    // with a gh child when we looked. Only a pane titled and placed as the
    // panel is taken at its word.
    if let Some(tab) = tab.as_deref() {
        let path = marker(state_dir, tab);
        if let Ok(pane) = std::fs::read_to_string(&path) {
            let _ = std::fs::remove_file(&path);
            let pane = pane.trim();
            let panel = Command::new(herdr)
                .args(["pane", "get", pane])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
                .is_some_and(|v| ours(&v["result"]["pane"]));
            if panel && !open_now.iter().chain(&dead).any(|p| p == pane) {
                open_now.push(pane.to_string());
            }
        }
    }
    if !open_now.is_empty() {
        for pane in open_now {
            let _ = Command::new(herdr).args(["pane", "close", &pane]).output();
        }
        return Ok(());
    }
    // What a restart left of a panel: the panel opens again in its place.
    if !dead.is_empty() {
        for pane in &dead {
            replace(herdr, pane, true);
        }
        return Ok(());
    }
    let mut open = Command::new(herdr);
    open.args(["plugin", "pane", "open", "--plugin", PLUGIN])
        .args(["--entrypoint", ENTRYPOINT])
        // Focused, so its keys work at once.
        .args(["--placement", "split", "--direction", "right", "--focus"]);
    // Start on the pane the key was pressed in; after that, follow focus.
    if let Some(pane) = focused {
        open.args(["--env", &format!("{FOR_PANE}={pane}")]);
        if let Some(edge) = right_edge(herdr, &pane) {
            open.args(["--target-pane", &edge]);
        }
    }
    let opened = open.output()?;
    // Narrow it at once, from the id the open returns: the panel would size
    // itself too, but only once its process is up, and the half-screen pane
    // would show until then.
    let pane = serde_json::from_slice::<Value>(&opened.stdout)
        .ok()
        .and_then(|v| {
            v["result"]["plugin_pane"]["pane"]["pane_id"]
                .as_str()
                .map(String::from)
        });
    if let Some(pane) = pane {
        fit_width(herdr, &pane, Some(PANEL_COLS));
    }
    Ok(())
}

/// The workspace this run belongs to. Plugin panes and actions get it only
/// inside `HERDR_PLUGIN_CONTEXT_JSON`; ordinary panes have `HERDR_WORKSPACE_ID`.
fn workspace_id() -> Option<String> {
    context_id("HERDR_WORKSPACE_ID", "workspace_id")
}

/// The tab this run belongs to, found the same way.
fn tab_id() -> Option<String> {
    context_id("HERDR_TAB_ID", "tab_id")
}

fn context_id(var: &str, key: &str) -> Option<String> {
    std::env::var(var).ok().or_else(|| {
        let ctx = std::env::var("HERDR_PLUGIN_CONTEXT_JSON").ok()?;
        let ctx: Value = serde_json::from_str(&ctx).ok()?;
        ctx[key].as_str().map(String::from)
    })
}

/// Where the panel open in `tab` notes its pane id.
fn marker(state_dir: &Path, tab: &str) -> std::path::PathBuf {
    state_dir.join(format!("changes-pane-{}", tab.replace(['/', ':'], "_")))
}

/// The plugin's panes `our_panes` found.
#[derive(Default)]
struct Ours {
    /// Running the Changes panel.
    live: Vec<String>,
    /// What a restart left of a panel or its diff (see `bare_shell`).
    dead: Vec<String>,
    /// Titled as ours but not at their prompt yet.
    waiting: usize,
}

/// The plugin's panes in `tab` of `workspace`, or everywhere.
fn our_panes(herdr: &str, workspace: Option<&str>, tab: Option<&str>) -> Ours {
    let mut list = Command::new(herdr);
    list.args(["pane", "list"]);
    if let Some(ws) = workspace {
        list.args(["--workspace", ws]);
    }
    let Some(v) = list
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
    else {
        return Ours::default();
    };
    let mut found = Ours::default();
    for p in v["result"]["panes"].as_array().into_iter().flatten() {
        let Some(pane) = p["pane_id"].as_str() else {
            continue;
        };
        if tab.is_some_and(|tab| p["tab_id"].as_str() != Some(tab)) {
            continue;
        }
        let titled = ours(p);
        // Everywhere, only panes titled as ours are asked about.
        if tab.is_none() && !titled {
            continue;
        }
        let info = Command::new(herdr)
            .args(["pane", "process-info", "--pane", pane])
            .output()
            .ok()
            .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok());
        let info = info.as_ref().map(|i| &i["result"]["process_info"]);
        match info {
            Some(info) if info.to_string().contains("--changes-pane") => {
                found.live.push(pane.to_string())
            }
            Some(info) if titled && bare_shell(info) => found.dead.push(pane.to_string()),
            // Its shell not started, or still in its rc files.
            Some(info) if titled && !info.to_string().contains("herdr-sim") => found.waiting += 1,
            None if titled => found.waiting += 1,
            _ => {}
        }
    }
    found
}

/// Whether `pane` (from `pane list`) has the title and directory of the
/// panel or its diff. Herdr brings such a pane back after a restart, by its
/// title, as a shell in the plugin's directory.
fn ours(pane: &Value) -> bool {
    matches!(pane["label"].as_str(), Some("Changes" | "Diff"))
        && pane["cwd"].as_str().is_some_and(|cwd| {
            let root = Path::new(cwd);
            root.file_name()
                .is_some_and(|name| name.to_string_lossy().contains("herdr-sim"))
                && root.join("herdr-plugin.toml").exists()
        })
}

/// Whether only the pane's shell runs in it (from `pane process-info`), so a
/// live pane busy with a git or gh child never counts.
fn bare_shell(info: &Value) -> bool {
    info["foreground_processes"]
        .as_array()
        .is_some_and(|procs| {
            !procs.is_empty()
                && procs.iter().all(|p| {
                    p["pid"] == info["shell_pid"]
                        && !p["cmdline"].as_str().unwrap_or("").contains("herdr-sim")
                })
        })
}

/// Opens a panel in place of each pane a restart left of one (or of its
/// diff), so the panels come back with the server. Run at startup.
pub fn revive(herdr: &str) {
    // The shells start with the server and run their rc files first; give
    // them a few seconds to reach their prompt.
    let mut done: Vec<String> = Vec::new();
    for _ in 0..20 {
        let found = our_panes(herdr, None, None);
        // Each once: one whose close failed is not given a second panel.
        for pane in found.dead {
            if !done.contains(&pane) {
                replace(herdr, &pane, false);
                done.push(pane);
            }
        }
        if found.waiting == 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Opens a panel in `pane`'s place, then closes `pane`.
fn replace(herdr: &str, pane: &str, focus: bool) {
    let opened = Command::new(herdr)
        .args(["plugin", "pane", "open", "--plugin", PLUGIN])
        .args(["--entrypoint", ENTRYPOINT, "--placement", "split"])
        .args(["--direction", "right", "--target-pane", pane])
        .arg(if focus { "--focus" } else { "--no-focus" })
        .args(["--env", &format!("{REPLACES}={pane}")])
        .output()
        .is_ok_and(|o| o.status.success());
    if opened {
        let _ = Command::new(herdr).args(["pane", "close", pane]).output();
    }
}

/// The pane a new panel opens to the right of, from `pane`'s tab: one at the
/// tab's right edge, so the panel never lands between two panes.
fn right_edge(herdr: &str, pane: &str) -> Option<String> {
    rightmost(layout(herdr, pane)?["panes"].as_array()?, pane)
}

/// The layout of `pane`'s tab: its `area` and its `panes` with their `rect`s.
fn layout(herdr: &str, pane: &str) -> Option<Value> {
    let out = Command::new(herdr)
        .args(["pane", "layout", "--pane", pane])
        .output()
        .ok()?;
    let mut v: Value = serde_json::from_slice(&out.stdout).ok()?;
    Some(v["result"]["layout"].take())
}

/// Of `panes` (with their `rect`s), `pane` if it reaches the right edge,
/// else the tallest that does, the top one of equals.
fn rightmost(panes: &[Value], pane: &str) -> Option<String> {
    let n = |p: &Value, k: &str| p["rect"][k].as_f64().unwrap_or(0.0);
    let right = |p: &Value| n(p, "x") + n(p, "width");
    let edge = panes.iter().map(right).fold(0.0, f64::max);
    let at_edge: Vec<&Value> = panes.iter().filter(|p| right(p) >= edge - 0.5).collect();
    if at_edge.iter().any(|p| p["pane_id"].as_str() == Some(pane)) {
        return Some(pane.to_string());
    }
    at_edge
        .into_iter()
        .max_by(|a, b| {
            n(a, "height")
                .total_cmp(&n(b, "height"))
                .then(n(b, "y").total_cmp(&n(a, "y")))
        })
        .and_then(|p| p["pane_id"].as_str().map(String::from))
}

/// Width the panel aims for, in columns.
const PANEL_COLS: f64 = 42.0;

/// Names the pane a newly opened panel takes the place of; it waits for it
/// to close before sizing itself.
pub(crate) const REPLACES: &str = "HERDR_SIM_REPLACES";

/// Waits, a second at most, for the pane in `REPLACES` to close.
pub(crate) fn wait_gone(herdr: &str) {
    let Ok(pane) = std::env::var(REPLACES) else {
        return;
    };
    for _ in 0..20 {
        let alive = Command::new(herdr)
            .args(["pane", "get", &pane])
            .output()
            .is_ok_and(|o| o.status.success());
        if !alive {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Moves `pane`'s left edge so it is about `target` columns wide, or half
/// the tab when `None`.
pub(crate) fn fit_width(herdr: &str, pane: &str, target: Option<f64>) {
    let measure = || {
        let layout = layout(herdr, pane)?;
        let width = layout["panes"]
            .as_array()?
            .iter()
            .find(|p| p["pane_id"].as_str() == Some(pane))?["rect"]["width"]
            .as_f64()?;
        Some((width, layout["area"]["width"].as_f64()?))
    };
    let Some((mut width, area)) = measure() else {
        return;
    };
    let target = target.unwrap_or(area / 2.0);
    // A resize's amount is a share of the split the pane is in, which is the
    // whole tab only when the pane was split off a full-width one. Start from
    // the tab, then learn the split's width from how far the edge moved.
    let mut unit = area;
    for _ in 0..3 {
        let by = width - target;
        if by.abs() < 1.0 {
            return;
        }
        // `right` moves the edge right, narrowing the pane; `left` widens it.
        let direction = if by > 0.0 { "right" } else { "left" };
        let amount = (by.abs() / unit).min(1.0);
        let _ = Command::new(herdr)
            .args(["pane", "resize", "--pane", pane, "--direction", direction])
            .arg("--amount")
            .arg(format!("{amount:.3}"))
            .output();
        let Some((now, _)) = measure() else {
            return;
        };
        let moved = (now - width).abs();
        if moved < 1.0 {
            return;
        }
        unit = moved / amount;
        width = now;
    }
}

/// PR details by URL, with when they were fetched; `None` for details read
/// back from disk, which show at once but are refetched.
type Prs = HashMap<String, (Option<Instant>, Option<Pr>)>;

/// Runs the pane until it is closed.
pub fn run(herdr: &str, home: &Path, state_dir: &Path) -> anyhow::Result<()> {
    // Read the main pass's scan cache once and keep scanning in memory; never
    // write it back, so this pane cannot race the plugin's own runs.
    let mut cache = Cache::load(&state_dir.join("pr-cache.json"));
    // Each directory's checkout, found once: sessions revisit the same few.
    let mut checkouts: HashMap<String, Option<String>> = HashMap::new();
    // Each checkout's repo name and GitHub owner/repo, from its remote; asked once.
    let mut names: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    // The last details seen, so a reopened panel draws its PRs before GitHub
    // answers.
    let saved = state_dir.join("changes-prs.json");
    let mut prs = load_prs(&saved);
    let own = std::env::var("HERDR_PANE_ID").ok();
    // The panel's own tab: the agents it shows and falls back on.
    let tab = own.as_deref().and_then(|pane| tab_of(herdr, pane));
    if let Some(pane) = &own {
        // Opened in place of the diff pane, it takes that pane's space when
        // it goes, so it sizes itself then. Opened by the toggle, the toggle
        // has sized it already; sizing again here would race it.
        if std::env::var(REPLACES).is_ok() {
            wait_gone(herdr);
            fit_width(herdr, pane, Some(PANEL_COLS));
        }
        // Tell the toggle which pane is the panel in this tab.
        if let Some(tab) = tab.as_deref() {
            let _ = std::fs::write(marker(state_dir, tab), pane);
        }
    }
    // GitHub is asked off the drawing thread; one request at a time. Clicks
    // arrive on the same channel.
    let (done, events) = mpsc::channel::<Event>();
    listen_for_clicks(done.clone());
    // File-system events tell which checkouts to ask git about again.
    let mut watch = crate::watch::Watch::new(done.clone());
    // Each checkout's `+a -d` and, for a linked worktree, its branch.
    let mut local_cache: HashMap<String, (Instant, Option<String>, Option<String>)> =
        HashMap::new();
    let mut fetching = false;
    let mut last_frame = String::new();
    // What each drawn line does when clicked.
    let mut targets: Vec<Target> = Vec::new();
    // The rows the keys move between, and the selected one, by key so it
    // stays put as the list changes.
    let mut items: Vec<Item> = Vec::new();
    let mut selected: Option<String> = None;
    let mut pending_g = false;
    // Where the left button went down: its column and line in the frame.
    let mut drag_from: Option<(usize, usize)> = None;
    let mut view = View {
        scroll: 0,
        rows: size().0,
        cols: size().1,
        step: 0,
        hover: None,
        selected: None,
        dragged: None,
    };
    let mut last_agent: Option<Value> = None;
    let start = std::env::var(FOR_PANE).ok();
    let workspace = workspace_id();
    let is_here = |a: &&Value| here(a, tab.as_deref(), workspace.as_deref());
    loop {
        // Shortened when a checkout waits out its LOCAL_GAP.
        let mut tick = TICK;
        let agents = agents(herdr);
        let focus = agents.iter().find(|a| a["focused"].as_bool() == Some(true));
        // Only this tab's agents: a panel in each tab follows its own, not
        // whichever agent has focus elsewhere.
        let focused = focus.filter(is_here);
        let opener = start
            .as_deref()
            .and_then(|pane| agents.iter().find(|a| a["pane_id"].as_str() == Some(pane)));
        // The pane it shows closed: another agent in the panel's tab takes
        // its place, even when focus landed elsewhere; with none left, the
        // panel goes with it.
        match next(last_agent.as_ref(), &agents, tab.as_deref(), |pane| {
            pane_exists(herdr, pane)
        }) {
            Next::Follow(agent) => last_agent = Some(agent.clone()),
            Next::Close if close(herdr) => return Ok(()),
            _ => {}
        }
        // The followed agent as it is now: a new session or directory shows
        // without its pane taking focus first.
        if let Some(now) = last_agent
            .as_ref()
            .and_then(|a| agents.iter().find(|b| b["pane_id"] == a["pane_id"]))
        {
            last_agent = Some(now.clone());
        }
        // Focus wins once it is on an agent; until then, the opener's pane,
        // else (a panel brought back with the server) the tab's first agent.
        if let Some(agent) = focused.or_else(|| match last_agent {
            None => opener.or_else(|| agents.iter().find(is_here)),
            Some(_) => None,
        }) {
            last_agent = Some(agent.clone());
        }
        // A panel in a tab not on screen keeps its last frame: no git, no
        // transcripts, no redraw. Only the cheap agent list runs, so it
        // catches up within a tick of coming back on screen.
        let hidden = focus.is_some_and(|a| !is_here(&a));
        let frame = if hidden && !last_frame.is_empty() {
            last_frame.clone()
        } else {
            match &last_agent {
                Some(agent) => {
                    let session = session_of(agent, home, &mut cache);
                    // Newest first, as in Claude Code's Changes panel.
                    let list: Vec<String> = session
                        .as_ref()
                        .map(|s| s.prs.iter().rev().cloned().collect())
                        .unwrap_or_default();
                    let stale: Vec<String> = list
                        .iter()
                        .filter(|url| {
                            !prs.get(*url).is_some_and(|(at, pr)| {
                                let open = pr.as_ref().is_none_or(|p| p.state == "OPEN");
                                at.is_some_and(|at| {
                                    at.elapsed() < if open { OPEN_TTL } else { DONE_TTL }
                                })
                            })
                        })
                        .cloned()
                        .collect();
                    if !stale.is_empty() && !fetching {
                        fetching = true;
                        let done = done.clone();
                        std::thread::spawn(move || {
                            let got = fetch_all(&stale);
                            let _ = done.send(Event::Fetched(stale, got));
                        });
                    }
                    // Every checkout the session worked in, where it runs first.
                    let dirs = agent["cwd"]
                        .as_str()
                        .map(String::from)
                        .into_iter()
                        .chain(session.iter().flat_map(|s| s.dirs.iter().cloned()));
                    let dirs: Vec<String> = dirs.collect();
                    // Directories not seen before are looked up together.
                    let new: Vec<&String> = dirs
                        .iter()
                        .filter(|d| !checkouts.contains_key(*d))
                        .collect();
                    let found: Vec<Option<String>> = std::thread::scope(|s| {
                        let running: Vec<_> =
                            new.iter().map(|dir| s.spawn(|| checkout_of(dir))).collect();
                        running
                            .into_iter()
                            .map(|t| t.join().ok().flatten())
                            .collect()
                    });
                    for (dir, top) in new.into_iter().zip(found) {
                        checkouts.insert(dir.clone(), top);
                    }
                    let mut tops: Vec<String> = Vec::new();
                    for dir in &dirs {
                        if let Some(Some(top)) = checkouts.get(dir)
                            && !tops.contains(top)
                        {
                            tops.push(top.clone());
                        }
                    }
                    // git only for checkouts whose files changed (or not seen
                    // for a while), all at once: git is the slow part of a tick.
                    let stale: Vec<&String> = tops
                        .iter()
                        .filter(|top| {
                            if let Some(watch) = watch.as_mut() {
                                watch.add(top);
                            }
                            let since = local_cache.get(*top).map(|(at, ..)| at.elapsed());
                            // While files keep changing (a build, an install),
                            // at most once a LOCAL_GAP; the change stays noted,
                            // and the tick ends when the gap does.
                            if let Some(t) = since.filter(|t| *t < LOCAL_GAP) {
                                let left = LOCAL_GAP - t;
                                tick = tick.min(left);
                                return false;
                            }
                            let changed = watch.as_ref().is_none_or(|w| w.take(top));
                            changed || since.is_none_or(|t| t >= LOCAL_TTL)
                        })
                        .collect();
                    let fresh: Vec<(Option<String>, Option<String>)> = std::thread::scope(|s| {
                        let running: Vec<_> = stale
                            .iter()
                            .map(|top| s.spawn(|| (local_diff(top), worktree_branch(top))))
                            .collect();
                        running
                            .into_iter()
                            .map(|t| t.join().unwrap_or_default())
                            .collect()
                    });
                    for (top, (diff, branch)) in stale.into_iter().zip(fresh) {
                        local_cache.insert(top.clone(), (Instant::now(), diff, branch));
                    }
                    let local: Vec<Local> = tops
                        .iter()
                        .filter_map(|top| {
                            let (_, diff, branch) = local_cache.get(top)?;
                            let (repo, slug) = names
                                .entry(top.clone())
                                .or_insert_with(|| (repo_name(top), github_slug(top)));
                            Some(Local {
                                top: top.clone(),
                                repo: repo.clone()?,
                                diff: diff.clone()?,
                                branch: branch.clone(),
                                url: slug.as_ref().map(|s| format!("https://github.com/{s}")),
                            })
                        })
                        .collect();
                    let (changes, clicks, rows) = render(&list, &prs, &local);
                    targets = clicks;
                    items = rows;
                    changes
                }
                None => "No agent focused yet.\n".to_string(),
            }
        };
        // Redraw only on change: no flicker, no work for the terminal. The
        // pane's size is read once a tick, not per draw.
        let (rows, cols) = size();
        if frame != last_frame || rows != view.rows || cols != view.cols {
            view.rows = rows;
            view.cols = cols;
            view.scroll = view.scroll.min(max_scroll(&frame, rows));
            // The first row starts selected, so the keys have somewhere to go.
            if selected.is_none() {
                selected = items.first().map(|i| i.key.clone());
            }
            view.selected = selected
                .as_ref()
                .and_then(|key| items.iter().find(|i| &i.key == key))
                .map(|i| i.lines);
            draw(&frame, view, None);
            last_frame = frame;
        }
        // Lines with a running check's dot: the only ones a pulse redraws.
        let pulsing: Vec<usize> = last_frame
            .lines()
            .enumerate()
            .filter(|(_, l)| l.contains(PULSE))
            .map(|(i, _)| i)
            .collect();
        // Wake early when GitHub answers or a click comes, so either shows at
        // once. Meanwhile, running checks pulse.
        let deadline = Instant::now() + tick;
        let event = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let wait = if pulsing.is_empty() {
                left
            } else {
                left.min(PULSE_STEP)
            };
            match events.recv_timeout(wait) {
                // The wheel moves the drawn frame; nothing needs reading again.
                Ok(Event::Scroll(by)) => {
                    let to = view
                        .scroll
                        .saturating_add_signed(by)
                        .min(max_scroll(&last_frame, view.rows));
                    if to != view.scroll {
                        view.scroll = to;
                        view.hover = None;
                        draw(&last_frame, view, None);
                    }
                }
                // The link under the pointer is underlined; only the lines it
                // leaves and enters are redrawn.
                // A drag marks whole rows; letting go copies their text.
                Ok(Event::Press { x, y }) => {
                    let line = y.saturating_sub(1) + view.scroll;
                    view.dragged = None;
                    drag_from = Some((x, line));
                }
                // Leaving the row it started on makes it a drag, not a click.
                Ok(Event::Drag(y)) => {
                    if let Some((_, from)) = drag_from {
                        let to = y.saturating_sub(1) + view.scroll;
                        let range = (to != from || view.dragged.is_some())
                            .then_some((from.min(to), from.max(to)));
                        if range != view.dragged {
                            view.dragged = range;
                            draw(&last_frame, view, None);
                        }
                    }
                }
                Ok(Event::Release) => {
                    let from = drag_from.take();
                    if let Some((first, last)) = view.dragged.take() {
                        copy(&plain_lines(&last_frame, first, last));
                        draw(&last_frame, view, None);
                    } else if let Some((x, line)) = from
                        && let Some(url) = targets.get(line).and_then(|t| t.at(x))
                    {
                        // A plain click opens what it is on, as Ctrl+click does.
                        open_url(url);
                    }
                }
                Ok(Event::Hover { x, y }) => {
                    let line = y.saturating_sub(1) + view.scroll;
                    let to = targets
                        .get(line)
                        .and_then(|t| t.span(x))
                        .map(|(start, end)| (line, start, end));
                    if to != view.hover {
                        let lines: Vec<usize> =
                            [view.hover, to].iter().flatten().map(|h| h.0).collect();
                        view.hover = to;
                        draw(&last_frame, view, Some(&lines));
                    }
                }
                // j/k move between rows; Enter opens the row, p its PR, d its
                // diff, c its latest comment; q closes the panel.
                Ok(Event::Key(key)) => {
                    let at = selected
                        .as_ref()
                        .and_then(|key| items.iter().position(|i| &i.key == key));
                    // gg: a first g waits for the second.
                    let after_g = std::mem::take(&mut pending_g);
                    // Ctrl-d/u move half a screen of rows.
                    let half = (view.rows / 4).max(1);
                    let to = match (key, at) {
                        (b'q', _) if close(herdr) => return Ok(()),
                        (b'g', _) if !after_g => {
                            pending_g = true;
                            None
                        }
                        (b'g', _) => items.first(),
                        (b'G', _) => items.last(),
                        (4, Some(i)) => items.get((i + half).min(items.len().saturating_sub(1))),
                        (21, Some(i)) => items.get(i.saturating_sub(half)),
                        (b'j', Some(i)) => items.get(i + 1),
                        (b'k', Some(i)) => i.checked_sub(1).and_then(|i| items.get(i)),
                        // Nothing selected yet: the first or last row on screen.
                        (b'j', None) => items.iter().find(|i| i.lines.0 >= view.scroll),
                        (b'k', None) => items
                            .iter()
                            .rev()
                            .find(|i| i.lines.1 < view.scroll + view.rows),
                        _ => {
                            let item = at.and_then(|i| items.get(i));
                            let url = item.and_then(|item| match key {
                                b'\r' | b'\n' => item.open.as_deref(),
                                b'p' => item.pr.as_deref(),
                                b'c' => item.comment.as_deref(),
                                _ => None,
                            });
                            if let Some(url) = url {
                                open_url(url);
                            }
                            if let Some((source, cwd)) = item.and_then(|i| diff_source(key, i))
                                && let Some(agent) =
                                    last_agent.as_ref().and_then(|a| a["pane_id"].as_str())
                            {
                                let cwd = cwd.unwrap_or_else(|| home.display().to_string());
                                view_diff(herdr, &source, &cwd, agent);
                            }
                            None
                        }
                    };
                    if let Some(to) = to {
                        // Keep the row on screen.
                        let (first, last) = to.lines;
                        if first < view.scroll {
                            view.scroll = first;
                        } else if last >= view.scroll + view.rows {
                            view.scroll = last + 1 - view.rows;
                        }
                        selected = Some(to.key.clone());
                        view.selected = Some(to.lines);
                        draw(&last_frame, view, None);
                    }
                }
                Ok(event) => break Some(event),
                Err(_) if Instant::now() >= deadline => break None,
                Err(_) => {
                    view.step += 1;
                    draw(&last_frame, view, Some(&pulsing));
                }
            }
        };
        if let Some(Event::Click { x, y }) = event {
            let target = y
                .checked_sub(1)
                .and_then(|row| targets.get(row + view.scroll));
            if let Some(url) = target.and_then(|t| t.at(x)) {
                open_url(url);
            }
        }
        if let Some(Event::Fetched(asked, mut got)) = event {
            fetching = false;
            let now = Some(Instant::now());
            for url in asked {
                // A failed request keeps the last details rather than
                // blanking them.
                let pr = got
                    .remove(&url)
                    .or_else(|| prs.remove(&url).and_then(|(_, pr)| pr));
                prs.insert(url, (now, pr));
            }
            save_prs(&saved, &prs);
        }
    }
}

/// PR details saved by an earlier panel; all count as stale.
fn load_prs(path: &Path) -> Prs {
    let Some(v) = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return HashMap::new();
    };
    v.as_object()
        .into_iter()
        .flatten()
        .map(|(url, p)| {
            let pr = Pr {
                url: url.clone(),
                repo: p["repo"].as_str().unwrap_or("").to_string(),
                number: p["number"].as_str().unwrap_or("").to_string(),
                title: p["title"].as_str().unwrap_or("").to_string(),
                state: p["state"].as_str().unwrap_or("").to_string(),
                draft: p["draft"].as_bool().unwrap_or(false),
                approved: p["approved"].as_bool().unwrap_or(false),
                additions: p["additions"].as_u64().unwrap_or(0),
                deletions: p["deletions"].as_u64().unwrap_or(0),
                comments: p["comments"].as_u64().unwrap_or(0) as usize,
                comment_url: p["comment_url"].as_str().map(String::from),
                checks: p["checks"].as_array().and_then(|c| {
                    let n = |i: usize| c.get(i)?.as_u64().map(|n| n as usize);
                    Some((n(0)?, n(1)?, n(2)?))
                }),
            };
            (url.clone(), (None, Some(pr)))
        })
        .collect()
}

/// Saves the details the panel has, through a temporary file so a panel in
/// another workspace never reads half a file.
fn save_prs(path: &Path, prs: &Prs) {
    let all: serde_json::Map<String, Value> = prs
        .iter()
        .filter_map(|(url, (_, pr))| {
            let p = pr.as_ref()?;
            Some((
                url.clone(),
                json!({
                    "repo": p.repo, "number": p.number, "title": p.title,
                    "state": p.state, "draft": p.draft, "approved": p.approved, "additions": p.additions,
                    "deletions": p.deletions, "comments": p.comments, "comment_url": p.comment_url,
                    "checks": p.checks.map(|(ok, failed, total)| [ok, failed, total]),
                }),
            ))
        })
        .collect();
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, Value::Object(all).to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Every agent pane Herdr knows, or none when the call fails. Asked over the
/// server's socket, which costs no process; the CLI only without one.
fn agents(herdr: &str) -> Vec<Value> {
    if let Ok(mut result) = crate::socket_request("agent.list", json!({}))
        && let Value::Array(agents) = result["agents"].take()
    {
        return agents;
    }
    Command::new(herdr)
        .args(["agent", "list"])
        .output()
        .ok()
        .and_then(|out| serde_json::from_slice::<Value>(&out.stdout).ok())
        .and_then(|v| v["result"]["agents"].as_array().cloned())
        .unwrap_or_default()
}

/// What the panel does about the pane it follows.
enum Next<'a> {
    Stay,
    /// It closed; show this agent in the panel's tab instead.
    Follow(&'a Value),
    /// It closed, and no agent is left in the panel's tab.
    Close,
}

/// Whether the pane the panel follows has closed, and what then. Closed means
/// not among `agents`, and Herdr, asked only then, no longer has it: a pane
/// whose agent quit is still there, and focus on a pane that is no agent never
/// changes what is followed. Another agent in `tab` (the panel's, else the
/// closed pane's), the focused one first, takes its place.
fn next<'a>(
    followed: Option<&Value>,
    agents: &'a [Value],
    tab: Option<&str>,
    exists: impl FnOnce(&str) -> bool,
) -> Next<'a> {
    let Some(pane) = followed.and_then(|a| a["pane_id"].as_str()) else {
        return Next::Stay;
    };
    if agents.iter().any(|a| a["pane_id"].as_str() == Some(pane)) || exists(pane) {
        return Next::Stay;
    }
    let tab = tab.or(followed.and_then(|a| a["tab_id"].as_str()));
    let mut left = agents
        .iter()
        .filter(|a| tab.is_some() && a["tab_id"].as_str() == tab);
    match left.clone().find(|a| a["focused"].as_bool() == Some(true)) {
        Some(agent) => Next::Follow(agent),
        None => left.next().map_or(Next::Close, Next::Follow),
    }
}

/// Whether `agent` is where the panel is: in its tab, or, with the tab
/// unknown, in its workspace (anywhere when that is unknown too).
fn here(agent: &Value, tab: Option<&str>, workspace: Option<&str>) -> bool {
    match tab {
        Some(tab) => agent["tab_id"].as_str() == Some(tab),
        None => workspace.is_none_or(|ws| agent["workspace_id"].as_str() == Some(ws)),
    }
}

/// The tab `pane` is in.
fn tab_of(herdr: &str, pane: &str) -> Option<String> {
    let out = Command::new(herdr)
        .args(["pane", "get", pane])
        .output()
        .ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    v["result"]["pane"]["tab_id"].as_str().map(String::from)
}

/// Whether Herdr still has `pane`: no only when it answers that the pane is
/// not found, so a call that fails otherwise never closes the panel.
fn pane_exists(herdr: &str, pane: &str) -> bool {
    !Command::new(herdr)
        .args(["pane", "get", pane])
        .output()
        .is_ok_and(|o| {
            !o.status.success()
                && [&o.stderr, &o.stdout]
                    .iter()
                    .any(|out| String::from_utf8_lossy(out).contains("pane_not_found"))
        })
}

/// Closes the panel's own pane, which ends this process too; false when
/// Herdr did not close it, and the panel stays.
fn close(herdr: &str) -> bool {
    std::env::var("HERDR_PANE_ID").is_ok_and(|pane| {
        Command::new(herdr)
            .args(["pane", "close", &pane])
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

fn session_of(agent: &Value, home: &Path, cache: &mut Cache) -> Option<Session> {
    let kind = agent["agent"].as_str()?;
    let id = agent["agent_session"]["value"].as_str()?;
    Some(if kind == "claude" {
        claude_session(home, &resolve_claude_session(home, id), cache)
    } else {
        codex_session(home, id, cache)
    })
}

/// What the panel asks GitHub for each PR.
const PR_FIELDS: &str = "title number state isDraft reviewDecision additions deletions \
    comments(last: 1) { totalCount nodes { url } } \
    commits(last: 1) { nodes { commit { statusCheckRollup { contexts { \
    checkRunCountsByState { state count } statusContextCountsByState { state count } \
    } } } } }";

/// Details of every PR in `urls` in one GraphQL request (a PR per alias),
/// instead of a `gh pr view` each. A repository the token cannot read only
/// loses its own entries.
fn fetch_all(urls: &[String]) -> HashMap<String, Pr> {
    let safe = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    let mut query = String::from("query {");
    let mut asked = Vec::new();
    for url in urls {
        let parts: Vec<&str> = url.split('/').collect();
        let (Some(owner), Some(repo), Some(number)) = (parts.get(3), parts.get(4), parts.get(6))
        else {
            continue;
        };
        let Ok(number) = number.parse::<u64>() else {
            continue;
        };
        if !safe(owner) || !safe(repo) {
            continue;
        }
        query += &format!(
            " p{}: repository(owner: \"{owner}\", name: \"{repo}\") {{ pullRequest(number: {number}) {{ {PR_FIELDS} }} }}",
            asked.len()
        );
        asked.push(url.clone());
    }
    // GitHub's budget, read in the same request.
    query += " rateLimit { remaining resetAt } }";
    if asked.is_empty() {
        return HashMap::new();
    }
    // The plugin's 10 s gh deadline: a stalled network must not freeze the panel.
    let Some(out) = crate::gh(&["api", "graphql", "-f", &format!("query={query}")], None) else {
        return HashMap::new();
    };
    let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else {
        return HashMap::new();
    };
    let limit = &v["data"]["rateLimit"];
    if limit["remaining"]
        .as_u64()
        .is_some_and(|left| left < crate::GH_RESERVE)
        && let Some(reset) = limit["resetAt"].as_str().and_then(parse_utc)
    {
        crate::pause_gh(reset);
    }
    asked
        .into_iter()
        .enumerate()
        .filter_map(|(i, url)| {
            let pr = &v["data"][format!("p{i}")]["pullRequest"];
            let title = pr["title"].as_str()?.to_string();
            // Counts per state, not every check: a small answer on busy PRs.
            let contexts = &pr["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"];
            let (mut passed, mut failed, mut total) = (0, 0, 0);
            for kind in ["checkRunCountsByState", "statusContextCountsByState"] {
                for c in contexts[kind].as_array().into_iter().flatten() {
                    let n = c["count"].as_u64().unwrap_or(0) as usize;
                    total += n;
                    match c["state"].as_str() {
                        Some("SUCCESS" | "NEUTRAL" | "SKIPPED") => passed += n,
                        Some(
                            "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED"
                            | "STARTUP_FAILURE",
                        ) => failed += n,
                        _ => {}
                    }
                }
            }
            let parsed = Pr {
                repo: url.split('/').nth(4).unwrap_or("").to_string(),
                number: pr["number"].to_string(),
                title,
                state: pr["state"].as_str().unwrap_or("").to_string(),
                draft: pr["isDraft"].as_bool().unwrap_or(false),
                approved: pr["reviewDecision"] == "APPROVED",
                additions: pr["additions"].as_u64().unwrap_or(0),
                deletions: pr["deletions"].as_u64().unwrap_or(0),
                comments: pr["comments"]["totalCount"].as_u64().unwrap_or(0) as usize,
                comment_url: pr["comments"]["nodes"][0]["url"].as_str().map(String::from),
                checks: (total > 0).then_some((passed, failed, total)),
                url: url.clone(),
            };
            Some((url, parsed))
        })
        .collect()
}

pub(crate) const GREEN: &str = "\x1b[38;2;166;227;161m";
const PURPLE: &str = "\x1b[38;2;203;166;247m";
pub(crate) const RED: &str = "\x1b[38;2;243;139;168m";
pub(crate) const DIM: &str = "\x1b[2m";
/// The selected or pointed-at link: underlined on a raised background, which
/// shows even where underline is not drawn.
const HL_ON: &str = "\x1b[4;48;2;69;71;90m";
const HL_OFF: &str = "\x1b[24;49m";
/// The row the keys selected: a bar across the pane.
pub(crate) const SELECTED: &str = "\x1b[48;2;49;50;68m";
/// Rows a drag covers, to be copied.
const DRAGGED: &str = "\x1b[48;2;88;91;112m";
pub(crate) const RESET: &str = "\x1b[0m";

/// Laid out like Claude Code's Changes panel: per PR, its state icon and
/// title, then repo and number with lines changed, and comments and a checks
/// dot on the right.
fn render(urls: &[String], prs: &Prs, local: &[Local]) -> (String, Vec<Target>, Vec<Item>) {
    let width = size().1.saturating_sub(1).max(20);
    let mut out: Vec<(String, Target)> = Vec::new();
    let mut items: Vec<Item> = Vec::new();
    let blank = |out: &mut Vec<(String, Target)>| out.push((String::new(), Target::default()));
    // What the session's checkouts have not committed yet, above its PRs.
    for local in local {
        let lines: Vec<String> = local
            .diff
            .split(' ')
            .map(|n| {
                let c = if n.starts_with('+') { GREEN } else { RED };
                format!("{c}{n}{RESET}")
            })
            .collect();
        // A linked worktree's branch after the repo, as in the sidebar's
        // header, so it stands apart from the main checkout.
        let name = header_value(&local.repo, local.branch.as_deref());
        let repo = cut(&name, width.saturating_sub(4 + cols(&local.diff)).max(4));
        items.push(Item {
            // By checkout: linked worktrees of one repo share its name.
            key: format!("local:{}", local.top),
            lines: (out.len(), out.len()),
            open: local.url.clone(),
            checkout: Some(local.top.clone()),
            ..Item::default()
        });
        out.push((
            format!("{DIM}±{RESET} {DIM}{repo}{RESET} {}", lines.join(" ")),
            // The row opens the repo, and so is a stop for hjkl too.
            Target::all(local.url.clone()),
        ));
    }
    if !local.is_empty() {
        blank(&mut out);
    } else if urls.is_empty() {
        out.push((format!("{DIM}No changes.{RESET}"), Target::default()));
    }
    for url in urls {
        let Some((_, Some(pr))) = prs.get(url) else {
            // Until its details arrive, the PR is its repo and number.
            let parts: Vec<&str> = url.split('/').collect();
            let name = match (parts.get(4), parts.get(6)) {
                (Some(repo), Some(n)) => format!("{repo} #{n}"),
                _ => url.clone(),
            };
            items.push(Item {
                key: url.clone(),
                lines: (out.len(), out.len()),
                open: Some(url.clone()),
                pr: Some(url.clone()),
                ..Item::default()
            });
            out.push((
                format!("{DIM}\u{f407} {}{RESET}", cut(&name, width - 2)),
                Target::all(Some(url.clone())),
            ));
            blank(&mut out);
            blank(&mut out);
            continue;
        };
        let (icon, colour) = match (pr.state.as_str(), pr.draft) {
            ("MERGED", _) => ("\u{f419}", PURPLE),
            ("CLOSED", _) => ("\u{f4dc}", RED),
            (_, true) => ("\u{f4dd}", DIM),
            // Approved and still open: a green check, as in Claude Code.
            _ if pr.approved => ("\u{f49e}", GREEN),
            _ => ("\u{f407}", GREEN),
        };
        let title = cut(&pr.title, width - 2);
        items.push(Item {
            key: pr.url.clone(),
            lines: (out.len(), out.len() + 1),
            open: Some(pr.url.clone()),
            pr: Some(pr.url.clone()),
            comment: Some(pr.comment_url.clone().unwrap_or_else(|| pr.url.clone())),
            checkout: None,
        });
        out.push((
            format!("{colour}{icon}{RESET} {title}"),
            Target::all(Some(pr.url.clone())),
        ));
        // Each part of the details line links to its own page; columns are
        // 1-based, after the two-space indent.
        let mut spans: Vec<(usize, usize, String)> = Vec::new();
        let name = format!("{} #{}", pr.repo, pr.number);
        let repo_url = pr.url.split("/pull/").next().unwrap_or(&pr.url).to_string();
        spans.push((3, 3 + cols(&pr.repo), repo_url));
        spans.push((3 + cols(&pr.repo), 3 + cols(&name), pr.url.clone()));
        let mut left = format!("{DIM}{name}{RESET}");
        let mut left_cols = cols(&name);
        if pr.additions + pr.deletions > 0 {
            let lines = format!("+{} -{}", pr.additions, pr.deletions);
            let at = 3 + left_cols + 2;
            spans.push((at, at + lines.len(), format!("{}/files", pr.url)));
            left += &format!(
                "  {GREEN}+{}{RESET} {RED}-{}{RESET}",
                pr.additions, pr.deletions
            );
            left_cols += 2 + lines.len();
        }
        // Right-hand parts: (text, columns, colour, link).
        let mut right: Vec<(String, usize, &str, String)> = Vec::new();
        if pr.comments > 0 {
            let n = pr.comments.to_string();
            right.push((format!("\u{f41f} {n}"), 2 + n.len(), DIM, pr.url.clone()));
        }
        // Checks matter only while the PR is open. Running ones show a
        // pulsing grey dot and how many have passed, as in Claude Code.
        if let Some((ok, failed, total)) = pr.checks.filter(|_| pr.state == "OPEN") {
            let checks = format!("{}/checks", pr.url);
            if failed > 0 {
                right.push(("\u{25cf}".into(), 1, RED, checks));
            } else if ok == total {
                right.push(("\u{25cf}".into(), 1, GREEN, checks));
            } else {
                let count = format!("{ok}/{total}");
                let n = 2 + count.len();
                right.push((format!("{PULSE}{RESET} {DIM}{count}"), n, "", checks));
            }
        }
        let right_cols: usize =
            right.iter().map(|(_, n, ..)| n).sum::<usize>() + 2 * right.len().saturating_sub(1);
        let gap = (width - 2).saturating_sub(left_cols + right_cols).max(1);
        let mut line = format!("  {left}{}", " ".repeat(gap));
        let mut at = 3 + left_cols + gap;
        for (i, (text, n, c, url)) in right.into_iter().enumerate() {
            if i > 0 {
                line += "  ";
                at += 2;
            }
            line += &format!("{c}{text}{RESET}");
            spans.push((at, at + n, url));
            at += n;
        }
        out.push((
            line,
            Target {
                spans,
                rest: Some(pr.url.clone()),
            },
        ));
        blank(&mut out);
    }
    let (lines, targets): (Vec<String>, Vec<Target>) = out.into_iter().unzip();
    (lines.join("\n"), targets, items)
}

/// The links a click on a drawn line opens: by column, else the line's own.
#[derive(Clone, Default)]
struct Target {
    /// Columns `[start, end)`, 1-based, and the link there.
    spans: Vec<(usize, usize, String)>,
    rest: Option<String>,
}

impl Target {
    /// The whole line opens `url`.
    fn all(url: Option<String>) -> Self {
        Self {
            spans: Vec::new(),
            rest: url,
        }
    }

    /// The columns `[start, end)` of the link at column `x`.
    fn span(&self, x: usize) -> Option<(usize, usize)> {
        match self
            .spans
            .iter()
            .find(|(start, end, _)| (*start..*end).contains(&x))
        {
            Some((start, end, _)) => Some((*start, *end)),
            // A line of parts underlines only its parts.
            None if self.spans.is_empty() => self.rest.as_ref().map(|_| (1, usize::MAX)),
            None => None,
        }
    }

    /// The link at column `x`.
    fn at(&self, x: usize) -> Option<&str> {
        self.spans
            .iter()
            .find(|(start, end, _)| (*start..*end).contains(&x))
            .map(|(.., url)| url.as_str())
            .or(self.rest.as_deref())
    }
}

/// A row the keys move between: a PR or a checkout's changes, with the
/// pages its keys open.
#[derive(Default)]
struct Item {
    /// What it is, to keep it selected as the list changes.
    key: String,
    /// Its first and last lines in the frame.
    lines: (usize, usize),
    /// Enter.
    open: Option<String>,
    /// p, c.
    pr: Option<String>,
    comment: Option<String>,
    /// d: the checkout, for a row of uncommitted changes.
    checkout: Option<String>,
}

/// What `d` shows for `item` in herdr-sim's diff pane: a checkout's
/// uncommitted changes or a PR's, and the directory to run it in.
fn diff_source(key: u8, item: &Item) -> Option<(String, Option<String>)> {
    if key != b'd' {
        return None;
    }
    if let Some(top) = &item.checkout {
        return Some((format!("checkout:{top}"), Some(top.clone())));
    }
    Some((format!("pr:{}", item.pr.as_deref()?), None))
}

/// Swaps this panel for the diff pane on `source`: the diff opens on the
/// right half beside `agent`, and this panel closes. Quitting the diff brings
/// the panel back.
fn view_diff(herdr: &str, source: &str, cwd: &str, agent: &str) {
    let Ok(pane) = std::env::var("HERDR_PANE_ID") else {
        return;
    };
    let opened = Command::new(herdr)
        .args(["plugin", "pane", "open", "--plugin", PLUGIN])
        .args([
            "--entrypoint",
            "diff",
            "--placement",
            "split",
            "--direction",
            "right",
        ])
        .args(["--focus", "--target-pane", agent, "--cwd", cwd])
        .args(["--env", &format!("{}={source}", crate::diff::SOURCE)])
        .args(["--env", &format!("{FOR_PANE}={agent}")])
        .args(["--env", &format!("{REPLACES}={pane}")])
        .output()
        .is_ok_and(|o| o.status.success());
    if opened {
        // Ends this process too.
        let _ = Command::new(herdr).args(["pane", "close", &pane]).output();
    }
}

/// Opens the Changes panel beside `agent` in place of `replaces`.
pub(crate) fn reopen(herdr: &str, agent: &str, replaces: &str) {
    let _ = Command::new(herdr)
        .args(["plugin", "pane", "open", "--plugin", PLUGIN])
        .args([
            "--entrypoint",
            ENTRYPOINT,
            "--placement",
            "split",
            "--direction",
            "right",
        ])
        .args(["--focus", "--target-pane", agent])
        .args(["--env", &format!("{FOR_PANE}={agent}")])
        .args(["--env", &format!("{REPLACES}={replaces}")])
        .output();
}

/// Stands in a frame for a running check's dot, drawn at each step.
const PULSE: char = '\u{1}';
const PULSE_STEP: Duration = Duration::from_millis(120);
/// The dot's grey at each step: fading in and out.
const PULSE_GREYS: [u8; 10] = [90, 110, 130, 150, 170, 185, 170, 150, 130, 110];

/// Lines one wheel notch scrolls.
const SCROLL_LINES: isize = 3;

/// How far `frame` can scroll: the lines that do not fit the pane.
fn max_scroll(frame: &str, rows: usize) -> usize {
    frame.lines().count().saturating_sub(rows)
}

/// Draws what of `frame` fits the pane from line `scroll`, pulsing dots at
/// `step`: line by line over the last frame, each cleared to its end, so
/// nothing flickers.
fn draw(frame: &str, view: View, only: Option<&[usize]>) {
    let grey = PULSE_GREYS[view.step % PULSE_GREYS.len()];
    let dot = format!("\x1b[38;2;{grey};{grey};{grey}m\u{25cf}");
    let mut out = String::new();
    // No autowrap: a line one column too wide is cut, instead of wrapping
    // and scrolling the whole pane.
    out += "\x1b[?7l";
    let lines: Vec<&str> = frame.lines().skip(view.scroll).take(view.rows).collect();
    for row in 0..view.rows {
        let i = row + view.scroll;
        if only.is_some_and(|only| !only.contains(&i)) {
            continue;
        }
        let line = lines.get(row).copied().unwrap_or("");
        let line = match (view.selected, view.hover) {
            _ if view
                .dragged
                .is_some_and(|(first, last)| (first..=last).contains(&i)) =>
            {
                bar(line, view.cols, DRAGGED)
            }
            (Some((first, last)), _) if (first..=last).contains(&i) => {
                bar(line, view.cols, SELECTED)
            }
            (_, Some((at, start, end))) if at == i => highlight(line, start, end),
            _ => line.to_string(),
        };
        // Each row by its position, cleared to its end: nothing else moves.
        out += &format!("\x1b[{};1H{}\x1b[K", row + 1, line.replace(PULSE, &dot));
    }
    let mut stdout = io::stdout();
    // One synchronized update (DEC mode 2026): terminals that know it show
    // the frame whole, never half drawn.
    let _ = stdout.write_all(format!("\x1b[?2026h{out}\x1b[?2026l").as_bytes());
    let _ = stdout.flush();
}

/// What part of a frame is drawn, and how.
#[derive(Clone, Copy)]
struct View {
    /// Lines scrolled off the top.
    scroll: usize,
    /// The pane's height and width.
    rows: usize,
    cols: usize,
    /// Pulse step of running checks' dots.
    step: usize,
    /// The link under the pointer: its line in the frame, and columns.
    hover: Option<(usize, usize, usize)>,
    /// The selected row's first and last lines in the frame.
    selected: Option<(usize, usize)>,
    /// The lines a drag covers so far.
    dragged: Option<(usize, usize)>,
}

/// `line` with columns `[start, end)` (1-based) highlighted, through any
/// colour resets inside.
fn highlight(line: &str, start: usize, end: usize) -> String {
    let mut out = String::new();
    let mut col = 1;
    let mut on = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Copy the escape whole; a reset inside the span turns the
            // underline off, so turn it back on.
            out.push(c);
            let mut sgr = false;
            for e in chars.by_ref() {
                out.push(e);
                if e.is_ascii_alphabetic() || e == '\\' {
                    sgr = e == 'm';
                    break;
                }
            }
            if sgr && on {
                out += HL_ON;
            }
            continue;
        }
        if !on && col >= start && col < end && c != ' ' {
            on = true;
            out += HL_ON;
        }
        if on && col >= end {
            on = false;
            out += HL_OFF;
        }
        out.push(c);
        col += if c == PULSE { 1 } else { char_cols(c) };
    }
    if on {
        out += HL_OFF;
    }
    out
}

/// `line` on `colour`'s background, across the pane's whole width.
fn bar(line: &str, width: usize, colour: &str) -> String {
    let mut out = colour.to_string();
    let mut cols = 0;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // A reset inside the line would end the background; restore it.
            out.push(c);
            for e in chars.by_ref() {
                out.push(e);
                if e.is_ascii_alphabetic() || e == '\\' {
                    if e == 'm' {
                        out += colour;
                    }
                    break;
                }
            }
            continue;
        }
        out.push(c);
        cols += if c == PULSE { 1 } else { char_cols(c) };
    }
    out += &" ".repeat(width.saturating_sub(cols));
    out + "\x1b[49m"
}

/// The text of `frame`'s lines `first..=last`, without colours.
fn plain_lines(frame: &str, first: usize, last: usize) -> String {
    let lines: Vec<String> = frame
        .lines()
        .skip(first)
        .take(last + 1 - first)
        .map(|line| {
            let mut out = String::new();
            let mut chars = line.chars();
            while let Some(c) = chars.next() {
                if c == '\x1b' {
                    for e in chars.by_ref() {
                        if e.is_ascii_alphabetic() || e == '\\' {
                            break;
                        }
                    }
                } else {
                    out.push(if c == PULSE { '\u{25cf}' } else { c });
                }
            }
            out.trim_end().to_string()
        })
        .collect();
    lines.join("\n")
}

/// Puts `text` on the clipboard.
fn copy(text: &str) {
    let tool = if cfg!(target_os = "macos") {
        "pbcopy"
    } else {
        "wl-copy"
    };
    if let Ok(mut child) = Command::new(tool).stdin(Stdio::piped()).spawn() {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

/// What wakes the Changes loop besides its tick.
pub(crate) enum Event {
    Fetched(Vec<String>, HashMap<String, Pr>),
    Click {
        x: usize,
        y: usize,
    },
    /// Wheel: lines to move down (negative: up).
    Scroll(isize),
    /// The pointer moved, no button held.
    Hover {
        x: usize,
        y: usize,
    },
    /// A key pressed in the pane.
    Key(u8),
    /// Work finished off the drawing thread; draw again.
    Wake,
    /// Left button pressed or dragged, on a 1-based row, or released.
    Press {
        x: usize,
        y: usize,
    },
    Drag(usize),
    Release,
}

/// Turns on mouse reporting in the pane and sends each Ctrl+left click, as
/// 1-based column and row, and each wheel notch to `events`.
pub(crate) fn listen_for_clicks(events: mpsc::Sender<Event>) {
    // Unbuffered input without echo; output keeps its newlines.
    let _ = Command::new("stty")
        .args(["-icanon", "-echo"])
        .stdin(Stdio::inherit())
        .status();
    // Button and motion events, in SGR encoding.
    // The alternate screen has no scrollback, so herdr shows no scrollbar
    // flashing at each redraw; the cursor is hidden.
    print!("\x1b[?1049h\x1b[?25l\x1b[?1003h\x1b[?1006h");
    let _ = io::stdout().flush();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while io::stdin().read(&mut byte).is_ok_and(|n| n == 1) {
            // A plain key, outside any escape sequence.
            if buf.is_empty() && byte[0] != 0x1b {
                if events.send(Event::Key(byte[0])).is_err() {
                    return;
                }
                continue;
            }
            // ESC [ < button ; x ; y M  — a press; `m` is a release.
            if byte[0] == 0x1b {
                buf.clear();
            }
            buf.push(byte[0]);
            // Any escape but a mouse report (an arrow key, a bare Esc) is
            // dropped once complete, so keys after it still arrive.
            if !buf.starts_with(b"\x1b[<") {
                match buf.len() {
                    1 => {}
                    // A bare Esc: the byte after it is a key of its own.
                    2 if buf[1] != b'[' => {
                        buf.clear();
                        if events.send(Event::Key(byte[0])).is_err() {
                            return;
                        }
                    }
                    2 => {}
                    _ if (0x40..=0x7e).contains(&byte[0]) => buf.clear(),
                    _ => {}
                }
                continue;
            }
            if !matches!(byte[0], b'M' | b'm') {
                continue;
            }
            let seq = String::from_utf8_lossy(&buf[3..buf.len() - 1]).to_string();
            let press = byte[0] == b'M';
            buf.clear();
            let n: Vec<usize> = seq.split(';').filter_map(|n| n.parse().ok()).collect();
            let event = match n[..] {
                // Ctrl adds 16 to the button: Ctrl+click opens at once.
                [16, x, y] if press => Event::Click { x, y },
                // A plain click opens what it is on; a drag across rows copies.
                [0, x, y] if press => Event::Press { x, y },
                [32, _, y] => Event::Drag(y),
                [0, ..] => Event::Release,
                // Motion with no button (32 + 3), with or without Ctrl.
                [35 | 51, x, y] => Event::Hover { x, y },
                [64, ..] => Event::Scroll(-SCROLL_LINES),
                [65, ..] => Event::Scroll(SCROLL_LINES),
                _ => continue,
            };
            if events.send(event).is_err() {
                return;
            }
        }
    });
}

/// Opens `url` in the default browser.
fn open_url(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = Command::new(opener)
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Uncommitted changes in a checkout: its repo, `+a -d`, and its page.
struct Local {
    /// The checkout's top directory.
    top: String,
    repo: String,
    diff: String,
    /// The branch, when the checkout is a linked worktree: its repo's name
    /// alone would not tell it from the main checkout.
    branch: Option<String>,
    /// The repo on GitHub, if it is there.
    url: Option<String>,
}

/// The top of the git checkout holding `dir`, if any.
fn checkout_of(dir: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["-C", dir, "rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    let top = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !top.is_empty()).then_some(top)
}

/// The branch the checkout at `top` has out (the commit, detached), if it
/// is a linked worktree: its git directory is not the repository's own.
/// (The sidebar's `git_repo` test would also take a submodule for one.)
fn worktree_branch(top: &str) -> Option<String> {
    let out = git_in(
        top,
        &[
            "rev-parse",
            "--git-dir",
            "--git-common-dir",
            "--abbrev-ref",
            "HEAD",
        ],
    )?;
    let mut lines = out.lines();
    let (dir, common, branch) = (lines.next()?, lines.next()?, lines.next()?);
    if dir == common {
        return None;
    }
    match branch {
        "HEAD" => git_in(top, &["rev-parse", "--short", "HEAD"]),
        _ => Some(branch.to_string()),
    }
}

/// `+a -d` the checkout at `top` has not committed, new untracked files'
/// lines counted as added, as the diff pane shows them; `None` when clean.
fn local_diff(top: &str) -> Option<String> {
    let tracked = git_in(top, &["diff", "--shortstat", "HEAD"]).unwrap_or_default();
    let (added, removed) = shortstat_counts(&tracked);
    let new: u64 = git_in(top, &["ls-files", "-z", "--others", "--exclude-standard"])
        .unwrap_or_default()
        .split('\0')
        .filter(|f| !f.is_empty())
        .map(|f| text_lines(&Path::new(top).join(f)))
        .sum();
    diff_text(added + new, removed)
}

/// Lines in the text file at `path`, as git counts them; none for a binary
/// file (a NUL in its first 8000 bytes, git's test) or one it cannot read.
/// Each file is read again only when its size or time changed: an agent
/// writing many new files must not have every one reread at each.
fn text_lines(path: &Path) -> u64 {
    type Stamp = (u64, Option<std::time::SystemTime>);
    static SEEN: LazyLock<Mutex<HashMap<PathBuf, (Stamp, u64)>>> = LazyLock::new(Mutex::default);
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    let stamp = (meta.len(), meta.modified().ok());
    if let Some((at, n)) = SEEN.lock().unwrap().get(path)
        && *at == stamp
    {
        return *n;
    }
    let n = count_lines(path).unwrap_or(0);
    let mut seen = SEEN.lock().unwrap();
    // Files come and go; forget them all now and then rather than track which.
    if seen.len() > 100_000 {
        seen.clear();
    }
    seen.insert(path.to_path_buf(), (stamp, n));
    n
}

/// `text_lines` without the memo, reading the file a block at a time and
/// a binary one no further than its start.
fn count_lines(path: &Path) -> io::Result<u64> {
    let mut file = std::fs::File::open(path)?;
    let mut buf = [0u8; 8000];
    let (mut lines, mut last, mut first) = (0, b'\n', true);
    loop {
        let n = io::Read::read(&mut file, &mut buf)?;
        if n == 0 {
            return Ok(lines + u64::from(last != b'\n'));
        }
        if first && buf[..n].contains(&0) {
            return Ok(0);
        }
        first = false;
        lines += buf[..n].iter().filter(|&&b| b == b'\n').count() as u64;
        last = buf[n - 1];
    }
}

/// `owner/repo` of the checkout's GitHub `origin`, if it has one.
fn github_slug(top: &str) -> Option<String> {
    let remote = git_in(top, &["remote", "get-url", "origin"])?;
    let slug = remote
        .strip_prefix("git@github.com:")
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))
        .or_else(|| remote.strip_prefix("https://github.com/"))?;
    Some(slug.trim_end_matches(".git").to_string())
}

/// The repo's name: from its GitHub remote, since a linked worktree's
/// directory is not the repo's name; else the checkout's directory.
fn repo_name(top: &str) -> Option<String> {
    match github_slug(top) {
        Some(slug) => Some(slug.rsplit('/').next().unwrap_or(&slug).to_string()),
        None => Some(Path::new(top).file_name()?.to_string_lossy().into_owned()),
    }
}

/// `git -C dir args`'s trimmed output; `None` on failure or no output.
fn git_in(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

/// The pane's size as (rows, columns), from the terminal it runs in.
pub(crate) fn size() -> (usize, usize) {
    window_size().unwrap_or_else(|| {
        let out = Command::new("stty")
            .arg("size")
            .stdin(Stdio::inherit())
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default();
        let mut n = out.split_whitespace().map(|n| n.parse().ok());
        (
            n.next().flatten().unwrap_or(40),
            n.next().flatten().unwrap_or(40),
        )
    })
}

/// The terminal's rows and columns from the kernel (TIOCGWINSZ on stdout),
/// without spawning `stty`; `None` where that is not available.
fn window_size() -> Option<(usize, usize)> {
    #[cfg(target_os = "macos")]
    const TIOCGWINSZ: std::ffi::c_ulong = 0x4008_7468;
    #[cfg(target_os = "linux")]
    const TIOCGWINSZ: std::ffi::c_ulong = 0x5413;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        #[repr(C)]
        struct Winsize {
            rows: u16,
            cols: u16,
            xpixel: u16,
            ypixel: u16,
        }
        unsafe extern "C" {
            fn ioctl(fd: std::ffi::c_int, request: std::ffi::c_ulong, ...) -> std::ffi::c_int;
        }
        let mut size = Winsize {
            rows: 0,
            cols: 0,
            xpixel: 0,
            ypixel: 0,
        };
        // SAFETY: TIOCGWINSZ writes one Winsize through the pointer given.
        let ok = unsafe { ioctl(1, TIOCGWINSZ, &mut size as *mut Winsize) } == 0;
        (ok && size.rows > 0).then_some((size.rows as usize, size.cols as usize))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    None
}

/// Terminal columns `c` takes: two for Hangul and other East Asian wide
/// characters, one otherwise.
fn char_cols(c: char) -> usize {
    match c as u32 {
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6 => 2,
        _ => 1,
    }
}

pub(crate) fn cols(s: &str) -> usize {
    s.chars().map(char_cols).sum()
}

/// `s` cut to `n` columns, with an ellipsis when cut.
pub(crate) fn cut(s: &str, n: usize) -> String {
    if cols(s) <= n {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        if used + char_cols(c) > n.saturating_sub(1) {
            break;
        }
        used += char_cols(c);
        out.push(c);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(checks: (usize, usize, usize)) -> Prs {
        let url = "https://github.com/o/repo/pull/7".to_string();
        let pr = Pr {
            url: url.clone(),
            repo: "repo".into(),
            number: "7".into(),
            title: "Title".into(),
            state: "OPEN".into(),
            draft: false,
            approved: false,
            comment_url: Some("https://github.com/o/repo/pull/7#issuecomment-1".into()),
            additions: 3,
            deletions: 1,
            comments: 2,
            checks: Some(checks),
        };
        HashMap::from([(url, (None, Some(pr)))])
    }

    #[test]
    fn highlight_marks_only_its_columns() {
        let line = format!("  {DIM}repo #7{RESET}  x");
        assert_eq!(
            highlight(&line, 3, 7),
            format!("  {DIM}{HL_ON}repo{HL_OFF} #7{RESET}  x")
        );
        // A reset inside the span keeps the underline on.
        let line = format!("{GREEN}+3{RESET} {RED}-1{RESET}");
        assert_eq!(
            highlight(&line, 1, 6),
            format!("{GREEN}{HL_ON}+3{RESET}{HL_ON} {RED}{HL_ON}-1{RESET}{HL_ON}{HL_OFF}")
        );
    }

    #[test]
    fn details_line_links_each_part() {
        let prs = pr((1, 0, 2));
        let urls: Vec<String> = prs.keys().cloned().collect();
        let (frame, targets, items) = render(&urls, &prs, &[]);
        // One row for the PR, over its title and details; d shows its diff,
        // c opens its latest comment.
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].lines, (0, 1));
        assert_eq!(
            diff_source(b'd', &items[0]),
            Some(("pr:https://github.com/o/repo/pull/7".into(), None))
        );
        assert_eq!(
            items[0].comment.as_deref(),
            Some("https://github.com/o/repo/pull/7#issuecomment-1")
        );
        let line = frame.lines().nth(1).unwrap();
        let target = &targets[1];
        let plain: String = {
            let mut s = String::new();
            let mut esc = false;
            for c in line.chars() {
                match c {
                    '\x1b' => esc = true,
                    'm' if esc => esc = false,
                    _ if esc => {}
                    _ => s.push(c),
                }
            }
            s
        };
        let col = |needle: &str| {
            plain
                .find(needle)
                .map(|i| plain[..i].chars().count() + 1)
                .unwrap()
        };
        let pr = "https://github.com/o/repo/pull/7";
        assert_eq!(target.at(col("repo")), Some("https://github.com/o/repo"));
        assert_eq!(target.at(col("#7")), Some(pr));
        assert_eq!(target.at(col("+3")), Some(&*format!("{pr}/files")));
        assert_eq!(target.at(col("2")), Some(pr));
        // Checks still running pulse, and link to the checks page.
        assert_eq!(
            target.at(col(&PULSE.to_string())),
            Some(&*format!("{pr}/checks"))
        );
    }

    #[test]
    fn follows_another_agent_in_its_tab_before_closing() {
        let agent = |pane: &str, tab: &str| json!({ "pane_id": pane, "tab_id": tab });
        let followed = agent("p1", "t1");
        let never = |_: &str| panic!("asked Herdr about a pane it listed");
        let pane = |a: &Value| a["pane_id"].as_str().map(String::from);
        // The pane it shows is still an agent: stays, without asking Herdr.
        // Focus on a pane that is no agent leaves it listed, so this holds.
        let agents = [agent("p1", "t1"), agent("p2", "t1")];
        assert!(matches!(
            next(Some(&followed), &agents, Some("t1"), never),
            Next::Stay
        ));
        // Its agent quit but the pane is still there: stays.
        let agents = [agent("p2", "t1")];
        assert!(matches!(
            next(Some(&followed), &agents, Some("t1"), |_| true),
            Next::Stay
        ));
        // Closed, with another agent stacked in its tab: shows that one.
        match next(Some(&followed), &agents, Some("t1"), |_| false) {
            Next::Follow(a) => assert_eq!(pane(a).as_deref(), Some("p2")),
            _ => panic!("expected to follow p2"),
        }
        // Of several, the focused one.
        let agents = [
            agent("p2", "t1"),
            json!({ "pane_id": "p4", "tab_id": "t1", "focused": true }),
        ];
        match next(Some(&followed), &agents, Some("t1"), |_| false) {
            Next::Follow(a) => assert_eq!(pane(a).as_deref(), Some("p4")),
            _ => panic!("expected to follow p4"),
        }
        // Closed, with agents only in other tabs: closes.
        let agents = [agent("p3", "t2")];
        assert!(matches!(
            next(Some(&followed), &agents, Some("t1"), |_| false),
            Next::Close
        ));
        assert!(matches!(
            next(Some(&followed), &[], Some("t1"), |_| false),
            Next::Close
        ));
        // The panel's tab unknown: the closed pane's tab stands in.
        let agents = [agent("p2", "t1")];
        assert!(matches!(
            next(Some(&followed), &agents, None, |_| false),
            Next::Follow(_)
        ));
        // Following nothing yet: nothing to lose.
        assert!(matches!(next(None, &[], Some("t1"), |_| false), Next::Stay));
    }

    #[test]
    fn opens_at_the_tabs_right_edge() {
        let pane = |id: &str, x: u32, y: u32, w: u32, h: u32| json!({ "pane_id": id, "rect": { "x": x, "y": y, "width": w, "height": h } });
        // Agent beside a shell: right of the shell, not between them.
        let side = [pane("a", 0, 0, 60, 40), pane("s", 60, 0, 60, 40)];
        assert_eq!(rightmost(&side, "a").as_deref(), Some("s"));
        assert_eq!(rightmost(&side, "s").as_deref(), Some("s"));
        // Stacked on the right: the focused one if it is there, else the
        // tallest, the top one of equals.
        let stack = [
            pane("a", 0, 0, 60, 40),
            pane("t", 60, 0, 60, 10),
            pane("b", 60, 10, 60, 30),
        ];
        assert_eq!(rightmost(&stack, "a").as_deref(), Some("b"));
        assert_eq!(rightmost(&stack, "t").as_deref(), Some("t"));
        let halves = [pane("t", 0, 0, 120, 20), pane("b", 0, 20, 120, 20)];
        assert_eq!(rightmost(&halves, "x").as_deref(), Some("t"));
    }

    #[test]
    fn a_restart_leaves_a_shell_where_the_panel_was() {
        let pane = |label: &str, cwd: &str| json!({ "label": label, "cwd": cwd });
        let store = tempfile::TempDir::new().unwrap();
        let root = store.path().join("abc-herdr-sim-0.31.1");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("herdr-plugin.toml"), "").unwrap();
        let root = root.to_str().unwrap();
        let shell = json!({ "shell_pid": 7, "foreground_processes": [
            { "pid": 7, "cmdline": "-zsh" } ] });
        assert!(ours(&pane("Changes", root)) && bare_shell(&shell));
        assert!(ours(&pane("Diff", root)));
        // A pane of the user's that shares the title, elsewhere, or in a
        // directory merely named like the plugin.
        assert!(!ours(&pane("Changes", "/home/u/src")));
        assert!(!ours(&pane("Changes", store.path().to_str().unwrap())));
        // The live panel, and the live diff busy with a git child.
        let live = json!({ "shell_pid": 7, "foreground_processes": [
            { "pid": 7, "cmdline": format!("{root}/bin/herdr-sim --diff-pane") } ] });
        assert!(!bare_shell(&live));
        let busy = json!({ "shell_pid": 7, "foreground_processes": [
            { "pid": 9, "cmdline": "git diff HEAD" } ] });
        assert!(!bare_shell(&busy));
    }

    #[test]
    fn shows_only_its_own_tabs_agents() {
        let a = json!({ "pane_id": "w1:p3", "tab_id": "w1:t2", "workspace_id": "w1" });
        // Another tab of the same workspace is not the panel's.
        assert!(!here(&a, Some("w1:t1"), Some("w1")));
        assert!(here(&a, Some("w1:t2"), Some("w1")));
        // The tab unknown: its workspace stands in, then anywhere.
        assert!(here(&a, None, Some("w1")));
        assert!(!here(&a, None, Some("w2")));
        assert!(here(&a, None, None));
    }

    #[test]
    fn local_rows_count_new_files_and_name_worktrees() {
        let dir = tempfile::TempDir::new().unwrap();
        let top = dir.path().to_str().unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .args(["-C", top, "-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        std::fs::write(dir.path().join("a.txt"), "a\nb\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        assert_eq!(local_diff(top), None);
        // Only a new file: its lines, the last one without a newline too.
        std::fs::write(dir.path().join("new.txt"), "1\n2\n3").unwrap();
        assert_eq!(local_diff(top).as_deref(), Some("+3"));
        // With an edit; binary and ignored files add nothing.
        std::fs::write(dir.path().join("a.txt"), "a\nc\n").unwrap();
        std::fs::write(dir.path().join("bin"), b"x\0y\n").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "out\n").unwrap();
        std::fs::write(dir.path().join("out"), "1\n2\n").unwrap();
        assert_eq!(local_diff(top).as_deref(), Some("+5 -1"));
        // A linked worktree names its branch; the main checkout does not.
        let wt = dir.path().join("wt");
        git(&["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()]);
        assert_eq!(worktree_branch(top), None);
        assert_eq!(
            worktree_branch(wt.to_str().unwrap()).as_deref(),
            Some("feat")
        );
        // Detached: the short commit, not `HEAD`.
        let det = dir.path().join("det");
        git(&["worktree", "add", "-q", "--detach", det.to_str().unwrap()]);
        let sha = git_in(top, &["rev-parse", "--short", "HEAD"]);
        assert_eq!(worktree_branch(det.to_str().unwrap()), sha);
        let local = |top: &str, branch: Option<&str>| Local {
            top: top.into(),
            repo: "repo".into(),
            diff: "+1".into(),
            branch: branch.map(String::from),
            url: None,
        };
        let (frame, ..) = render(
            &[],
            &HashMap::new(),
            &[local("/r", None), local("/r/wt", Some("feat"))],
        );
        assert!(frame.contains("repo  \u{f418} feat"));
        assert_eq!(frame.matches('\u{f418}').count(), 1);
    }
}
