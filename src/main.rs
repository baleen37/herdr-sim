//! Herdr plugin entry point: set `$pr` (plus `$pr_open` / `$pr_merged` /
//! `$pr_closed`) on every Claude Code or Codex pane from the PRs its session
//! touched, plus `$agent_icon` with the agent's logo, and on every workspace
//! from the PR of the branch it has checked out.
//!
//! Herdr runs this at server startup, on `pane.agent_status_changed`, and when
//! a worktree workspace is created or opened. A finished turn only revisits
//! its own pane and workspace, a started turn does nothing, and runs never
//! overlap: one that finds another in progress leaves a note for it instead.

use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use herdr_sim::{
    AGE_TOKEN, AGENT_ICON_TOKEN, AGENT_KINDS, Activity, BRANCH_TOKEN, BranchPr, CI_TOKEN, Cache,
    Ci, DIFF_TOKEN, HEADING_TOKEN, PR_SUMMARY_TOKEN, PROJECT_TOKEN, Place, PrState,
    SORT_RANK_TOKEN, STALE_HEADING_TOKEN, STATE_HEADING_TOKENS, Scope, TITLE_TOKEN, TREE_GAP_TOKEN,
    TREE_GROUP_STALE_TOKEN, TREE_GROUP_TOKEN, TREE_HEADER_TOKEN, TREE_PR_TOKEN, TREE_PROJECT_TOKEN,
    TREE_SPLIT_TOKEN, TREE_WORKTREE_TOKEN, activity_order, age_value, agent_count_value,
    agent_icon, checkout_headers, claude_session, codex_session, diff_value, group_rows,
    header_value, heading_value, held_status, indent, project_name, resolve_claude_session, scope,
    split_marks, state_heading_token, state_heading_value, summary_value, token_value,
    tree_headers, workspace_agent_token,
};
use serde_json::{Value, json};

/// Longest a single `gh` call may take before the run gives up on it.
const GH_TIMEOUT: Duration = Duration::from_secs(10);

struct Run {
    herdr: String,
    home: PathBuf,
    /// Whether the Agents list is in activity order.
    ordered: bool,
    now: u64,
    cache: Cache,
}

fn main() -> Result<()> {
    let event = env::var("HERDR_PLUGIN_EVENT").ok();
    let mut scope = scope(
        event.as_deref(),
        env::var("HERDR_PLUGIN_EVENT_JSON").ok().as_deref(),
    );
    let state_dir = env::var("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| env::temp_dir().join("herdr-sim"));
    fs::create_dir_all(&state_dir)?;
    // Present while the order is off; the toggle action flips it.
    let order_off = state_dir.join("order-off");
    if env::args().any(|a| a == "--toggle-order") {
        // Turning it on needs no call here: every pass below sets the view.
        if order_off.exists() {
            fs::remove_file(&order_off)?;
        } else {
            fs::write(&order_off, b"")?;
            set_agent_view(false);
        }
    }
    let pending = state_dir.join("pending");
    let lock = File::create(state_dir.join("lock"))?;
    let startup = event.as_deref() == Some("startup");
    if startup {
        // A restarted server lost the metadata; this run must resend it itself.
        lock.lock()?;
    } else {
        // Leave the note before trying the lock, so a holder that is about to
        // release it still sees the note afterwards.
        fs::write(&pending, b"")?;
        if lock.try_lock().is_err() {
            return Ok(());
        }
    }

    let cache_path = state_dir.join("pr-cache.json");
    let mut run = Run {
        herdr: env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into()),
        home: PathBuf::from(env::var("HOME").context("HOME is not set")?),
        ordered: false,
        now: 0,
        cache: Cache::load(&cache_path),
    };
    if startup {
        run.cache.forget_all_reported();
    }
    loop {
        let _ = fs::remove_file(&pending);
        // Every pass sets the view while the order is on: it dies with the
        // server, and a plugin linked into a running server gets no startup
        // hook. Only the toggle clears it, so another plugin's view stays.
        run.ordered = !order_off.exists();
        if run.ordered {
            set_agent_view(true);
        }
        run.now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        run.pass(&scope);
        run.cache.save(&cache_path)?;
        lock.unlock()?;
        // Notes left meanwhile do not say which pane they were for. If another
        // run took the lock first, it handles them.
        if !pending.exists() || lock.try_lock().is_err() {
            return Ok(());
        }
        run.cache = Cache::load(&cache_path);
        scope = Scope::All;
    }
}

impl Run {
    fn pass(&mut self, scope: &Scope) {
        let (only_pane, only_workspace) = match scope {
            Scope::Pane { pane, workspace } => (Some(pane.as_str()), Some(workspace.as_str())),
            _ => (None, None),
        };
        // One list serves both the pane pass and each Space's agent count.
        // Without the list every Space would look agent-free; change nothing.
        let Some(agents) = self.json(&["agent", "list"])["result"]["agents"]
            .take()
            .as_array()
            .cloned()
        else {
            return;
        };
        let mut per_workspace: HashMap<String, HashMap<String, usize>> = HashMap::new();
        for agent in &agents {
            if let (Some(workspace), Some(kind)) =
                (agent["workspace_id"].as_str(), agent["agent"].as_str())
            {
                *per_workspace
                    .entry(workspace.to_string())
                    .or_default()
                    .entry(kind.to_string())
                    .or_default() += 1;
            }
            if scope != &Scope::Status
                && only_pane.is_none_or(|p| agent["pane_id"].as_str() == Some(p))
            {
                self.agent(agent);
            }
        }

        let states = self.states(&agents);
        // A turn start or a focus changes only colours; tree order and
        // headers wait for a turn's end, as before.
        if scope == &Scope::Status {
            self.report_states(&agents, &states);
            return;
        }

        // Project and checkout (branch) headers over the Agents list. Herdr
        // shows it in the same order as `agent list` when sorted by Space; in
        // activity order the view sorts on the ranks set here, so the headers
        // follow the same order.
        let activity: Vec<Activity> = agents
            .iter()
            .zip(&states)
            .map(|(agent, (_, state))| Activity {
                place: agent["cwd"].as_str().map(|cwd| self.place(cwd)),
                tab: agent["tab_id"].as_str().unwrap_or_default().to_string(),
                seq: agent["state_change_seq"].as_u64().unwrap_or(0),
                stale: *state == STALE_HEADING_TOKEN,
            })
            .collect();
        // Agents without a place get no rank, so the view lists them last.
        let (order, ranked) = if self.ordered {
            let placed = activity.iter().filter(|a| a.place.is_some()).count();
            (activity_order(&activity), placed)
        } else {
            ((0..agents.len()).collect(), 0)
        };
        let places: Vec<Option<Place>> = order.iter().map(|&i| activity[i].place.clone()).collect();
        let tabs: Vec<&str> = order.iter().map(|&i| activity[i].tab.as_str()).collect();
        let marks = split_marks(&tabs);
        let stale: Vec<bool> = order.iter().map(|&i| activity[i].stale).collect();
        let headers = tree_headers(&places)
            .into_iter()
            .zip(checkout_headers(&places))
            .zip(group_rows(&places, &stale));
        for (pos, (((project, worktree), header), row)) in headers.enumerate() {
            let agent = &agents[order[pos]];
            let Some(pane) = agent["pane_id"].as_str() else {
                continue;
            };
            // The indent goes on the row's first cell: the split mark when
            // there is one, else the heading.
            let split = marks[pos].map(|mark| format!("{}{mark}", indent(row.depth)));
            self.cache
                .set_depth(pane, if split.is_some() { 0 } else { row.depth });
            let (group, group_stale) = if row.stale {
                (None, row.group)
            } else {
                (row.group, None)
            };
            // The branch PR rides on the checkout's card, from the same cache
            // the Space pass fills, so it costs no extra gh call.
            let card_pr = match (&worktree, &places[pos]) {
                (Some(_), Some(place)) => {
                    let (repo, branch) = (place.checkout.clone(), place.label.clone());
                    let key = format!("{repo}#{branch}");
                    let pr = self
                        .cache
                        .branch_state(&key, self.now, || gh_branch_pr(&repo, &branch));
                    pr.state.and_then(|state| token_value(state, 1))
                }
                _ => None,
            };
            let tokens = [
                (TREE_PROJECT_TOKEN.to_string(), project),
                (TREE_WORKTREE_TOKEN.to_string(), worktree),
                (TREE_PR_TOKEN.to_string(), card_pr),
                (TREE_HEADER_TOKEN.to_string(), header),
                (TREE_GROUP_TOKEN.to_string(), group),
                (TREE_GROUP_STALE_TOKEN.to_string(), group_stale),
                (TREE_GAP_TOKEN.to_string(), row.gap),
                (AGE_TOKEN.to_string(), self.age_secs(agent).map(age_value)),
                (TREE_SPLIT_TOKEN.to_string(), split),
                (
                    SORT_RANK_TOKEN.to_string(),
                    (pos < ranked).then(|| format!("{pos:04}")),
                ),
            ];
            self.report("pane", pane, "tree", None, &tokens);
        }
        self.report_states(&agents, &states);

        // A workspace shows the PR of the branch checked out in its first pane
        // that sits in a git repository, and the logos of its agents.
        let panes = self.json(&["pane", "list"]);
        let mut branch_prs: HashMap<String, HashMap<PrState, usize>> = HashMap::new();
        let mut cis: HashMap<String, Ci> = HashMap::new();
        let mut diffs: HashMap<String, String> = HashMap::new();
        let mut projects: HashMap<String, String> = HashMap::new();
        let mut headers: HashMap<String, String> = HashMap::new();
        let mut order = Vec::new();
        for pane in panes["result"]["panes"].as_array().into_iter().flatten() {
            let (Some(workspace), Some(cwd)) =
                (pane["workspace_id"].as_str(), pane["cwd"].as_str())
            else {
                continue;
            };
            if only_workspace.is_some_and(|w| w != workspace) {
                continue;
            }
            if !order.iter().any(|w| w == workspace) {
                order.push(workspace.to_string());
                // Until a pane in a repository turns up, name it after the directory.
                projects.insert(workspace.to_string(), dir_name(cwd, &self.home));
            }
            if branch_prs.contains_key(workspace) {
                continue;
            }
            let Some(repo) = git_repo(cwd) else {
                continue;
            };
            let place = repo.place();
            headers.insert(
                workspace.to_string(),
                header_value(&place.project, Some(&place.label)),
            );
            projects.insert(workspace.to_string(), place.project);
            if let Some(diff) = git_diff(&repo.top) {
                diffs.insert(workspace.to_string(), diff);
            }
            let Some(branch) = repo.branch else {
                branch_prs.insert(workspace.to_string(), HashMap::new());
                continue;
            };
            let repo = repo.top;
            let key = format!("{repo}#{branch}");
            let pr = self
                .cache
                .branch_state(&key, self.now, || gh_branch_pr(&repo, &branch));
            if let Some(ci) = pr.ci {
                cis.insert(workspace.to_string(), ci);
            }
            let counts = pr
                .state
                .map(|state| HashMap::from([(state, 1)]))
                .unwrap_or_default();
            branch_prs.insert(workspace.to_string(), counts);
        }
        for workspace in order {
            let kinds = per_workspace.get(&workspace);
            let mut tokens: Vec<(String, Option<String>)> = AGENT_KINDS
                .iter()
                .map(|kind| {
                    let count = kinds.and_then(|k| k.get(*kind)).copied().unwrap_or(0);
                    (workspace_agent_token(kind), agent_count_value(kind, count))
                })
                .collect();
            let project = projects.remove(&workspace);
            // Same header as the Agents list, so both panels read alike.
            let header = headers
                .remove(&workspace)
                .or_else(|| project.as_deref().map(|p| header_value(p, None)));
            tokens.push((TREE_HEADER_TOKEN.into(), header));
            tokens.push((PROJECT_TOKEN.into(), project));
            tokens.push((DIFF_TOKEN.into(), diffs.remove(&workspace)));
            let ci = cis.remove(&workspace).map(|ci| ci.icon().to_string());
            tokens.push((CI_TOKEN.into(), ci));
            let prs = branch_prs.remove(&workspace).unwrap_or_default();
            self.report("workspace", &workspace, "space", Some(&prs), &tokens);
        }
    }

    fn agent(&mut self, agent: &Value) {
        let kind = agent["agent"].as_str().unwrap_or_default();
        let (Some(pane), Some(icon)) = (agent["pane_id"].as_str(), agent_icon(kind)) else {
            return;
        };
        let (project, branch) = match agent["cwd"].as_str() {
            Some(cwd) => match git_repo(cwd) {
                Some(repo) => {
                    self.cache.set_place(cwd, repo.place());
                    (Some(repo.project), repo.branch)
                }
                None => (Some(dir_name(cwd, &self.home)), None),
            },
            None => (None, None),
        };
        let mut tokens = vec![
            (AGENT_ICON_TOKEN.to_string(), Some(icon.to_string())),
            (PROJECT_TOKEN.to_string(), project),
            (BRANCH_TOKEN.to_string(), branch),
        ];
        // The logo needs no session; the PRs and title do, so leave those be.
        let Some(session) = agent["agent_session"]["value"].as_str() else {
            let heading = self
                .cache
                .heading(pane)
                .unwrap_or_else(|| heading_value(icon, None));
            self.cache.set_heading(pane, heading.clone());
            tokens.push((HEADING_TOKEN.to_string(), Some(heading)));
            self.report("pane", pane, "agent", None, &tokens);
            return;
        };
        let session = if kind == "claude" {
            let session = resolve_claude_session(&self.home, session);
            claude_session(&self.home, &session, &mut self.cache)
        } else {
            codex_session(&self.home, session, &mut self.cache)
        };
        let mut counts: HashMap<PrState, usize> = HashMap::new();
        for url in &session.prs {
            if let Some(state) = self.cache.state(url, self.now, gh_pr_state) {
                *counts.entry(state).or_default() += 1;
            }
        }
        let heading = heading_value(icon, session.title.as_deref());
        self.cache.set_heading(pane, heading.clone());
        tokens.push((HEADING_TOKEN.to_string(), Some(heading)));
        tokens.push((TITLE_TOKEN.to_string(), session.title));
        self.report("pane", pane, "agent", Some(&counts), &tokens);
    }

    /// Sets `tokens` on a pane or workspace (clearing those that are `None`),
    /// and sets or clears the PR tokens
    /// (`$pr` and the three per-state ones) from `counts` unless it is `None`.
    /// Skips the call when it would send what was sent last time for the same
    /// `slot` (each part of a run that reports on a pane keeps its own record).
    fn report(
        &mut self,
        kind: &str,
        id: &str,
        slot: &str,
        counts: Option<&HashMap<PrState, usize>>,
        tokens: &[(String, Option<String>)],
    ) {
        let mut args: Vec<String> = [kind, "report-metadata", id, "--source", "herdr-sim"]
            .map(String::from)
            .to_vec();
        for (name, value) in tokens {
            match value {
                Some(value) => args.extend(["--token".into(), format!("{name}={value}")]),
                None => args.extend(["--clear-token".into(), name.clone()]),
            }
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
        let key = format!("{kind}:{id}:{slot}");
        if !self.cache.changed(&key, &args[3..].join("\u{1f}")) {
            return;
        }
        // A pane or workspace can close between listing it and here; keep going.
        if let Err(err) = run(&self.herdr, &args) {
            self.cache.forget_reported(&key);
            eprintln!("herdr-sim: {id}: {err:#}");
        }
    }

    /// How long ago the agent's session file last changed.
    /// Each agent's shown status and state token: Herdr's status, holding
    /// a finished turn until seen and a question until answered.
    fn states(&mut self, agents: &[Value]) -> Vec<(String, &'static str)> {
        agents
            .iter()
            .map(|agent| {
                let status = agent["agent_status"].as_str().unwrap_or("unknown");
                let focused = agent["focused"].as_bool().unwrap_or(false);
                let pane = agent["pane_id"].as_str().unwrap_or_default();
                let held = self.cache.held(pane).map(String::from);
                let (status, held) = held_status(status, held.as_deref(), focused);
                let status = status.to_string();
                self.cache.set_held(pane, held);
                let token = state_heading_token(&status, self.age_secs(agent));
                (status, token)
            })
            .collect()
    }

    /// Puts each agent's heading in the token for its state (herdr-radar's
    /// colour by state), reusing the heading the last full pass computed.
    fn report_states(&mut self, agents: &[Value], states: &[(String, &'static str)]) {
        for (agent, (status, state)) in agents.iter().zip(states) {
            let (Some(pane), Some(icon)) = (
                agent["pane_id"].as_str(),
                agent_icon(agent["agent"].as_str().unwrap_or_default()),
            ) else {
                continue;
            };
            let heading = self.cache.heading(pane).unwrap_or_else(|| icon.to_string());
            let value = state_heading_value(&heading, status, self.cache.depth(pane));
            let tokens: Vec<(String, Option<String>)> = STATE_HEADING_TOKENS
                .iter()
                .map(|t| (t.to_string(), (t == state).then(|| value.clone())))
                .collect();
            self.report("pane", pane, "state", None, &tokens);
        }
    }

    /// Seconds since the agent's session last had a message.
    fn age_secs(&mut self, agent: &Value) -> Option<u64> {
        let kind = agent["agent"].as_str()?;
        let session = agent["agent_session"]["value"].as_str()?;
        let last = if kind == "claude" {
            let session = resolve_claude_session(&self.home, session);
            claude_session(&self.home, &session, &mut self.cache).last_activity
        } else {
            codex_session(&self.home, session, &mut self.cache).last_activity
        }?;
        Some(self.now.saturating_sub(last))
    }

    /// Where `cwd` sits in the tree, from the cache or one `git` call.
    fn place(&mut self, cwd: &str) -> Place {
        if let Some(place) = self.cache.place(cwd) {
            return place;
        }
        let place = match git_repo(cwd) {
            Some(repo) => repo.place(),
            None => Place {
                project: dir_name(cwd, &self.home),
                checkout: cwd.to_string(),
                label: dir_name(cwd, &self.home),
            },
        };
        self.cache.set_place(cwd, place.clone());
        place
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

/// Puts the Agents list in activity order (`on`), or back in Herdr's own.
/// A failure only costs the order, so it is logged and the run goes on.
fn set_agent_view(on: bool) {
    let source = format!(
        "plugin:{}",
        env::var("HERDR_PLUGIN_ID").unwrap_or_else(|_| "baleen37.herdr-sim".into())
    );
    let (method, params) = if on {
        let sort = [json!({"field": {"token": SORT_RANK_TOKEN}, "order": "asc"})];
        let params = json!({"source": source, "label": "active", "sort": sort});
        ("agent.view.set", params)
    } else {
        ("agent.view.clear", json!({"source": source}))
    };
    if let Err(err) = socket_call(method, params) {
        eprintln!("herdr-sim: {method}: {err:#}");
    }
}

/// One request over Herdr's socket, for the calls its CLI does not wrap. The
/// server answers one newline-terminated JSON line and hangs up.
fn socket_call(method: &str, params: Value) -> Result<()> {
    let path = env::var("HERDR_SOCKET_PATH").context("HERDR_SOCKET_PATH is not set")?;
    let mut stream = UnixStream::connect(&path).with_context(|| format!("connecting to {path}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(4)))?;
    let request = json!({"id": "herdr-sim", "method": method, "params": params});
    writeln!(stream, "{request}")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let reply: Value = serde_json::from_str(&line)?;
    if !reply["error"].is_null() {
        bail!("{}", reply["error"]);
    }
    Ok(())
}

struct Repo {
    top: String,
    project: String,
    /// `None` on a detached HEAD.
    branch: Option<String>,
}

impl Repo {
    /// This checkout in the tree, headed by its branch (its directory name
    /// when detached).
    fn place(&self) -> Place {
        let dir = || {
            Path::new(&self.top)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        };
        Place {
            project: self.project.clone(),
            checkout: self.top.clone(),
            label: self
                .branch
                .clone()
                .or_else(dir)
                .unwrap_or_else(|| self.top.clone()),
        }
    }
}

/// The repository `cwd` is in, in one `git` call; `None` outside a repository.
fn git_repo(cwd: &str) -> Option<Repo> {
    let out = Command::new("git")
        .args(["-C", cwd, "rev-parse", "--show-toplevel"])
        .args([
            "--path-format=absolute",
            "--git-common-dir",
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
    let (top, common, branch) = (lines.next()?, lines.next()?, lines.next()?);
    Some(Repo {
        top: top.to_string(),
        project: project_name(top, common),
        branch: (branch != "HEAD").then(|| branch.to_string()),
    })
}

/// Project name for a directory outside any repository: `~` for home,
/// otherwise its last component.
fn dir_name(cwd: &str, home: &Path) -> String {
    if Path::new(cwd) == home {
        return "~".into();
    }
    Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.to_string())
}

/// The branch's PR and its checks, empty when the branch has no PR; `None`
/// when `gh` fails otherwise.
fn gh_branch_pr(repo: &str, branch: &str) -> Option<BranchPr> {
    let out = gh(
        &["pr", "view", branch, "--json", "state,statusCheckRollup"],
        Some(Path::new(repo)),
    )?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return stderr
            .to_ascii_lowercase()
            .contains("no pull requests found")
            .then(BranchPr::default);
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let state = PrState::parse(v["state"].as_str()?)?;
    // Checks only matter while the PR is open.
    let ci = (state == PrState::Open)
        .then(|| Ci::from_rollup(&v["statusCheckRollup"]))
        .flatten();
    Some(BranchPr {
        state: Some(state),
        ci,
    })
}

/// Uncommitted changes in the checkout at `top` as `$diff`; `None` when clean
/// or when `git` fails.
fn git_diff(top: &str) -> Option<String> {
    let out = run("git", &["-C", top, "diff", "--shortstat", "HEAD"]).ok()?;
    diff_value(&String::from_utf8_lossy(&out))
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
