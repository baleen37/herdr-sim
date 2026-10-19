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

use herdr_sim::{Cache, Session, claude_session, codex_session, resolve_claude_session};
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
    checks: Option<(usize, usize)>,
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
    // The last details seen, so a reopened panel draws its PRs before GitHub
    // answers.
    let saved = state_dir.join("changes-prs.json");
    let mut prs = load_prs(&saved);
    // Tell the toggle which pane is the panel in this workspace.
    if let (Some(ws), Ok(pane)) = (workspace_id(), std::env::var("HERDR_PANE_ID")) {
        let _ = std::fs::write(marker(state_dir, &ws), pane);
    }
    // GitHub is asked off the drawing thread; one request at a time.
    let (done, fetched) = mpsc::channel::<(Vec<String>, HashMap<String, Pr>)>();
    let mut fetching = false;
    let mut last_frame = String::new();
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
                        let _ = done.send((stale, got));
                    });
                }
                render(session.and_then(|s| s.title), &list, &prs, fetching)
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
        // Wake early when GitHub answers, so its details show at once.
        if let Ok((asked, mut got)) = fetched.recv_timeout(TICK) {
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
                    Some((c.first()?.as_u64()? as usize, c.get(1)?.as_u64()? as usize))
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
                    "checks": p.checks.map(|(ok, total)| [ok, total]),
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
    query += " }";
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
    asked
        .into_iter()
        .enumerate()
        .filter_map(|(i, url)| {
            let pr = &v["data"][format!("p{i}")]["pullRequest"];
            let title = pr["title"].as_str()?.to_string();
            // Counts per state, not every check: a small answer on busy PRs.
            let contexts = &pr["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"];
            let (mut passed, mut total) = (0, 0);
            for kind in ["checkRunCountsByState", "statusContextCountsByState"] {
                for c in contexts[kind].as_array().into_iter().flatten() {
                    let n = c["count"].as_u64().unwrap_or(0) as usize;
                    total += n;
                    if matches!(c["state"].as_str(), Some("SUCCESS" | "NEUTRAL" | "SKIPPED")) {
                        passed += n;
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
                checks: (total > 0).then_some((passed, total)),
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
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

fn render(title: Option<String>, urls: &[String], prs: &Prs, fetching: bool) -> String {
    let width = columns().saturating_sub(1).max(20);
    let mut s = format!("{BOLD}Changes{RESET}\n");
    if let Some(title) = title {
        s += &format!("{DIM}{}{RESET}\n", cut(&title, width));
    }
    s += "\n";
    if urls.is_empty() {
        s += &format!("{DIM}No PRs in this session.{RESET}\n");
    }
    for url in urls {
        let Some((_, Some(pr))) = prs.get(url) else {
            let note = if fetching && !prs.contains_key(url) {
                "loading…"
            } else {
                "could not load"
            };
            s += &format!(
                "{DIM}{}{RESET}\n{DIM}  ({note}){RESET}\n\n",
                cut(url, width)
            );
            continue;
        };
        let (icon, colour) = match (pr.state.as_str(), pr.draft) {
            ("MERGED", _) => ("\u{f419}", PURPLE),
            ("CLOSED", _) => ("\u{f4dc}", RED),
            (_, true) => ("\u{f407}", DIM),
            _ => ("\u{f407}", GREEN),
        };
        // OSC 8 makes the title a link to the PR.
        let title = cut(&pr.title, width.saturating_sub(2));
        s += &format!(
            "{colour}{icon}{RESET} \x1b]8;;{}\x1b\\{title}\x1b]8;;\x1b\\\n",
            pr.url
        );
        let mut meta = format!("  {DIM}{} #{}{RESET}", pr.repo, pr.number);
        meta += &format!(
            "  {GREEN}+{}{RESET} {RED}-{}{RESET}",
            pr.additions, pr.deletions
        );
        if pr.comments > 0 {
            meta += &format!("  {DIM}\u{f41f} {}{RESET}", pr.comments);
        }
        if let Some((ok, total)) = pr.checks {
            let c = if ok == total { GREEN } else { DIM };
            meta += &format!("  {c}{ok}/{total}{RESET}");
        }
        s += &meta;
        s += "\n\n";
    }
    s
}

/// The pane's width in columns, from the terminal it runs in.
fn columns() -> usize {
    Command::new("stty")
        .arg("size")
        .stdin(Stdio::inherit())
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.split_whitespace().nth(1)?.parse().ok())
        .unwrap_or(40)
}

/// `s` cut to `n` characters, with an ellipsis when cut.
fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('…');
    out
}
