//! Herdr plugin entry point: set `$pr` (plus `$pr_open` / `$pr_merged` /
//! `$pr_closed`) on every Claude Code or Codex pane from the PRs its session
//! touched, plus `$agent_icon` with the agent's logo, and on every workspace
//! from the PR of the branch it has checked out.
//!
//! Herdr runs this at server startup, on `pane.agent_status_changed`, and when
//! a worktree workspace is created or opened. A finished turn only revisits
//! its own pane and workspace, a started turn does nothing, and runs never
//! overlap: one that finds another in progress leaves a note for it instead.

mod changes;
mod diff;
mod watch;

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{OnceLock, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use herdr_sim::{
    AGE_TOKEN, AGENT_ICON_TOKEN, AGENT_KINDS, BRANCH_TOKEN, BranchPr, CI_TOKEN, Cache, Ci,
    DIFF_TOKEN, GROUP_PR_TOKEN, HEADING_TOKEN, ICON_TOKENS, PR_SUMMARY_TOKEN, PROJECT_TOKEN, Place,
    PrState, SORT_RANK_TOKEN, SPACE_AGE_TOKEN, SPACE_HEADER_TOKENS, SPACE_PR_TOKEN, SPIN_MS,
    STALE_HEADING_TOKEN, STATE_HEADING_TOKENS, Scope, Session, TITLE_TOKEN, TREE_GAP_TOKEN,
    TREE_GROUP_PARENT_TOKEN, TREE_GROUP_STALE_TOKEN, TREE_GROUP_TOKEN, TREE_HEADER_IDLE_TOKEN,
    TREE_HEADER_TOKEN, TREE_PROJECT_TOKEN, TREE_WORKTREE_TOKEN, ZWSP, age_value, agent_icon,
    agent_logo_value, animation, checkout_headers, claude_session, claude_session_name,
    codex_session, diff_value, header_value, heading_title, heading_value, held_status, icon_token,
    indent,
    layout::{self, Pane, Space},
    project_name, resolve_claude_session, scope, space_header_value, space_lead, space_status,
    space_title_animation, space_title_tokens, state_heading_token, state_heading_value,
    state_label_token, state_label_value, summary_value, token_value, tree_headers,
    workspace_agent_token,
};
use serde_json::{Value, json};

/// Longest a single `gh` call may take before the run gives up on it.
const GH_TIMEOUT: Duration = Duration::from_secs(10);
/// GraphQL points per hour left to the user's own `gh` and agents: below
/// this, the plugin stops asking until GitHub's budget resets.
pub const GH_RESERVE: u64 = 1000;
/// How long to stop after GitHub refuses a call for its rate limit.
const GH_BACKOFF_SECS: u64 = 300;
/// File holding the Unix time `gh` may be called again, shared by every run
/// and the Changes pane.
static GH_PAUSE: OnceLock<PathBuf> = OnceLock::new();

struct Run {
    herdr: String,
    home: PathBuf,
    /// Whether the Agents list is in activity order.
    ordered: bool,
    /// Herdr's `ui.sidebar_width`, to set a header's PR at its right end.
    sidebar_width: Option<usize>,
    now: u64,
    cache: Cache,
}

fn main() -> Result<()> {
    // Every git this plugin runs only reads: never take the index lock an
    // agent's own git may want (git status's --no-optional-locks).
    // SAFETY: set before any thread starts.
    unsafe { env::set_var("GIT_OPTIONAL_LOCKS", "0") };
    let event = env::var("HERDR_PLUGIN_EVENT").ok();
    let mut scope = scope(
        event.as_deref(),
        env::var("HERDR_PLUGIN_EVENT_JSON").ok().as_deref(),
    );
    let plugin_dir = env::var("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| env::temp_dir().join("herdr-sim"));
    // A named session has panes of its own, and pane ids repeat across
    // sessions: keep its cache, locks and order apart. GitHub's budget is
    // one, so the pause stays shared.
    let state_dir = match env::var("HERDR_SESSION") {
        Ok(session) if !session.is_empty() => plugin_dir.join("sessions").join(session),
        _ => plugin_dir.clone(),
    };
    fs::create_dir_all(&state_dir)?;
    let _ = GH_PAUSE.set(plugin_dir.join("gh-pause"));
    // Present while the activity order is on (it is off by default, leaving
    // Herdr's own sort); the toggle action flips it.
    let order_on = state_dir.join("order-on");
    let herdr_bin = env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
    if env::args().any(|a| a == "--changes-pane") {
        let home = PathBuf::from(env::var("HOME").context("HOME is not set")?);
        return changes::run(&herdr_bin, &home, &state_dir);
    }
    if env::args().any(|a| a == "--edit-pane") {
        return diff::edit_pane();
    }
    if env::args().any(|a| a == "--diff-pane") {
        return diff::run(&herdr_bin);
    }
    if env::args().any(|a| a == "--animate") {
        return animate(&state_dir);
    }
    if env::args().any(|a| a == "--toggle-changes") {
        return changes::toggle(&herdr_bin, &state_dir);
    }
    if env::args().any(|a| a == "--toggle-order") {
        // Turning it on needs no call here: every pass below sets the view.
        if order_on.exists() {
            fs::remove_file(&order_on)?;
            set_agent_view(false);
        } else {
            fs::write(&order_on, b"")?;
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
        sidebar_width: sidebar_width(),
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
        run.ordered = order_on.exists();
        // Our own view only: on while the order is on, cleared otherwise
        // (it would outlive the setting until the server restarts).
        set_agent_view(run.ordered);
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
        let workspaces = self.json(&["workspace", "list"]);
        // A turn start or a focus changes only colours; tree order and
        // headers wait for a turn's end, as before.
        if scope == &Scope::Status {
            self.report_states(&agents, &states);
            self.report_space_states(&agents, &states, &workspaces);
            return;
        }

        // herdr-radar's tree: a group per workspace, worktrees under their
        // checkout, ordered by when each session last worked.
        let spaces: Vec<Space> = workspaces["result"]["workspaces"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|w| Space {
                id: w["workspace_id"].as_str().unwrap_or_default().to_string(),
                label: w["label"].as_str().unwrap_or_default().to_string(),
                repo_key: w["worktree"]["repo_key"].as_str().map(String::from),
                linked: w["worktree"]["is_linked_worktree"].as_bool() == Some(true),
                repo_name: w["worktree"]["repo_name"].as_str().map(String::from),
            })
            .collect();
        let panes: Vec<Pane> = agents
            .iter()
            .zip(&states)
            .map(|(agent, (status, state))| Pane {
                workspace: agent["workspace_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                tab: agent["tab_id"].as_str().unwrap_or_default().to_string(),
                last_minute: self
                    .session_of(agent)
                    .and_then(|session| session.last_activity)
                    .map(|at| at / 60),
                stale: *state == STALE_HEADING_TOKEN,
                urgency: layout::urgency(status),
            })
            .collect();
        let (order, rows) = layout::layout(&panes, &spaces, self.ordered);
        let places: Vec<Option<Place>> = order
            .iter()
            .map(|&i| agents[i]["cwd"].as_str().map(|cwd| self.place(cwd)))
            .collect();
        let headers = tree_headers(&places)
            .into_iter()
            .zip(checkout_headers(&places))
            .zip(rows);
        let prs = self.space_prs(&agents);
        for (pos, (((project, worktree), header), row)) in headers.enumerate() {
            let agent = &agents[order[pos]];
            let Some(pane) = agent["pane_id"].as_str() else {
                continue;
            };
            // The indent rides on the session row's first cell, the logo.
            self.cache.set_depth(pane, row.depth);
            // The workspace's PRs at the right end of its header, as on its
            // Space; Herdr draws the header from the second column, and the
            // last stays free for the scrollbar.
            let workspace = agent["workspace_id"].as_str().unwrap_or_default();
            let group_pr = row
                .group
                .as_ref()
                .and_then(|_| prs.get(workspace))
                .and_then(summary_value);
            let group = match (&group_pr, self.sidebar_width) {
                (Some(pr), Some(width)) => row
                    .group
                    .map(|g| pad_before_pr(&g, width.saturating_sub(5), pr)),
                _ => row.group,
            };
            let (group, group_stale) = if row.group_stale {
                (None, group)
            } else {
                (group, None)
            };
            let tokens = [
                (TREE_PROJECT_TOKEN.to_string(), project),
                (TREE_WORKTREE_TOKEN.to_string(), worktree),
                (TREE_HEADER_TOKEN.to_string(), header),
                (TREE_GROUP_PARENT_TOKEN.to_string(), row.group_parent),
                (TREE_GROUP_TOKEN.to_string(), group),
                (TREE_GROUP_STALE_TOKEN.to_string(), group_stale),
                (GROUP_PR_TOKEN.to_string(), group_pr),
                (
                    TREE_GAP_TOKEN.to_string(),
                    row.gap.then(|| ZWSP.to_string()),
                ),
                (AGE_TOKEN.to_string(), self.age_secs(agent).map(age_value)),
                (
                    SORT_RANK_TOKEN.to_string(),
                    self.ordered.then(|| format!("{pos:04}")),
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
                    (
                        workspace_agent_token(kind),
                        agent_logo_value(kind, count > 0),
                    )
                })
                .collect();
            let project = projects.remove(&workspace);
            // Same header as the Agents list, so both panels read alike.
            let header = headers
                .remove(&workspace)
                .or_else(|| project.as_deref().map(|p| header_value(p, None)));
            // A Space without an agent puts its header in the idle token,
            // so the sidebar can dim it.
            let busy = kinds.is_some_and(|k| k.values().any(|&n| n > 0));
            let (header, idle) = if busy { (header, None) } else { (None, header) };
            tokens.push((TREE_HEADER_TOKEN.into(), header));
            tokens.push((TREE_HEADER_IDLE_TOKEN.into(), idle));
            tokens.push((PROJECT_TOKEN.into(), project));
            tokens.push((DIFF_TOKEN.into(), diffs.remove(&workspace)));
            let ci = cis.remove(&workspace).map(|ci| ci.icon().to_string());
            tokens.push((CI_TOKEN.into(), ci));
            let prs = branch_prs.remove(&workspace).unwrap_or_default();
            self.report("workspace", &workspace, "space", Some(&prs), &tokens);
        }
        self.report_space_states(&agents, &states, &workspaces);
    }

    /// Puts each Space's name in the `$header_*` token for the state of its
    /// agents.
    fn report_space_states(
        &mut self,
        agents: &[Value],
        states: &[(String, &'static str)],
        workspaces: &Value,
    ) {
        let prs = self.space_prs(agents);
        for space in workspaces["result"]["workspaces"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let Some(id) = space["workspace_id"].as_str() else {
                continue;
            };
            // The workspace's own name, as herdr-radar: a Space is picked by
            // what runs in it, not by its branch.
            let Some(header) = space["label"].as_str() else {
                continue;
            };
            let status = space_status(
                agents
                    .iter()
                    .zip(states)
                    .filter(|(a, _)| a["workspace_id"] == id)
                    .map(|(_, (status, _))| status.as_str()),
            );
            let token = format!("header_{status}");
            // The PRs its sessions touched, at the right end of the name's row.
            let pr = prs.get(id).and_then(summary_value);
            let mut value = space_header_value(header, status);
            if let (Some(pr), Some(width)) = (&pr, self.sidebar_width) {
                // Herdr draws a Space's name from its fourth column, a
                // worktree's under its checkout five further in, and keeps
                // the last two columns for a collapse arrow and the scrollbar.
                let family = |w: &Value| {
                    w["worktree"]["is_linked_worktree"] == false
                        && !w["worktree"]["repo_key"].is_null()
                        && w["worktree"]["repo_key"] == space["worktree"]["repo_key"]
                };
                let nested = space["worktree"]["is_linked_worktree"] == true
                    && workspaces["result"]["workspaces"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(family);
                let inset = if nested { 13 } else { 8 };
                let room = width.saturating_sub(inset);
                value = pad_before_pr(&value, room, pr);
            }
            let mut tokens: Vec<(String, Option<String>)> = SPACE_HEADER_TOKENS
                .iter()
                .map(|t| (t.to_string(), (*t == token).then(|| value.clone())))
                .collect();
            // Under the name, the session behind its state: its title and age.
            let sessions: Vec<(&Value, &str)> = agents
                .iter()
                .zip(states)
                .filter(|(a, _)| a["workspace_id"] == id)
                .map(|(a, (status, _))| (a, status.as_str()))
                .collect();
            let statuses: Vec<&str> = sessions.iter().map(|(_, s)| *s).collect();
            let lead = space_lead(&statuses).map(|i| sessions[i].0);
            let title = lead
                .and_then(|a| self.cache.heading(a["pane_id"].as_str()?))
                .and_then(|heading| Some(heading.split_once(' ')?.1.to_string()));
            let age = lead.and_then(|a| self.age_secs(a)).map(age_value);
            tokens.extend(space_title_tokens(title.as_deref(), status));
            tokens.push((SPACE_AGE_TOKEN.to_string(), age));
            tokens.push((SPACE_PR_TOKEN.to_string(), pr));
            self.report("workspace", id, "space-state", None, &tokens);
        }
    }

    /// Each workspace's PRs by state: those its sessions touched, as the
    /// Changes panel lists them, each once. Read from the cache only; the
    /// agent pass keeps it fresh.
    fn space_prs(&mut self, agents: &[Value]) -> HashMap<String, HashMap<PrState, usize>> {
        let mut urls: HashMap<String, HashSet<String>> = HashMap::new();
        for agent in agents {
            let (Some(workspace), Some(session)) =
                (agent["workspace_id"].as_str(), self.session_of(agent))
            else {
                continue;
            };
            urls.entry(workspace.to_string())
                .or_default()
                .extend(session.prs);
        }
        urls.into_iter()
            .map(|(workspace, urls)| {
                let mut counts: HashMap<PrState, usize> = HashMap::new();
                for url in urls {
                    if let Some(state) = self.cache.state(&url, self.now, |_| None) {
                        *counts.entry(state).or_default() += 1;
                    }
                }
                (workspace, counts)
            })
            .collect()
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
        if agent["agent_session"]["value"].as_str().is_none() {
            let heading = self
                .cache
                .heading(pane)
                .unwrap_or_else(|| heading_value(icon, None));
            self.cache.set_heading(pane, heading.clone());
            tokens.push((HEADING_TOKEN.to_string(), Some(heading)));
            self.report("pane", pane, "agent", None, &tokens);
            return;
        }
        // The same session lookup as the state pass, name fallback included.
        let Some(session) = self.session_of(agent) else {
            return;
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
            let kind = agent["agent"].as_str().unwrap_or_default();
            let (Some(pane), Some(icon)) = (agent["pane_id"].as_str(), agent_icon(kind)) else {
                continue;
            };
            // Read the title now rather than reuse the last pass's: Claude
            // titles a session during its first turn, before that turn ends.
            let heading = match self.session_of(agent) {
                Some(session) => {
                    let heading = heading_value(icon, session.title.as_deref());
                    self.cache.set_heading(pane, heading.clone());
                    heading
                }
                None => self.cache.heading(pane).unwrap_or_else(|| icon.to_string()),
            };
            let value = state_heading_value(&heading, status, 0);
            let tokens: Vec<(String, Option<String>)> = STATE_HEADING_TOKENS
                .iter()
                .map(|t| (t.to_string(), (t == state).then(|| value.clone())))
                .collect();
            self.report("pane", pane, "state", None, &tokens);
            // The same heading split in two, logo apart, for the radar rows.
            // A report takes at most 16 tokens, so these go in a call of their own.
            let title = heading.strip_prefix(icon).and_then(|t| t.strip_prefix(' '));
            // The indent leads the title: the row needs no logo ahead of it.
            let label = state_label_value(title, status)
                .map(|label| format!("{}{label}", indent(self.cache.depth(pane))));
            let mut radar: Vec<(String, Option<String>)> = STATE_HEADING_TOKENS
                .iter()
                .map(|t| {
                    (
                        state_label_token(t),
                        (t == state).then(|| label.clone()).flatten(),
                    )
                })
                .collect();
            let logo = icon_token(kind, *state == STALE_HEADING_TOKEN);
            let logo_value = icon.to_string();
            radar.extend(
                ICON_TOKENS
                    .iter()
                    .map(|t| (t.to_string(), (*t == logo).then(|| logo_value.clone()))),
            );
            self.report("pane", pane, "radar", None, &radar);
        }
        // The spinner and pulse need a process that outlives this run; it
        // stops itself once nothing works or asks.
        if states
            .iter()
            .any(|(status, _)| status == "working" || status == "blocked")
        {
            spawn_animator();
        }
    }

    /// The agent's session as of now; reading it again only reads what was
    /// appended since.
    fn session_of(&mut self, agent: &Value) -> Option<Session> {
        let kind = agent["agent"].as_str()?;
        let session = agent["agent_session"]["value"].as_str()?;
        Some(if kind == "claude" {
            let resolved = resolve_claude_session(&self.home, session);
            let mut found = claude_session(&self.home, &resolved, &mut self.cache);
            // No title and no question yet: Claude Code's own name for it,
            // rather than a bare logo.
            if found.title.is_none() {
                found.title = claude_session_name(&self.home, session);
            }
            found
        } else {
            codex_session(&self.home, session, &mut self.cache)
        })
    }

    /// Seconds since the agent's session last had a message.
    fn age_secs(&mut self, agent: &Value) -> Option<u64> {
        let last = self.session_of(agent)?.last_activity?;
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
                worktree: false,
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

/// `header` padded out to `room` columns less the PR cell Herdr puts after
/// it (` · ` and `pr`), so that cell lands at the row's right end; cut with
/// an ellipsis when it does not fit. Braille blanks, since Herdr trims
/// spaces off a value's end; a leading zero-width indent is not counted.
fn pad_before_pr(header: &str, room: usize, pr: &str) -> String {
    let width = room.saturating_sub(3 + changes::cols(pr));
    let body = header.trim_start_matches(ZWSP);
    let lead = &header[..header.len() - body.len()];
    let used = changes::cols(body);
    if used > width {
        return format!("{lead}{}", changes::cut(body, width));
    }
    format!("{header}{}", "\u{2800}".repeat(width - used))
}

/// `ui.sidebar_width` from Herdr's config file, if set.
fn sidebar_width() -> Option<usize> {
    let path = env::var("HERDR_CONFIG_PATH")
        .map(PathBuf::from)
        .or_else(|_| {
            env::var("HOME").map(|home| PathBuf::from(home).join(".config/herdr/config.toml"))
        });
    let text = fs::read_to_string(path.ok()?).ok()?;
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "sidebar_width").then(|| value.trim().parse().ok())?
    })
}

/// Starts `--animate` in its own process group, so it outlives this run.
fn spawn_animator() {
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let spawned = Command::new(exe)
        .arg("--animate")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
    if let Err(err) = spawned {
        eprintln!("herdr-sim: --animate: {err}");
    }
}

/// Moves the marks Herdr cannot animate itself, as herdr-radar does: a
/// spinner ahead of each working title and Space header, and a pulsing
/// question mark where an agent asks. One animator at a time; it talks to
/// Herdr's socket rather than spawning the CLI, since it steps every
/// [`SPIN_MS`], and exits once nothing works or asks.
fn animate(state_dir: &Path) -> Result<()> {
    let lock = File::create(state_dir.join("animate-lock"))?;
    if lock.try_lock().is_err() {
        return Ok(());
    }
    // What was last written, so a pulse that holds still costs no call.
    let mut written: HashMap<(String, String), Option<String>> = HashMap::new();
    for frame in 0.. {
        let mut moving = false;
        let agents = socket_request("agent.list", json!({}))?;
        for agent in agents["agents"].as_array().into_iter().flatten() {
            let tokens = &agent["tokens"];
            let Some(pane) = agent["pane_id"].as_str() else {
                continue;
            };
            let title = heading_title(tokens[HEADING_TOKEN].as_str().unwrap_or_default());
            if let Some((key, value)) = animation(tokens, "label", title, frame) {
                moving |= value.is_some();
                if written.insert((pane.into(), key.clone()), value.clone()) != Some(value.clone())
                {
                    report_tokens("pane", pane, json!({ key: value }));
                }
            }
        }
        let spaces = socket_request("workspace.list", json!({}))?;
        for space in spaces["workspaces"].as_array().into_iter().flatten() {
            let tokens = &space["tokens"];
            let (Some(id), Some(header)) =
                (space["workspace_id"].as_str(), space["label"].as_str())
            else {
                continue;
            };
            // A working Space spins on its title's line, as an agent does;
            // its name keeps the still mark. A question pulses on the name.
            let header_moves = tokens["header_working"].is_null();
            // The name as the run padded it, behind the mark it pulses.
            let header = tokens["header_blocked"]
                .as_str()
                .and_then(|v| v.split_once(' '))
                .map_or(header, |(_, name)| name);
            let moves = [
                header_moves
                    .then(|| animation(tokens, "header", header, frame))
                    .flatten(),
                space_title_animation(tokens, frame),
            ];
            for (key, value) in moves.into_iter().flatten() {
                moving |= value.is_some();
                if written.insert((id.into(), key.clone()), value.clone()) != Some(value.clone()) {
                    report_tokens("workspace", id, json!({ key: value }));
                }
            }
        }
        if !moving {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(SPIN_MS));
    }
    Ok(())
}

/// Sets or clears a pane's or workspace's (`kind`) tokens as herdr-sim, over
/// the socket.
fn report_tokens(kind: &str, id: &str, tokens: Value) {
    let params = json!({ format!("{kind}_id"): id, "source": "herdr-sim", "tokens": tokens });
    if let Err(err) = socket_call(&format!("{kind}.report_metadata"), params) {
        eprintln!("herdr-sim: {id}: {err:#}");
    }
}

/// One request over Herdr's socket, for the calls its CLI does not wrap. The
/// server answers one newline-terminated JSON line and hangs up.
fn socket_call(method: &str, params: Value) -> Result<()> {
    socket_request(method, params).map(|_| ())
}

/// One request to the Herdr server over its socket, without spawning the
/// CLI; the reply's `result`.
pub fn socket_request(method: &str, params: Value) -> Result<Value> {
    let path = env::var("HERDR_SOCKET_PATH").context("HERDR_SOCKET_PATH is not set")?;
    let mut stream = UnixStream::connect(&path).with_context(|| format!("connecting to {path}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(4)))?;
    let request = json!({"id": "herdr-sim", "method": method, "params": params});
    writeln!(stream, "{request}")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let mut reply: Value = serde_json::from_str(&line)?;
    if !reply["error"].is_null() {
        bail!("{}", reply["error"]);
    }
    Ok(reply["result"].take())
}

struct Repo {
    top: String,
    project: String,
    /// `None` on a detached HEAD.
    branch: Option<String>,
    /// A linked worktree rather than the main checkout.
    worktree: bool,
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
            worktree: self.worktree,
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
        // The main checkout keeps the repository in its own `.git`.
        worktree: Path::new(common) != Path::new(top).join(".git"),
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
/// the lock. Its output is read while it runs, so a large answer (the Changes
/// pane's) cannot fill the pipe and stall it.
///
/// While GitHub's rate limit is near, it returns `None` without calling, and
/// callers keep what they cached.
fn gh(args: &[&str], cwd: Option<&Path>) -> Option<Output> {
    if gh_paused_until().is_some_and(|t| t > unix_now()) {
        return None;
    }
    let mut cmd = Command::new("gh");
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let child = cmd.spawn().ok()?;
    let pid = child.id().to_string();
    let (done, waited) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(child.wait_with_output());
    });
    let Ok(out) = waited.recv_timeout(GH_TIMEOUT) else {
        let _ = Command::new("kill").arg(&pid).status();
        return None;
    };
    let out = out.ok()?;
    if !out.status.success()
        && String::from_utf8_lossy(&out.stderr)
            .to_ascii_lowercase()
            .contains("rate limit")
    {
        pause_gh(unix_now() + GH_BACKOFF_SECS);
    }
    Some(out)
}

/// Stops every `gh` call until Unix time `until`.
pub fn pause_gh(until: u64) {
    if let Some(path) = GH_PAUSE.get() {
        let _ = fs::write(path, until.to_string());
    }
}

fn gh_paused_until() -> Option<u64> {
    fs::read_to_string(GH_PAUSE.get()?)
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad_before_pr_sets_the_pr_at_the_right_end() {
        // 20 columns: name, blanks, then Herdr's ` · ` and the PR's 3.
        let padded = pad_before_pr("◐ search", 20, "\u{f407} 2");
        assert_eq!(changes::cols(&padded), 14);
        assert!(padded.starts_with("◐ search\u{2800}"));
        // An indent of a zero-width space is not counted.
        let nested = pad_before_pr("\u{200b}  └─ wt", 12, "\u{f407}");
        assert_eq!(nested, "\u{200b}  └─ wt\u{2800}");
        // Too long: cut, so the PR still shows.
        assert_eq!(pad_before_pr("한국어 이름", 10, "\u{f407}"), "한국…");
    }
}
