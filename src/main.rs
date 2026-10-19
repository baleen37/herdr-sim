//! Herdr plugin entry point: set `$pr_open` / `$pr_merged` / `$pr_closed` on
//! every Claude Code or Codex pane from the PRs its session touched, plus
//! `$agent_icon` with the agent's logo, and on
//! every workspace from the PR of the branch it has checked out.
//!
//! Herdr runs this at server startup, on `pane.agent_status_changed`, and when
//! a worktree workspace is created or opened.

use std::collections::{HashMap, HashSet};
use std::env;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use herdr_sim::{AGENT_ICON_TOKEN, Cache, PrState, agent_icon, claude_prs, codex_prs, token_value};
use serde_json::Value;

fn main() -> Result<()> {
    let herdr = env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
    let home = PathBuf::from(env::var("HOME").context("HOME is not set")?);
    let state_dir = env::var("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| env::temp_dir().join("herdr-sim"));
    std::fs::create_dir_all(&state_dir)?;
    let cache_path = state_dir.join("pr-cache.json");
    let mut cache = Cache::load(&cache_path);
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

    let list: Value = serde_json::from_slice(&run(&herdr, &["agent", "list"])?)?;
    for agent in list["result"]["agents"].as_array().into_iter().flatten() {
        let kind = agent["agent"].as_str().unwrap_or_default();
        let (Some(pane), Some(icon)) = (agent["pane_id"].as_str(), agent_icon(kind)) else {
            continue;
        };
        let icon = [(AGENT_ICON_TOKEN, icon)];
        // The logo needs no session; the PRs do, so leave those as they are.
        let Some(session) = agent["agent_session"]["value"].as_str() else {
            report(&herdr, "pane", pane, None, &icon);
            continue;
        };
        let urls = if kind == "claude" {
            claude_prs(&home, session, &mut cache)
        } else {
            codex_prs(&home, session, &mut cache)
        };

        let mut counts: HashMap<PrState, usize> = HashMap::new();
        for url in &urls {
            if let Some(state) = cache.state(url, now, gh_pr_state) {
                *counts.entry(state).or_default() += 1;
            }
        }

        report(&herdr, "pane", pane, Some(&counts), &icon);
    }

    // A workspace shows the PR of the branch checked out in its first pane
    // that sits in a git repository.
    // Failing here must not lose what the agent pass learned, so it only skips.
    let panes: Value = run(&herdr, &["pane", "list"])
        .ok()
        .and_then(|out| serde_json::from_slice(&out).ok())
        .unwrap_or_default();
    let mut done = HashSet::new();
    let mut workspaces = HashSet::new();
    for pane in panes["result"]["panes"].as_array().into_iter().flatten() {
        let (Some(workspace), Some(cwd)) = (pane["workspace_id"].as_str(), pane["cwd"].as_str())
        else {
            continue;
        };
        workspaces.insert(workspace);
        if done.contains(workspace) {
            continue;
        }
        let Some((repo, branch)) = git_branch(cwd) else {
            continue;
        };
        done.insert(workspace);
        let key = format!("{repo}#{branch}");
        let counts = cache
            .branch_state(&key, now, || gh_branch_pr_state(&repo, &branch))
            .map(|state| HashMap::from([(state, 1)]))
            .unwrap_or_default();
        report(&herdr, "workspace", workspace, Some(&counts), &[]);
    }
    for workspace in workspaces.difference(&done) {
        report(&herdr, "workspace", workspace, Some(&HashMap::new()), &[]);
    }

    cache.save(&cache_path)?;
    Ok(())
}

/// Sets `tokens` on a pane or workspace, and sets or clears the three PR
/// tokens from `counts` unless it is `None`.
fn report(
    herdr: &str,
    kind: &str,
    id: &str,
    counts: Option<&HashMap<PrState, usize>>,
    tokens: &[(&str, &str)],
) {
    let mut args: Vec<String> = [kind, "report-metadata", id, "--source", "herdr-sim"]
        .map(String::from)
        .to_vec();
    for (name, value) in tokens {
        args.extend(["--token".into(), format!("{name}={value}")]);
    }
    if let Some(counts) = counts {
        for state in PrState::ALL {
            match token_value(state, counts.get(&state).copied().unwrap_or(0)) {
                Some(value) => {
                    args.extend(["--token".into(), format!("{}={value}", state.token())])
                }
                None => args.extend(["--clear-token".into(), state.token().into()]),
            }
        }
    }
    // A pane or workspace can close between listing it and here; keep going.
    if let Err(err) = run(herdr, &args) {
        eprintln!("herdr-sim: {id}: {err:#}");
    }
}

/// Repository root and checked-out branch for `cwd`; `None` outside a
/// repository or on a detached HEAD.
fn git_branch(cwd: &str) -> Option<(String, String)> {
    let out = Command::new("git")
        .args([
            "-C",
            cwd,
            "rev-parse",
            "--show-toplevel",
            "--abbrev-ref",
            "HEAD",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let mut lines = text.lines();
    let (repo, branch) = (lines.next()?, lines.next()?);
    (branch != "HEAD").then(|| (repo.to_string(), branch.to_string()))
}

/// `Some(None)` when the branch has no PR; `None` when `gh` fails otherwise.
fn gh_branch_pr_state(repo: &str, branch: &str) -> Option<Option<PrState>> {
    let out = Command::new("gh")
        .args(["pr", "view", branch, "--json", "state"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return stderr
            .to_ascii_lowercase()
            .contains("no pull requests found")
            .then_some(None);
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    Some(PrState::parse(v["state"].as_str()?))
}

fn gh_pr_state(url: &str) -> Option<PrState> {
    let out = Command::new("gh")
        .args(["pr", "view", url, "--json", "state"])
        .output()
        .ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    PrState::parse(v["state"].as_str()?)
}

fn run(program: &str, args: &[impl AsRef<std::ffi::OsStr> + std::fmt::Debug]) -> Result<Vec<u8>> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!(
            "{program} {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(out.stdout)
}
