//! The Changes pane: Claude Code's "Changes" panel in a Herdr split. It lists
//! the PRs the focused agent's session touched — state, title, repo and
//! number, comments, checks, and lines changed — and follows focus between
//! agent panes. `--toggle-changes` opens it on the right or closes it.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use herdr_sim::{
    Cache, Session, claude_session, codex_session, diff_value, parse_utc, resolve_claude_session,
};
use serde_json::{Value, json};

/// How often the pane looks at focus and the session again.
const TICK: Duration = Duration::from_secs(3);
/// How long fetched PR details are reused: open PRs change (checks,
/// comments); merged and closed ones barely do.
const OPEN_TTL: Duration = Duration::from_secs(30);
const DONE_TTL: Duration = Duration::from_secs(600);

const PLUGIN: &str = "baleen37.herdr-sim";
/// The pane the panel starts on, set by the toggle.
const FOR_PANE: &str = "HERDR_SIM_CHANGES_FOR";
const ENTRYPOINT: &str = "changes";

struct Pr {
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

/// Opens the Changes pane to the right of the focused pane, or closes it if it
/// is already open in this workspace. The panel is found by its process, so
/// panes closed by hand or opened before an upgrade are handled too.
pub fn toggle(herdr: &str, state_dir: &Path) -> anyhow::Result<()> {
    let workspace = workspace_id();
    let mut open_now = changes_panes(herdr, workspace.as_deref());
    // The panel's own note of its pane id, in case its process list was busy
    // with a gh child when we looked.
    if let Some(ws) = workspace.as_deref() {
        let path = marker(state_dir, ws);
        if let Ok(pane) = std::fs::read_to_string(&path) {
            let _ = std::fs::remove_file(&path);
            let exists = Command::new(herdr)
                .args(["pane", "get", pane.trim()])
                .output()
                .is_ok_and(|o| o.status.success());
            if exists && !open_now.iter().any(|p| p == pane.trim()) {
                open_now.push(pane.trim().to_string());
            }
        }
    }
    if !open_now.is_empty() {
        for pane in open_now {
            let _ = Command::new(herdr).args(["pane", "close", &pane]).output();
        }
        return Ok(());
    }
    let mut open = Command::new(herdr);
    open.args(["plugin", "pane", "open", "--plugin", PLUGIN])
        .args(["--entrypoint", ENTRYPOINT])
        // Focused, so its keys work at once.
        .args(["--placement", "split", "--direction", "right", "--focus"]);
    // Start on the pane the key was pressed in; after that, follow focus.
    if let Ok(pane) = std::env::var("HERDR_PANE_ID") {
        open.args(["--env", &format!("{FOR_PANE}={pane}")]);
    }
    open.output()?;
    Ok(())
}

/// The workspace this run belongs to. Plugin panes and actions get it only
/// inside `HERDR_PLUGIN_CONTEXT_JSON`; ordinary panes have `HERDR_WORKSPACE_ID`.
fn workspace_id() -> Option<String> {
    std::env::var("HERDR_WORKSPACE_ID").ok().or_else(|| {
        let ctx = std::env::var("HERDR_PLUGIN_CONTEXT_JSON").ok()?;
        let ctx: Value = serde_json::from_str(&ctx).ok()?;
        ctx["workspace_id"].as_str().map(String::from)
    })
}

/// Where the panel open in `workspace` notes its pane id.
fn marker(state_dir: &Path, workspace: &str) -> std::path::PathBuf {
    state_dir.join(format!(
        "changes-pane-{}",
        workspace.replace(['/', ':'], "_")
    ))
}

/// Panes in `workspace` (or everywhere) running the Changes panel.
fn changes_panes(herdr: &str, workspace: Option<&str>) -> Vec<String> {
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
        return Vec::new();
    };
    v["result"]["panes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["pane_id"].as_str())
        .filter(|pane| {
            Command::new(herdr)
                .args(["pane", "process-info", "--pane", pane])
                .output()
                .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("--changes-pane"))
        })
        .map(String::from)
        .collect()
}

/// Width the panel aims for, in columns.
const PANEL_COLS: f64 = 42.0;

/// The `pane resize --amount` that brings `pane` down to about PANEL_COLS:
/// the fraction of the tab's width to move its left edge by.
fn shrink_amount(herdr: &str, pane: &str) -> Option<f64> {
    let out = Command::new(herdr)
        .args(["pane", "layout", "--pane", pane])
        .output()
        .ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let layout = &v["result"]["layout"];
    let area = layout["area"]["width"].as_f64()?;
    let width = layout["panes"]
        .as_array()?
        .iter()
        .find(|p| p["pane_id"].as_str() == Some(pane))?["rect"]["width"]
        .as_f64()?;
    let amount = (width - PANEL_COLS) / area;
    (amount > 0.0).then_some(amount)
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
    if let Ok(pane) = std::env::var("HERDR_PANE_ID") {
        // A side panel, not half the screen. Sized here, before the first
        // frame, so nothing is drawn for the wider pane.
        if let Some(amount) = shrink_amount(herdr, &pane) {
            let _ = Command::new(herdr)
                .args(["pane", "resize", "--pane", &pane, "--direction", "right"])
                .arg("--amount")
                .arg(format!("{amount:.3}"))
                .output();
        }
        // Tell the toggle which pane is the panel in this workspace.
        if let Some(ws) = workspace_id() {
            let _ = std::fs::write(marker(state_dir, &ws), pane);
        }
    }
    // GitHub is asked off the drawing thread; one request at a time. Clicks
    // arrive on the same channel.
    let (done, events) = mpsc::channel::<Event>();
    listen_for_clicks(done.clone());
    let mut fetching = false;
    let mut last_frame = String::new();
    // What each drawn line does when clicked.
    let mut targets: Vec<Target> = Vec::new();
    // The rows the keys move between, and the selected one, by key so it
    // stays put as the list changes.
    let mut items: Vec<Item> = Vec::new();
    let mut selected: Option<String> = None;
    // Where a left drag started, as a line in the frame.
    let mut drag_from: Option<usize> = None;
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
    loop {
        let agents = agents(herdr);
        // Only this workspace's agents: a panel in each workspace follows its
        // own, not whichever agent has focus elsewhere.
        let focused = agents.iter().find(|a| {
            a["focused"].as_bool() == Some(true)
                && workspace
                    .as_deref()
                    .is_none_or(|ws| a["workspace_id"].as_str() == Some(ws))
        });
        let opener = start
            .as_deref()
            .and_then(|pane| agents.iter().find(|a| a["pane_id"].as_str() == Some(pane)));
        // Focus wins once it is on an agent; until then, the opener's pane.
        if let Some(agent) = focused.or(if last_agent.is_none() { opener } else { None }) {
            last_agent = Some(agent.clone());
        }
        let frame = match &last_agent {
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
                // Every checkout's diff at once: git is the slow part of a tick.
                let diffs: Vec<Option<String>> = std::thread::scope(|s| {
                    let running: Vec<_> =
                        tops.iter().map(|top| s.spawn(|| local_diff(top))).collect();
                    running
                        .into_iter()
                        .map(|t| t.join().ok().flatten())
                        .collect()
                });
                let local: Vec<Local> = tops
                    .iter()
                    .zip(diffs)
                    .filter_map(|(top, diff)| {
                        let (repo, slug) = names
                            .entry(top.clone())
                            .or_insert_with(|| (repo_name(top), github_slug(top)));
                        Some(Local {
                            repo: repo.clone()?,
                            diff: diff?,
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
        let deadline = Instant::now() + TICK;
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
                Ok(Event::Press(y)) => {
                    let line = y.saturating_sub(1) + view.scroll;
                    view.dragged = None;
                    drag_from = Some(line);
                }
                Ok(Event::Drag(y)) => {
                    if let Some(from) = drag_from {
                        let to = y.saturating_sub(1) + view.scroll;
                        let range = Some((from.min(to), from.max(to)));
                        if range != view.dragged {
                            view.dragged = range;
                            draw(&last_frame, view, None);
                        }
                    }
                }
                Ok(Event::Release) => {
                    drag_from = None;
                    if let Some((first, last)) = view.dragged.take() {
                        copy(&plain_lines(&last_frame, first, last));
                        draw(&last_frame, view, None);
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
                // diff, c its latest comment.
                Ok(Event::Key(key)) => {
                    let at = selected
                        .as_ref()
                        .and_then(|key| items.iter().position(|i| &i.key == key));
                    let to = match (key, at) {
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
                                b'd' => item.diff.as_deref(),
                                b'c' => item.comment.as_deref(),
                                _ => None,
                            });
                            if let Some(url) = url {
                                open_url(url);
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

/// Every agent pane Herdr knows, or none when the call fails.
fn agents(herdr: &str) -> Vec<Value> {
    Command::new(herdr)
        .args(["agent", "list"])
        .output()
        .ok()
        .and_then(|out| serde_json::from_slice::<Value>(&out.stdout).ok())
        .and_then(|v| v["result"]["agents"].as_array().cloned())
        .unwrap_or_default()
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

const GREEN: &str = "\x1b[38;2;166;227;161m";
const PURPLE: &str = "\x1b[38;2;203;166;247m";
const RED: &str = "\x1b[38;2;243;139;168m";
const DIM: &str = "\x1b[2m";
/// The selected or pointed-at link: underlined on a raised background, which
/// shows even where underline is not drawn.
const HL_ON: &str = "\x1b[4;48;2;69;71;90m";
const HL_OFF: &str = "\x1b[24;49m";
/// The row the keys selected: a bar across the pane.
const SELECTED: &str = "\x1b[48;2;49;50;68m";
/// Rows a drag covers, to be copied.
const DRAGGED: &str = "\x1b[48;2;88;91;112m";
const RESET: &str = "\x1b[0m";

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
        let repo = cut(
            &local.repo,
            width.saturating_sub(4 + cols(&local.diff)).max(4),
        );
        items.push(Item {
            key: format!("local:{}", local.repo),
            lines: (out.len(), out.len()),
            open: local.url.clone(),
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
            diff: Some(format!("{}/files", pr.url)),
            comment: Some(pr.comment_url.clone().unwrap_or_else(|| pr.url.clone())),
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
    /// p, d, c.
    pr: Option<String>,
    diff: Option<String>,
    comment: Option<String>,
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
    let _ = stdout.write_all(out.as_bytes());
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
enum Event {
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
    /// Left button pressed or dragged, on a 1-based row, or released.
    Press(usize),
    Drag(usize),
    Release,
}

/// Turns on mouse reporting in the pane and sends each Ctrl+left click, as
/// 1-based column and row, and each wheel notch to `events`.
fn listen_for_clicks(events: mpsc::Sender<Event>) {
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
            if !matches!(byte[0], b'M' | b'm') || !buf.starts_with(b"\x1b[<") {
                continue;
            }
            let seq = String::from_utf8_lossy(&buf[3..buf.len() - 1]).to_string();
            let press = byte[0] == b'M';
            buf.clear();
            let n: Vec<usize> = seq.split(';').filter_map(|n| n.parse().ok()).collect();
            let event = match n[..] {
                // Ctrl adds 16 to the button: links open on Ctrl+click only,
                // so a plain click just focuses the pane.
                [16, x, y] if press => Event::Click { x, y },
                // A plain left drag selects rows to copy.
                [0, _, y] if press => Event::Press(y),
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
    repo: String,
    diff: String,
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

/// `+a -d` the checkout at `top` has not committed; `None` when clean.
fn local_diff(top: &str) -> Option<String> {
    diff_value(&git_in(top, &["diff", "--shortstat", "HEAD"])?)
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
fn size() -> (usize, usize) {
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

fn cols(s: &str) -> usize {
    s.chars().map(char_cols).sum()
}

/// `s` cut to `n` columns, with an ellipsis when cut.
fn cut(s: &str, n: usize) -> String {
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
        // One row for the PR, over its title and details; d and c open its
        // files and latest comment.
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].lines, (0, 1));
        assert_eq!(
            items[0].diff.as_deref(),
            Some("https://github.com/o/repo/pull/7/files")
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
}
