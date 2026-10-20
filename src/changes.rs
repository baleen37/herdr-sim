//! The Changes pane: Claude Code's "Changes" panel in a Herdr split. It lists
//! the PRs the focused agent's session touched — state, title, repo and
//! number, comments, checks, and lines changed — and follows focus between
//! agent panes. `--toggle-changes` opens it on the right or closes it.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread::sleep;
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
    additions: u64,
    deletions: u64,
    comments: usize,
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
        .args(["--placement", "split", "--direction", "right", "--no-focus"]);
    // Start on the pane the key was pressed in; after that, follow focus.
    if let Ok(pane) = std::env::var("HERDR_PANE_ID") {
        open.args(["--env", &format!("{FOR_PANE}={pane}")]);
    }
    open.output()?;
    // The panel's process starts just after the open returns; find it to size
    // it as a side panel, not half the screen.
    for _ in 0..20 {
        if let Some(pane) = changes_panes(herdr, workspace.as_deref()).first() {
            if let Some(amount) = shrink_amount(herdr, pane) {
                let _ = Command::new(herdr)
                    .args([
                        "pane",
                        "resize",
                        "--pane",
                        pane,
                        "--direction",
                        "right",
                        "--amount",
                    ])
                    .arg(format!("{amount:.3}"))
                    .output();
            }
            break;
        }
        sleep(Duration::from_millis(50));
    }
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
    // Each checkout's repo name, which comes from its remote.
    let mut names: HashMap<String, String> = HashMap::new();
    // The last details seen, so a reopened panel draws its PRs before GitHub
    // answers.
    let saved = state_dir.join("changes-prs.json");
    let mut prs = load_prs(&saved);
    // Tell the toggle which pane is the panel in this workspace.
    if let (Some(ws), Ok(pane)) = (workspace_id(), std::env::var("HERDR_PANE_ID")) {
        let _ = std::fs::write(marker(state_dir, &ws), pane);
    }
    // GitHub is asked off the drawing thread; one request at a time. Clicks
    // arrive on the same channel.
    let (done, events) = mpsc::channel::<Event>();
    listen_for_clicks(done.clone());
    let mut fetching = false;
    let mut last_frame = String::new();
    // What each drawn line does when clicked, and the PR whose menu is open.
    let mut targets: Vec<Target> = Vec::new();
    let mut menu: Option<String> = None;
    let mut last_agent: Option<Value> = None;
    let start = std::env::var(FOR_PANE).ok();
    loop {
        let agents = agents(herdr);
        let focused = agents.iter().find(|a| a["focused"].as_bool() == Some(true));
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
                let mut tops: Vec<String> = Vec::new();
                for dir in dirs {
                    let top = checkouts
                        .entry(dir)
                        .or_insert_with_key(|dir| checkout_of(dir));
                    if let Some(top) = top
                        && !tops.contains(top)
                    {
                        tops.push(top.clone());
                    }
                }
                let local: Vec<Local> = tops.iter().filter_map(|top| local_changes(top)).collect();
                // The agent's own checkout leads the environment when it is one.
                let own = agent["cwd"]
                    .as_str()
                    .and_then(|cwd| checkouts.get(cwd).cloned().flatten());
                let env = environment(&tops, own.as_deref(), home, &mut names);
                let (changes, clicks) = render(&list, &prs, &local, menu.as_deref());
                // Environment rows do nothing when clicked.
                targets = vec![Target::None; env.lines().count()];
                targets.extend(clicks);
                // Only what fits: a longer frame scrolls the top rows off.
                let rows = size().0;
                (env + &changes)
                    .lines()
                    .take(rows)
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            None => "No agent focused yet.\n".to_string(),
        };
        // Redraw only on change: no flicker, no work for the terminal.
        if frame != last_frame {
            let mut out = io::stdout();
            // Home, clear, frame.
            let _ = write!(out, "\x1b[H\x1b[2J{frame}");
            let _ = out.flush();
            last_frame = frame;
        }
        // Wake early when GitHub answers or a click comes, so either shows at
        // once.
        let event = events.recv_timeout(TICK);
        if let Ok(Event::Click { x, y }) = event {
            let width = size().1.saturating_sub(1).max(20);
            let target = y.checked_sub(1).and_then(|row| targets.get(row)).cloned();
            // Any click closes an open menu; the ⋮ of another PR opens its own.
            let was = menu.take();
            match target {
                Some(Target::Title(url)) if x + 3 >= width => {
                    if was.as_deref() != Some(url.as_str()) {
                        menu = Some(url);
                    }
                }
                Some(Target::Title(url) | Target::Open(url)) => open_url(&url),
                Some(Target::Copy(text)) => copy(&text),
                Some(Target::None) | None => {}
            }
        }
        if let Ok(Event::Fetched(asked, mut got)) = event {
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
                additions: p["additions"].as_u64().unwrap_or(0),
                deletions: p["deletions"].as_u64().unwrap_or(0),
                comments: p["comments"].as_u64().unwrap_or(0) as usize,
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
                    "state": p.state, "draft": p.draft, "additions": p.additions,
                    "deletions": p.deletions, "comments": p.comments,
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
const PR_FIELDS: &str = "title number state isDraft additions deletions \
    comments { totalCount } \
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
                additions: pr["additions"].as_u64().unwrap_or(0),
                deletions: pr["deletions"].as_u64().unwrap_or(0),
                comments: pr["comments"]["totalCount"].as_u64().unwrap_or(0) as usize,
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
const YELLOW: &str = "\x1b[38;2;249;226;175m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

/// Laid out like Claude Code's Changes panel: per PR, its state icon and
/// title, then repo and number with lines changed, and comments and a checks
/// dot on the right.
fn render(
    urls: &[String],
    prs: &Prs,
    local: &[Local],
    menu: Option<&str>,
) -> (String, Vec<Target>) {
    let width = size().1.saturating_sub(1).max(20);
    let mut out: Vec<(String, Target)> = Vec::new();
    let blank = |out: &mut Vec<(String, Target)>| out.push((String::new(), Target::None));
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
        // "Create PR" opens GitHub's compare page for the branch.
        let button = "\u{f407} Create PR";
        let button_cols = cols(button);
        let repo = cut(
            &local.repo,
            width
                .saturating_sub(4 + cols(&local.diff) + button_cols)
                .max(4),
        );
        let gap = width
            .saturating_sub(3 + cols(&repo) + cols(&local.diff) + button_cols)
            .max(1);
        out.push((
            format!(
                "{DIM}±{RESET} {DIM}{repo}{RESET} {}{}{DIM}{button}{RESET}",
                lines.join(" "),
                " ".repeat(gap)
            ),
            local.compare.clone().map_or(Target::None, Target::Open),
        ));
    }
    if !local.is_empty() {
        blank(&mut out);
    } else if urls.is_empty() {
        out.push((format!("{DIM}No changes.{RESET}"), Target::None));
    }
    for url in urls {
        let Some((_, Some(pr))) = prs.get(url) else {
            // Until its details arrive, the PR is its repo and number.
            let parts: Vec<&str> = url.split('/').collect();
            let name = match (parts.get(4), parts.get(6)) {
                (Some(repo), Some(n)) => format!("{repo} #{n}"),
                _ => url.clone(),
            };
            out.push((
                format!("{DIM}\u{f407} {}{RESET}", cut(&name, width - 2)),
                Target::Open(url.clone()),
            ));
            blank(&mut out);
            blank(&mut out);
            continue;
        };
        let (icon, colour) = match (pr.state.as_str(), pr.draft) {
            ("MERGED", _) => ("\u{f419}", PURPLE),
            ("CLOSED", _) => ("\u{f4dc}", RED),
            (_, true) => ("\u{f4dd}", DIM),
            _ => ("\u{f407}", GREEN),
        };
        // Title, then the row's menu mark at the right edge.
        let title = cut(&pr.title, width - 4);
        let gap = (width - 3).saturating_sub(cols(&title)).max(1);
        let open = menu == Some(pr.url.as_str());
        let mark = if open { "" } else { DIM };
        out.push((
            format!(
                "{colour}{icon}{RESET} {title}{}{mark}\u{f142}{RESET}",
                " ".repeat(gap)
            ),
            Target::Title(pr.url.clone()),
        ));
        if open {
            out.extend(pr_menu(&pr.url, width));
        }
        let mut left = format!("{DIM}{} #{}{RESET}", pr.repo, pr.number);
        let mut left_cols = cols(&pr.repo) + 2 + pr.number.len();
        if pr.additions + pr.deletions > 0 {
            let lines = format!("+{} -{}", pr.additions, pr.deletions);
            left += &format!(
                "  {GREEN}+{}{RESET} {RED}-{}{RESET}",
                pr.additions, pr.deletions
            );
            left_cols += 2 + lines.len();
        }
        let mut right = String::new();
        let mut right_cols = 0;
        if pr.comments > 0 {
            let n = pr.comments.to_string();
            right += &format!("{DIM}\u{f41f} {n}{RESET}");
            right_cols += 2 + n.len();
        }
        // Checks matter only while the PR is open.
        if let Some((ok, failed, total)) = pr.checks.filter(|_| pr.state == "OPEN") {
            let c = if failed > 0 {
                RED
            } else if ok == total {
                GREEN
            } else {
                YELLOW
            };
            right += &format!("  {c}\u{25cf}{RESET}");
            right_cols += 3;
        }
        let gap = (width - 2).saturating_sub(left_cols + right_cols).max(1);
        out.push((
            format!("  {left}{}{right}", " ".repeat(gap)),
            Target::Open(pr.url.clone()),
        ));
        blank(&mut out);
    }
    let (lines, targets): (Vec<String>, Vec<Target>) = out.into_iter().unzip();
    (lines.join("\n"), targets)
}

/// The menu a PR's ⋮ opens, boxed under its title at the right edge.
fn pr_menu(url: &str, width: usize) -> Vec<(String, Target)> {
    let items = [
        ("Open in browser", Target::Open(url.to_string())),
        ("Files changed", Target::Open(format!("{url}/files"))),
        ("Checks", Target::Open(format!("{url}/checks"))),
        ("Copy link", Target::Copy(url.to_string())),
    ];
    let inner = items
        .iter()
        .map(|(label, _)| cols(label))
        .max()
        .unwrap_or(0)
        + 2;
    let indent = " ".repeat(width.saturating_sub(inner + 2));
    let edge = |l: &str, r: &str| format!("{indent}{DIM}{l}{}{r}{RESET}", "─".repeat(inner));
    let mut rows = vec![(edge("┌", "┐"), Target::None)];
    for (label, target) in items {
        let pad = " ".repeat(inner - 1 - cols(label));
        rows.push((
            format!("{indent}{DIM}│{RESET} {label}{pad}{DIM}│{RESET}"),
            target,
        ));
    }
    rows.push((edge("└", "┘"), Target::None));
    rows
}

/// What a click on a drawn line does.
#[derive(Clone)]
enum Target {
    None,
    /// A PR's title row: its ⋮ at the right edge opens the menu, the rest
    /// opens the PR.
    Title(String),
    Open(String),
    Copy(String),
}

/// What wakes the Changes loop besides its tick.
enum Event {
    Fetched(Vec<String>, HashMap<String, Pr>),
    Click { x: usize, y: usize },
}

/// Turns on mouse reporting in the pane and sends each left click, as
/// 1-based column and row, to `events`.
fn listen_for_clicks(events: mpsc::Sender<Event>) {
    // Unbuffered input without echo; output keeps its newlines.
    let _ = Command::new("stty")
        .args(["-icanon", "-echo"])
        .stdin(Stdio::inherit())
        .status();
    // Button events, in SGR encoding.
    print!("\x1b[?1000h\x1b[?1006h");
    let _ = io::stdout().flush();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while io::stdin().read(&mut byte).is_ok_and(|n| n == 1) {
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
            if press
                && let [0, x, y] = n[..]
                && events.send(Event::Click { x, y }).is_err()
            {
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

/// Uncommitted changes in a checkout: its repo, `+a -d`, and GitHub's page
/// for opening a PR from its branch.
struct Local {
    repo: String,
    diff: String,
    compare: Option<String>,
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

/// What the checkout at `cwd` has not committed; `None` when clean.
fn local_changes(cwd: &str) -> Option<Local> {
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .ok()?;
        let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    let diff = diff_value(&git(&["diff", "--shortstat", "HEAD"])?)?;
    let slug = github_slug(cwd);
    let repo = repo_name(cwd)?;
    let compare = slug
        .zip(git(&["branch", "--show-current"]))
        .map(|(slug, branch)| {
            // Branch names may hold `#`, `?`, `%`, or spaces; `/` stays as is.
            let branch: String = branch
                .bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect();
            format!("https://github.com/{slug}/compare/{branch}?expand=1")
        });
    Some(Local {
        repo,
        diff,
        compare,
    })
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

/// The Environment section, as in Claude Code: this computer, the repos the
/// session works in (the agent's own first, with where it lives), and the
/// agent's branch, marked when it is a linked worktree.
fn environment(
    tops: &[String],
    own: Option<&str>,
    home: &Path,
    names: &mut HashMap<String, String>,
) -> String {
    let width = size().1.saturating_sub(1).max(20);
    let mut s = format!("{BOLD}Environment{RESET} {DIM}\u{f47c}{RESET}\n");
    // Herdr's name for the machine the panes run on.
    s += "\u{f0322} local\n";
    let mut shown: Vec<String> = Vec::new();
    for top in tops {
        let name = names
            .entry(top.clone())
            .or_insert_with(|| repo_name(top).unwrap_or_else(|| top.clone()))
            .clone();
        if shown.contains(&name) {
            continue;
        }
        shown.push(name.clone());
        let mut row = format!("\u{ea83} {}", cut(&name, width - 2));
        if Some(top.as_str()) == own {
            // Where the repo lives: its main checkout's folder, even from a
            // linked worktree.
            let main = git_in(
                top,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .and_then(|d| Some(Path::new(&d).parent()?.parent()?.to_path_buf()));
            if let Some(parent) = main {
                let parent = parent.display().to_string();
                let home = home.display().to_string();
                let place = match parent.strip_prefix(&home) {
                    Some(rest) => format!("~{rest}"),
                    None => parent,
                };
                let room = width.saturating_sub(5 + cols(&name));
                if room > 3 {
                    row += &format!(" {DIM}\u{b7} {}{RESET}", cut(&place, room));
                }
            }
        }
        s += &row;
        s += "\n";
    }
    if let Some(top) = own
        && let Some(branch) = git_in(top, &["branch", "--show-current"])
    {
        let worktree = git_in(top, &["rev-parse", "--absolute-git-dir"])
            != git_in(
                top,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            );
        let label = if worktree { "Worktree" } else { "" };
        let room = width.saturating_sub(3 + cols(label));
        let branch = cut_middle(&branch, room);
        let gap = width.saturating_sub(2 + cols(&branch) + cols(label)).max(1);
        s += &format!("\u{ec6f} {branch}{}{DIM}{label}{RESET}\n", " ".repeat(gap));
    }
    s + "\n" + &format!("{BOLD}Changes{RESET} {DIM}\u{f47c}{RESET}\n")
}

/// `s` cut to `n` columns by dropping its middle, as branch names keep their
/// meaning at both ends.
fn cut_middle(s: &str, n: usize) -> String {
    if cols(s) <= n {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let keep = n.saturating_sub(1);
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let mut out: String = chars.iter().take(head).collect();
    out.push('…');
    out.extend(chars[chars.len() - tail..].iter());
    out
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
