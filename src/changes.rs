//! The Changes pane: Claude Code's "Changes" panel in a Herdr split. It lists
//! the PRs the focused agent's session touched — state, title, repo and
//! number, comments, checks, and lines changed — and follows focus between
//! agent panes. `--toggle-changes` opens it on the right or closes it.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use herdr_sim::{Cache, Session, claude_session, codex_session, resolve_claude_session};
use serde_json::Value;

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
/// is already open in this workspace.
pub fn toggle(herdr: &str, state_dir: &Path) -> anyhow::Result<()> {
    // One marker per workspace, so the key in one workspace never closes the
    // panel open in another.
    let workspace = std::env::var("HERDR_WORKSPACE_ID").unwrap_or_default();
    let marker = state_dir.join(format!(
        "changes-pane-{}",
        workspace.replace(['/', ':'], "_")
    ));
    if let Ok(pane) = fs::read_to_string(&marker) {
        let pane = pane.trim();
        let closed = Command::new(herdr)
            .args(["plugin", "pane", "close", pane])
            .output()
            .is_ok_and(|o| o.status.success());
        let _ = fs::remove_file(&marker);
        if closed {
            return Ok(());
        }
        // The pane was already gone (closed by hand): open a new one.
    }
    let mut open = Command::new(herdr);
    open.args(["plugin", "pane", "open", "--plugin", PLUGIN])
        .args(["--entrypoint", ENTRYPOINT])
        .args(["--placement", "split", "--direction", "right", "--no-focus"]);
    // Start on the pane the key was pressed in; after that, follow focus.
    if let Ok(pane) = std::env::var("HERDR_PANE_ID") {
        open.args(["--env", &format!("{FOR_PANE}={pane}")]);
    }
    let out = open.output()?;
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let pane = v
        .pointer("/result/pane/pane_id")
        .or_else(|| v.pointer("/result/pane_id"))
        .and_then(Value::as_str);
    if let Some(pane) = pane {
        fs::write(&marker, pane)?;
        // A side panel, not half the screen: move its edge right until it is
        // about PANEL_COLS wide.
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
    }
    Ok(())
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

/// Runs the pane until it is closed.
pub fn run(herdr: &str, home: &Path, state_dir: &Path) -> anyhow::Result<()> {
    // Read the main pass's scan cache once and keep scanning in memory; never
    // write it back, so this pane cannot race the plugin's own runs.
    let mut cache = Cache::load(&state_dir.join("pr-cache.json"));
    let mut prs: HashMap<String, (Instant, Option<Pr>)> = HashMap::new();
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
                let mut list = Vec::new();
                // Newest first, as in Claude Code's Changes panel.
                for url in session
                    .as_ref()
                    .map(|s| s.prs.iter().rev().cloned().collect::<Vec<_>>())
                    .unwrap_or_default()
                {
                    let fresh = prs.get(&url).is_some_and(|(at, pr)| {
                        let open = pr.as_ref().is_none_or(|p| p.state == "OPEN");
                        at.elapsed() < if open { OPEN_TTL } else { DONE_TTL }
                    });
                    if !fresh {
                        prs.insert(url.clone(), (Instant::now(), fetch(&url)));
                    }
                    list.push(url);
                }
                render(session.and_then(|s| s.title), &list, &prs)
            }
            None => "No agent focused yet.\n".to_string(),
        };
        let mut out = io::stdout();
        // Home, clear, frame.
        let _ = write!(out, "\x1b[H\x1b[2J{frame}");
        let _ = out.flush();
        sleep(TICK);
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

fn fetch(url: &str) -> Option<Pr> {
    let fields = "title,number,state,isDraft,additions,deletions,comments,statusCheckRollup";
    // The same 10 s deadline as the plugin's other gh calls: a stalled
    // network must not freeze the panel.
    let out = crate::gh(&["pr", "view", url, "--json", fields], None)?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let rollup = v["statusCheckRollup"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let passed = rollup
        .iter()
        .filter(|c| {
            let r = c["conclusion"]
                .as_str()
                .or(c["state"].as_str())
                .unwrap_or("");
            matches!(r, "SUCCESS" | "NEUTRAL" | "SKIPPED")
        })
        .count();
    Some(Pr {
        url: url.to_string(),
        repo: url.split('/').nth(4).unwrap_or("").to_string(),
        number: v["number"].to_string(),
        title: v["title"].as_str().unwrap_or("").to_string(),
        state: v["state"].as_str().unwrap_or("").to_string(),
        draft: v["isDraft"].as_bool().unwrap_or(false),
        additions: v["additions"].as_u64().unwrap_or(0),
        deletions: v["deletions"].as_u64().unwrap_or(0),
        comments: v["comments"].as_array().map_or(0, Vec::len),
        checks: (!rollup.is_empty()).then_some((passed, rollup.len())),
    })
}

const GREEN: &str = "\x1b[38;2;166;227;161m";
const PURPLE: &str = "\x1b[38;2;203;166;247m";
const RED: &str = "\x1b[38;2;243;139;168m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

fn render(
    title: Option<String>,
    urls: &[String],
    prs: &HashMap<String, (Instant, Option<Pr>)>,
) -> String {
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
            s += &format!(
                "{DIM}{}{RESET}\n{DIM}  (could not load){RESET}\n\n",
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
