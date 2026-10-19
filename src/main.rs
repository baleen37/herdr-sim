//! Herdr plugin entry point: set `$pr` (plus `$pr_open` / `$pr_merged` /
//! `$pr_closed`) on every Claude Code or Codex pane from the PRs its session
//! touched, plus `$agent_icon` with the agent's logo, and on every workspace
//! from the PR of the branch it has checked out.
//!
//! Herdr runs this at server startup, on `pane.agent_status_changed`, and when
//! a worktree workspace is created or opened. A finished turn only revisits
//! its own pane and workspace, a started turn does nothing, and runs never
//! overlap: one that finds another in progress leaves a note for it instead.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use herdr_sim::{
    AGENT_ICON_TOKEN, Cache, PR_SUMMARY_TOKEN, PrState, Scope, agent_icon, claude_prs, codex_prs,
    scope, summary_value, token_value,
};
use serde_json::Value;

/// Longest a single `gh` call may take before the run gives up on it.
const GH_TIMEOUT: Duration = Duration::from_secs(10);

struct Run {
    herdr: String,
    home: PathBuf,
    now: u64,
    cache: Cache,
}

fn main() -> Result<()> {
    let event = env::var("HERDR_PLUGIN_EVENT").ok();
    let mut scope = scope(
        event.as_deref(),
        env::var("HERDR_PLUGIN_EVENT_JSON").ok().as_deref(),
    );
    if scope == Scope::Skip {
        return Ok(());
    }

    let state_dir = env::var("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| env::temp_dir().join("herdr-sim"));
    fs::create_dir_all(&state_dir)?;
    let pending = state_dir.join("pending");
    let lock = File::create(state_dir.join("lock"))?;
    if lock.try_lock().is_err() {
        // The running pass will pick this up when it finishes.
        fs::write(&pending, b"")?;
        return Ok(());
    }

    let cache_path = state_dir.join("pr-cache.json");
    let mut run = Run {
        herdr: env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into()),
        home: PathBuf::from(env::var("HOME").context("HOME is not set")?),
        now: 0,
        cache: Cache::load(&cache_path),
    };
    if event.as_deref() == Some("startup") {
        run.cache.forget_all_reported();
    }
    loop {
        let _ = fs::remove_file(&pending);
        run.now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        run.pass(&scope);
        run.cache.save(&cache_path)?;
        // Events that arrived meanwhile did not say which pane they were for.
        if !pending.exists() {
            return Ok(());
        }
        scope = Scope::All;
    }
}

impl Run {
    fn pass(&mut self, scope: &Scope) {
        let agents = match scope {
            Scope::Pane { pane, .. } => {
                let pane = self.json(&["pane", "get", pane])["result"]["pane"].take();
                if pane.is_object() {
                    vec![pane]
                } else {
                    Vec::new()
                }
            }
            _ => self.json(&["agent", "list"])["result"]["agents"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        };
        for agent in &agents {
            self.agent(agent);
        }

        let only = match scope {
            Scope::Pane { workspace, .. } => Some(workspace.as_str()),
            _ => None,
        };
        let panes = self.json(&["pane", "list"]);
        let mut done = HashSet::new();
        let mut workspaces = HashSet::new();
        for pane in panes["result"]["panes"].as_array().into_iter().flatten() {
            let (Some(workspace), Some(cwd)) =
                (pane["workspace_id"].as_str(), pane["cwd"].as_str())
            else {
                continue;
            };
            if only.is_some_and(|w| w != workspace) {
                continue;
            }
            workspaces.insert(workspace);
            if done.contains(workspace) {
                continue;
            }
            let Some((repo, branch)) = git_branch(cwd) else {
                continue;
            };
            done.insert(workspace);
            let key = format!("{repo}#{branch}");
            let counts = self
                .cache
                .branch_state(&key, self.now, || gh_branch_pr_state(&repo, &branch))
                .map(|state| HashMap::from([(state, 1)]))
                .unwrap_or_default();
            self.report("workspace", workspace, Some(&counts), &[]);
        }
        for workspace in workspaces.difference(&done) {
            self.report("workspace", workspace, Some(&HashMap::new()), &[]);
        }
    }

    fn agent(&mut self, agent: &Value) {
        let kind = agent["agent"].as_str().unwrap_or_default();
        let (Some(pane), Some(icon)) = (agent["pane_id"].as_str(), agent_icon(kind)) else {
            return;
        };
        let icon = [(AGENT_ICON_TOKEN, icon)];
        // The logo needs no session; the PRs do, so leave those as they are.
        let Some(session) = agent["agent_session"]["value"].as_str() else {
            self.report("pane", pane, None, &icon);
            return;
        };
        let urls = if kind == "claude" {
            claude_prs(&self.home, session, &mut self.cache)
        } else {
            codex_prs(&self.home, session, &mut self.cache)
        };
        let mut counts: HashMap<PrState, usize> = HashMap::new();
        for url in &urls {
            if let Some(state) = self.cache.state(url, self.now, gh_pr_state) {
                *counts.entry(state).or_default() += 1;
            }
        }
        self.report("pane", pane, Some(&counts), &icon);
    }

    /// Sets `tokens` on a pane or workspace, and sets or clears the PR tokens
    /// (`$pr` and the three per-state ones) from `counts` unless it is `None`.
    /// Skips the call when it would send what was sent last time.
    fn report(
        &mut self,
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
            match summary_value(counts) {
                Some(value) => {
                    args.extend(["--token".into(), format!("{PR_SUMMARY_TOKEN}={value}")])
                }
                None => args.extend(["--clear-token".into(), PR_SUMMARY_TOKEN.into()]),
            }
            for state in PrState::ALL {
                match token_value(state, counts.get(&state).copied().unwrap_or(0)) {
                    Some(value) => {
                        args.extend(["--token".into(), format!("{}={value}", state.token())])
                    }
                    None => args.extend(["--clear-token".into(), state.token().into()]),
                }
            }
        }
        let key = format!("{kind}:{id}");
        if !self.cache.changed(&key, &args[3..].join("\u{1f}")) {
            return;
        }
        // A pane or workspace can close between listing it and here; keep going.
        if let Err(err) = run(&self.herdr, &args) {
            self.cache.forget_reported(&key);
            eprintln!("herdr-sim: {id}: {err:#}");
        }
    }

    /// A Herdr CLI call's JSON, or `Null` when it fails: one failed lookup
    /// must not cost the rest of the pass.
    fn json(&self, args: &[&str]) -> Value {
        run(&self.herdr, args)
            .ok()
            .and_then(|out| serde_json::from_slice(&out).ok())
            .unwrap_or_default()
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
    let out = gh(
        &["pr", "view", branch, "--json", "state"],
        Some(Path::new(repo)),
    )?;
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
    let out = gh(&["pr", "view", url, "--json", "state"], None)?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    PrState::parse(v["state"].as_str()?)
}

/// Runs `gh`, killing it after [`GH_TIMEOUT`] so a stalled network cannot hold
/// the lock. Its output is a few hundred bytes, well under the pipe buffer.
fn gh(args: &[&str], cwd: Option<&Path>) -> Option<Output> {
    let mut cmd = Command::new("gh");
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + GH_TIMEOUT;
    while child.try_wait().ok()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        sleep(Duration::from_millis(20));
    }
    child.wait_with_output().ok()
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
