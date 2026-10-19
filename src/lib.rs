//! Find the pull requests a coding-agent session touched and summarise them as
//! Herdr sidebar tokens.
//!
//! Claude Code records `pr-link` lines in its transcript whenever a session
//! creates, pushes to, or edits a PR. Codex keeps no such record, so its PRs are
//! taken from the output of `gh pr ...` commands in the session rollout.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// Seconds an open PR's state is trusted. Merged and closed PRs never change.
pub const OPEN_TTL_SECS: u64 = 60;

const PR_COMMANDS: [&str; 7] = [
    "create", "edit", "comment", "ready", "close", "merge", "checkout",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

impl PrState {
    pub const ALL: [PrState; 3] = [PrState::Open, PrState::Merged, PrState::Closed];

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "OPEN" => Some(Self::Open),
            "MERGED" => Some(Self::Merged),
            "CLOSED" => Some(Self::Closed),
            _ => None,
        }
    }

    /// Sidebar token name, referenced as `$pr_open` etc. in Herdr's config.
    pub fn token(self) -> &'static str {
        match self {
            Self::Open => "pr_open",
            Self::Merged => "pr_merged",
            Self::Closed => "pr_closed",
        }
    }

    /// Nerd Font Octicons glyph.
    fn icon(self) -> char {
        match self {
            Self::Open => '\u{f407}',
            Self::Merged => '\u{f419}',
            Self::Closed => '\u{f4dc}',
        }
    }
}

/// Combined state of a PR's checks, shown as `$ci` on Space rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ci {
    Pass,
    Fail,
    Pending,
}

/// Sidebar token for the CI state of a Space's branch PR, referenced as `$ci`.
pub const CI_TOKEN: &str = "ci";

impl Ci {
    /// From `gh pr view --json statusCheckRollup`: any failure fails, else
    /// anything unfinished is pending; `None` when there are no checks.
    pub fn from_rollup(rollup: &Value) -> Option<Self> {
        let checks = rollup.as_array().filter(|c| !c.is_empty())?;
        let mut pending = false;
        for check in checks {
            // Check runs report status + conclusion, commit statuses a state.
            let result = check["conclusion"]
                .as_str()
                .filter(|c| !c.is_empty())
                .or_else(|| check["state"].as_str())
                .unwrap_or_default();
            match result {
                "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED"
                | "STARTUP_FAILURE" => return Some(Self::Fail),
                "SUCCESS" | "NEUTRAL" | "SKIPPED" => {}
                _ => pending = true,
            }
        }
        Some(if pending { Self::Pending } else { Self::Pass })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Pending => "pending",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "pass" => Some(Self::Pass),
            "fail" => Some(Self::Fail),
            "pending" => Some(Self::Pending),
            _ => None,
        }
    }

    /// Nerd Font Octicons glyph: check, x, dot-fill.
    pub fn icon(self) -> &'static str {
        match self {
            Self::Pass => "\u{f42e}",
            Self::Fail => "\u{f467}",
            Self::Pending => "\u{f444}",
        }
    }
}

/// Sidebar token with uncommitted changes in a Space's checkout, referenced as
/// `$diff`: `+12 -3`, from `git diff --shortstat HEAD`. Unset when clean.
pub const DIFF_TOKEN: &str = "diff";

/// `+<insertions> -<deletions>` from `git diff --shortstat` output, leaving
/// out a zero side; `None` when nothing changed.
pub fn diff_value(shortstat: &str) -> Option<String> {
    let count = |word: &str| {
        shortstat
            .split(',')
            .find(|part| part.contains(word))
            .and_then(|part| part.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
    };
    match (count("insertion"), count("deletion")) {
        (0, 0) => None,
        (added, 0) => Some(format!("+{added}")),
        (0, removed) => Some(format!("-{removed}")),
        (added, removed) => Some(format!("+{added} -{removed}")),
    }
}

/// Sidebar token holding the agent's logo, referenced as `$agent_icon`.
pub const AGENT_ICON_TOKEN: &str = "agent_icon";

/// Nerd Font Codicons logo for a Herdr agent id: Claude (`cod-claude`) or
/// Codex (`cod-openai`).
pub fn agent_icon(agent: &str) -> Option<&'static str> {
    match agent {
        "claude" => Some("\u{ec82}"),
        "codex" => Some("\u{ec81}"),
        _ => None,
    }
}

/// Token value for `count` PRs in one state: the icon, plus the count when
/// there is more than one. `None` clears the token.
pub fn token_value(state: PrState, count: usize) -> Option<String> {
    match count {
        0 => None,
        1 => Some(state.icon().to_string()),
        n => Some(format!("{}{n}", state.icon())),
    }
}

/// What one run has to look at, from the Herdr event that started it.
#[derive(Debug, PartialEq, Eq)]
pub enum Scope {
    /// Startup and worktree events: every agent pane and workspace.
    All,
    /// A turn ended in one pane: that pane and its workspace.
    Pane { pane: String, workspace: String },
    /// A turn started or a pane got focus: only state colours can have changed.
    Status,
}

/// Reads `HERDR_PLUGIN_EVENT` / `HERDR_PLUGIN_EVENT_JSON`. Anything it does
/// not understand widens to [`Scope::All`] rather than missing an update.
pub fn scope(event: Option<&str>, event_json: Option<&str>) -> Scope {
    // Focus can only release a held tick.
    if event == Some("pane.focused") {
        return Scope::Status;
    }
    if event != Some("pane.agent_status_changed") {
        return Scope::All;
    }
    let Some(v) = event_json.and_then(parse) else {
        return Scope::All;
    };
    // The payload may come bare or wrapped in an envelope.
    let e = if v.get("pane_id").is_some() {
        &v
    } else {
        &v["event"]
    };
    match (
        e["pane_id"].as_str(),
        e["workspace_id"].as_str(),
        e["agent_status"].as_str(),
    ) {
        (_, _, Some("working")) => Scope::Status,
        (Some(pane), Some(workspace), _) => Scope::Pane {
            pane: pane.to_string(),
            workspace: workspace.to_string(),
        },
        _ => Scope::All,
    }
}

/// Sidebar token naming the project a pane or workspace works in, referenced
/// as `$project`.
pub const PROJECT_TOKEN: &str = "project";

/// The project a checkout belongs to, from `git rev-parse --show-toplevel` and
/// an absolute `--git-common-dir`: a linked worktree names its main repository
/// rather than its own directory, and a bare repository drops its `.git` suffix.
pub fn project_name(toplevel: &str, common_dir: &str) -> String {
    let common = Path::new(common_dir);
    let dir = if common.file_name().is_some_and(|n| n == ".git") {
        common.parent()
    } else if common.is_absolute() {
        Some(common)
    } else {
        None
    };
    let name = dir
        .unwrap_or(Path::new(toplevel))
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.strip_suffix(".git").map(String::from).unwrap_or(name)
}

/// Sidebar tokens that draw a project → worktree tree over the Agents list,
/// referenced as `$tree_project` and `$tree_worktree`. Each is set only on the
/// first agent of its group, so it reads as a header above that group.
pub const TREE_PROJECT_TOKEN: &str = "tree_project";
pub const TREE_WORKTREE_TOKEN: &str = "tree_worktree";

/// What `$tree_project` holds above a project's second and later checkouts;
/// style it apart with an `equals` rule.
pub const TREE_SPINE: &str = "│";

/// Where an agent works, for the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub project: String,
    /// The checkout's top-level directory: one per main checkout or worktree.
    pub checkout: String,
    /// What the checkout header shows: its branch, or its directory name on a
    /// detached HEAD.
    pub label: String,
}

/// Sidebar token with a one-line checkout header, referenced as
/// `$tree_header`: `<repo icon> <project>  <branch icon> <branch>`.
pub const TREE_HEADER_TOKEN: &str = "tree_header";

/// Sidebar token with the PR of a checkout's branch, as an icon on the same
/// agent as `$tree_worktree` (Orca puts the PR on the worktree card),
/// referenced as `$tree_pr`.
pub const TREE_PR_TOKEN: &str = "tree_pr";

/// The header text shared by Agent and Space rows: the project, then branch
/// icon and branch when there is one.
pub fn header_value(project: &str, branch: Option<&str>) -> String {
    match branch {
        Some(branch) => format!("{project}  \u{f418} {branch}"),
        None => project.to_string(),
    }
}

/// One header per checkout, on the first agent of each, for a flat two-level
/// list: every header is an entry's first row, so they all line up, and the
/// project is named on worktrees too. Agents without a place get none.
pub fn checkout_headers(places: &[Option<Place>]) -> Vec<Option<String>> {
    let mut seen = std::collections::HashSet::new();
    places
        .iter()
        .map(|place| {
            let place = place.as_ref()?;
            seen.insert(place.checkout.clone())
                .then(|| header_value(&place.project, Some(&place.label)))
        })
        .collect()
}

/// Tree header tokens for agents listed in sidebar order, like Orca's project
/// list: the project name on the first agent of each project, and
/// `<branch icon> <branch>` on the first agent of each checkout (main checkout
/// and worktrees alike). A later checkout of the same project gets a `│` in
/// the project slot: Herdr indents every row but an entry's first, so this
/// keeps all branch headers at one indent and marks them as one project.
/// Agents without a place get neither.
pub fn tree_headers(places: &[Option<Place>]) -> Vec<(Option<String>, Option<String>)> {
    let mut seen_projects = std::collections::HashSet::new();
    let mut seen_worktrees = std::collections::HashSet::new();
    places
        .iter()
        .map(|place| {
            let Some(place) = place else {
                return (None, None);
            };
            let first_of_project = seen_projects.insert(place.project.clone());
            let first_of_checkout = seen_worktrees.insert(place.checkout.clone());
            let project = match (first_of_project, first_of_checkout) {
                (true, _) => Some(place.project.clone()),
                (false, true) => Some(TREE_SPINE.to_string()),
                (false, false) => None,
            };
            let checkout = first_of_checkout.then(|| format!("\u{f418} {}", place.label));
            (project, checkout)
        })
        .collect()
}

/// Sidebar tokens for herdr-radar's one-header-per-group layout, referenced as
/// `$tree_group`, `$tree_group_stale` and `$tree_gap`: the project (and its
/// branch, when it has several checkouts) over its first checkout,
/// `└─ <branch>` over each later one, and a blank row after the project. A
/// header whose sessions have all gone stale moves to `$tree_group_stale`.
pub const TREE_GROUP_TOKEN: &str = "tree_group";
pub const TREE_GROUP_STALE_TOKEN: &str = "tree_group_stale";
pub const TREE_GAP_TOKEN: &str = "tree_gap";

/// Herdr trims leading whitespace off token values, so the indent and the
/// blank gap row both lean on a zero-width space.
const ZWSP: &str = "\u{200b}";

/// The indent `depth` levels deep, two columns a level, as herdr-radar.
pub fn indent(depth: usize) -> String {
    if depth == 0 {
        String::new()
    } else {
        format!("{ZWSP}{}", "  ".repeat(depth))
    }
}

/// What [`group_rows`] gives one agent.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GroupRow {
    /// The group header, if this agent heads a checkout.
    pub group: Option<String>,
    /// Whether every session of that checkout has gone stale.
    pub stale: bool,
    /// The blank row after a project's last agent.
    pub gap: Option<String>,
    /// How far the agent's own row sits in, on top of Herdr's: Herdr indents
    /// an entry's continuation rows itself, so a header's agent starts a level
    /// in and the rest need one of ours; a later checkout's agents go one
    /// level deeper, under its `└─` header.
    pub depth: usize,
}

/// [`GroupRow`]s for agents listed in sidebar order, given whether each one's
/// session is stale. The gap goes on a project's last agent: one above a
/// header would make the header a continuation row, which Herdr indents.
/// Agents without a place get nothing.
pub fn group_rows(places: &[Option<Place>], stale: &[bool]) -> Vec<GroupRow> {
    let mut seen_projects = std::collections::HashSet::new();
    let mut seen_checkouts = std::collections::HashSet::new();
    let mut nested = std::collections::HashSet::new();
    places
        .iter()
        .enumerate()
        .map(|(i, place)| {
            let Some(place) = place else {
                return GroupRow::default();
            };
            let first_of_project = seen_projects.insert(place.project.clone());
            let first_of_checkout = seen_checkouts.insert(place.checkout.clone());
            if !first_of_project && first_of_checkout {
                nested.insert(place.checkout.clone());
            }
            // A project with one checkout is named alone, as in herdr-radar;
            // with more, its first header names the branch too.
            let several = places
                .iter()
                .flatten()
                .any(|other| other.project == place.project && other.checkout != place.checkout);
            let group = match (first_of_project, first_of_checkout) {
                (true, _) if several => Some(header_value(&place.project, Some(&place.label))),
                (true, _) => Some(place.project.clone()),
                (false, true) => Some(format!("{}└─ \u{f418} {}", indent(1), place.label)),
                (false, false) => None,
            };
            let stale = group.is_some()
                && places.iter().zip(stale).all(|(other, &stale)| {
                    other.as_ref().is_none_or(|o| o.checkout != place.checkout) || stale
                });
            let last_of_project = !places[i + 1..]
                .iter()
                .flatten()
                .any(|next| next.project == place.project);
            let depth =
                usize::from(group.is_none()) + usize::from(nested.contains(&place.checkout));
            GroupRow {
                group,
                stale,
                gap: last_of_project.then(|| ZWSP.to_string()),
                depth,
            }
        })
        .collect()
}

/// Sidebar token marking the second and later agents of a split tab, referenced
/// as `$tree_split`: `├─`, and `└─` on the last, so a split screen reads as one
/// unit under its first agent.
pub const TREE_SPLIT_TOKEN: &str = "tree_split";

/// Split marks for agents listed in sidebar order, given each agent's tab.
pub fn split_marks(tabs: &[&str]) -> Vec<Option<&'static str>> {
    (0..tabs.len())
        .map(|i| {
            if i == 0 || tabs[i - 1] != tabs[i] {
                None
            } else if tabs.get(i + 1) == Some(&tabs[i]) {
                Some("├─")
            } else {
                Some("└─")
            }
        })
        .collect()
}

/// Pane token the activity order sorts on, referenced as `$sort_rank`: the
/// agent's position in [`activity_order`], zero-padded since the Agents view
/// sorts tokens as strings. Herdr shows exactly the order computed here, and
/// the token only changes when the order does.
pub const SORT_RANK_TOKEN: &str = "sort_rank";

/// One agent as the activity order sees it.
pub struct Activity {
    pub place: Option<Place>,
    pub tab: String,
    /// Herdr's `state_change_seq`: higher changed state more recently.
    pub seq: u64,
    /// Whether the session has been idle over two hours.
    pub stale: bool,
}

/// Indices of the agents in activity order: projects by their most recent
/// agent, then checkouts (main checkout and worktrees alike) and tabs within
/// them the same way; names break ties, then list order. A tab whose sessions
/// have all gone stale sinks to the bottom of its checkout, as in herdr-radar.
/// Agents without a place come last, in list order.
pub fn activity_order(agents: &[Activity]) -> Vec<usize> {
    fn groups(agent: &Activity) -> Option<[&str; 3]> {
        let place = agent.place.as_ref()?;
        Some([&place.project, &place.checkout, &agent.tab])
    }
    let mut latest: [HashMap<&str, u64>; 3] = Default::default();
    for agent in agents {
        for (level, name) in groups(agent).into_iter().flatten().enumerate() {
            let seq = latest[level].entry(name).or_default();
            *seq = (*seq).max(agent.seq);
        }
    }
    let mut fresh_tabs = std::collections::HashSet::new();
    for agent in agents.iter().filter(|a| !a.stale) {
        fresh_tabs.insert(agent.tab.as_str());
    }
    let key = |level: usize, name| (std::cmp::Reverse(latest[level][name]), name);
    let mut placed: Vec<_> = agents
        .iter()
        .enumerate()
        .filter_map(|(i, agent)| {
            let [project, checkout, tab] = groups(agent)?;
            let sunk = !fresh_tabs.contains(tab);
            Some((i, (key(0, project), key(1, checkout), sunk, key(2, tab))))
        })
        .collect();
    placed.sort_by(|a, b| a.1.cmp(&b.1));
    let mut order: Vec<usize> = placed.into_iter().map(|(i, _)| i).collect();
    order.extend((0..agents.len()).filter(|&i| agents[i].place.is_none()));
    order
}

/// Sidebar token with how long ago an agent's session last changed,
/// referenced as `$age`.
pub const AGE_TOKEN: &str = "age";

/// A short age like Orca's: `now` under 5 minutes, then 10-minute steps,
/// hours, days. Coarse on purpose: the value, and so a `report-metadata` call,
/// changes at most six times an hour.
pub fn age_value(secs: u64) -> String {
    match secs {
        0..300 => "now".into(),
        300..3600 => format!("{}m", (secs / 600).max(1) * 10),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// The files an agent session writes as it works: a Claude Code transcript or
/// Codex rollout.
pub fn session_files(home: &Path, kind: &str, session_id: &str) -> Vec<PathBuf> {
    let name = format!("{session_id}.jsonl");
    if kind == "claude" {
        return subdirs(&home.join(".claude/projects"))
            .into_iter()
            .map(|project| project.join(&name))
            .filter(|path| path.is_file())
            .collect();
    }
    let mut files = Vec::new();
    for year in subdirs(&home.join(".codex/sessions")) {
        for month in subdirs(&year) {
            for day in subdirs(&month) {
                files.extend(files_ending_with(&day, &name));
            }
        }
    }
    files
}

/// The agents herdr-sim knows, as Herdr agent ids.
pub const AGENT_KINDS: [&str; 2] = ["claude", "codex"];

/// Workspace token for how many agents of one kind run there, referenced as
/// `$agent_claude` / `$agent_codex` so each can have its own colour.
pub fn workspace_agent_token(kind: &str) -> String {
    format!("agent_{kind}")
}

/// The kind's logo, plus the count when more than one runs. `None` clears it.
pub fn agent_count_value(kind: &str, count: usize) -> Option<String> {
    let icon = agent_icon(kind)?;
    match count {
        0 => None,
        1 => Some(icon.to_string()),
        n => Some(format!("{icon} {n}")),
    }
}

/// Sidebar token summarising all PRs, referenced as `$pr`.
pub const PR_SUMMARY_TOKEN: &str = "pr";

/// All PRs as one icon, the way Claude Code labels a session: the icon of the
/// state that most needs attention (open, then merged, then closed) and, when
/// there is more than one PR, the total count. `None` clears the token.
pub fn summary_value(counts: &HashMap<PrState, usize>) -> Option<String> {
    let lead = PrState::ALL
        .into_iter()
        .find(|s| counts.get(s).is_some_and(|&n| n > 0))?;
    match counts.values().sum::<usize>() {
        1 => Some(lead.icon().to_string()),
        total => Some(format!("{} {total}", lead.icon())),
    }
}

/// Sidebar token holding an agent session's title, referenced as `$title`.
pub const TITLE_TOKEN: &str = "title";

/// Sidebar token with the branch checked out where an agent pane works,
/// referenced as `$branch` (Herdr's own `branch` token exists on Space rows
/// only). Unset outside a repository or on a detached HEAD.
pub const BRANCH_TOKEN: &str = "branch";

/// Sidebar token with the agent's logo and title in one value, referenced as
/// `$heading`. Herdr puts ` · ` between tokens; one token reads like a line in
/// Claude Code's own session list. The cost is a single colour for both.
pub const HEADING_TOKEN: &str = "heading";

/// The logo followed by the title, or the logo alone before there is a title:
/// the logo already says which agent it is.
pub fn heading_value(icon: &str, title: Option<&str>) -> String {
    match title {
        Some(title) => format!("{icon} {title}"),
        None => icon.to_string(),
    }
}

/// `$heading` again, but in one of these tokens by the agent's state, so the
/// sidebar can colour titles the way herdr-radar does. Exactly one is set.
pub const STATE_HEADING_TOKENS: [&str; 7] = [
    "heading_working",
    "heading_blocked",
    "heading_done",
    "heading_idle_fresh",
    "heading_idle",
    "heading_idle_stale",
    "heading_unknown",
];

/// The state token of a session idle over two hours.
pub const STALE_HEADING_TOKEN: &str = "heading_idle_stale";

/// Which of [`STATE_HEADING_TOKENS`] holds an agent's heading: by Herdr's
/// agent status, and for idle agents by how long ago the session last changed
/// (under 15 minutes fresh, over 2 hours stale, as in herdr-radar).
pub fn state_heading_token(status: &str, idle_secs: Option<u64>) -> &'static str {
    match status {
        "working" => "heading_working",
        "blocked" => "heading_blocked",
        "done" => "heading_done",
        "idle" => match idle_secs {
            Some(s) if s < 15 * 60 => "heading_idle_fresh",
            Some(s) if s > 2 * 3600 => STALE_HEADING_TOKEN,
            _ => "heading_idle",
        },
        _ => "heading_unknown",
    }
}

/// The status a pane shows, and what it keeps holding, given Herdr's status,
/// what it held before, and whether it has focus. As in herdr-radar, a
/// finished turn keeps its tick until the pane is looked at (Herdr turns
/// `done` into `idle` within seconds), and a question keeps its mark until the
/// agent works again.
pub fn held_status<'a>(
    status: &'a str,
    held: Option<&'a str>,
    focused: bool,
) -> (&'a str, Option<&'a str>) {
    match (status, held) {
        ("working", _) => ("working", None),
        ("blocked", _) | ("idle", Some("blocked")) => ("blocked", Some("blocked")),
        ("done" | "idle", _) if focused => ("idle", None),
        ("done", _) | ("idle", Some("done")) => ("done", Some("done")),
        _ => (status, None),
    }
}

/// A state token's value: the indent, then the heading with herdr-radar's
/// mark for an event that needs you between logo and title — `✓` a finished
/// turn not yet seen, `?` a question.
pub fn state_heading_value(heading: &str, status: &str, depth: usize) -> String {
    let mark = match status {
        "done" => "✓",
        "blocked" => "?",
        _ => return format!("{}{heading}", indent(depth)),
    };
    let marked = match heading.split_once(' ') {
        Some((logo, title)) => format!("{logo} {mark} {title}"),
        None => format!("{heading} {mark}"),
    };
    format!("{}{marked}", indent(depth))
}

/// What herdr-sim knows about one agent session.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Session {
    /// PR URLs the session touched, in first-seen order.
    pub prs: Vec<String>,
    /// The session's title, if it has one yet.
    pub title: Option<String>,
    /// When the session last had a message (Unix seconds).
    pub last_activity: Option<u64>,
}

/// One thing found in a session file line.
enum Hit {
    Pr(String),
    /// A message's time (Unix seconds).
    Activity(u64),
    /// `custom`: a title the user gave (`/rename`), which outranks generated ones.
    Title {
        custom: bool,
        text: String,
    },
}

/// The session a Claude Code pane is really showing. Herdr reports the id of
/// the `claude` process in the pane, but that process may have parked its
/// conversation as a background job (`/background`): its
/// `~/.claude/sessions/<pid>.json` then names the job, whose
/// `~/.claude/jobs/<job>/state.json` holds the session the user is in.
pub fn resolve_claude_session(home: &Path, session_id: &str) -> String {
    let parked = entries(&home.join(".claude/sessions")).find_map(|path| {
        if path.extension().is_none_or(|e| e != "json") {
            return None;
        }
        let v = parse(&fs::read_to_string(&path).ok()?)?;
        if v["sessionId"].as_str() != Some(session_id) {
            return None;
        }
        v["parkedJobId"].as_str().map(String::from)
    });
    parked
        .and_then(|job| {
            let state = fs::read_to_string(home.join(".claude/jobs").join(job).join("state.json"));
            parse(&state.ok()?)?["sessionId"].as_str().map(String::from)
        })
        .unwrap_or_else(|| session_id.to_string())
}

/// A Claude Code session: PRs from its `pr-link` records, and its title from
/// the latest `custom-title` record, or else the latest `ai-title` one.
pub fn claude_session(home: &Path, session_id: &str, cache: &mut Cache) -> Session {
    let file = format!("{session_id}.jsonl");
    let mut session = Session::default();
    for project in subdirs(&home.join(".claude/projects")) {
        let found = cache.scan(&project.join(&file), |line| {
            // Claude Code also rewrites old records, so the file's mtime is not
            // when the session last did something; its messages' times are.
            if line.contains(r#""type":"user""#) || line.contains(r#""type":"assistant""#) {
                return message_time(line).map(Hit::Activity).into_iter().collect();
            }
            let (field, custom) = if line.contains(r#""type":"pr-link""#) {
                ("prUrl", None)
            } else if line.contains(r#""type":"custom-title""#) {
                ("customTitle", Some(true))
            } else if line.contains(r#""type":"ai-title""#) {
                ("aiTitle", Some(false))
            } else {
                return Vec::new();
            };
            let Some(text) = parse(line).and_then(|v| v[field].as_str().map(String::from)) else {
                return Vec::new();
            };
            vec![match custom {
                None => Hit::Pr(text),
                Some(custom) => Hit::Title { custom, text },
            }]
        });
        session.prs.extend(found.urls);
        session.title = session.title.or(found.title);
        session.last_activity = session.last_activity.max(found.last);
    }
    session.prs = dedup(session.prs);
    session
}

/// A Codex session: PRs printed by `gh pr ...` commands in its rollout, and
/// its title from the latest `thread_name` in `~/.codex/session_index.jsonl`.
pub fn codex_session(home: &Path, session_id: &str, cache: &mut Cache) -> Session {
    let suffix = format!("{session_id}.jsonl");
    let mut urls = Vec::new();
    let mut last = None;
    for year in subdirs(&home.join(".codex/sessions")) {
        for month in subdirs(&year) {
            for day in subdirs(&month) {
                for rollout in files_ending_with(&day, &suffix) {
                    let found = cache.scan(&rollout, |line| {
                        let mut hits: Vec<Hit> = Vec::new();
                        if line.contains(r#""type":"response_item""#) {
                            hits.extend(message_time(line).map(Hit::Activity));
                        }
                        if !line.contains(r#""CommandExecution""#) || !line.contains("/pull/") {
                            return hits;
                        }
                        let Some(v) = parse(line) else {
                            return Vec::new();
                        };
                        let item = &v["payload"]["item"];
                        if !is_gh_pr_command(&item["command"].to_string()) {
                            return Vec::new();
                        }
                        let output = item["aggregated_output"].as_str().unwrap_or("");
                        hits.extend(pr_urls(output).into_iter().map(Hit::Pr));
                        hits
                    });
                    urls.extend(found.urls);
                    last = last.max(found.last);
                }
            }
        }
    }
    let mut title = None;
    let index = home.join(".codex/session_index.jsonl");
    let id = format!(r#""id":"{session_id}""#);
    for_each_line(&index, |line| {
        if line.contains(&id)
            && let Some(name) =
                parse(line).and_then(|v| v["thread_name"].as_str().map(String::from))
        {
            title = Some(name);
        }
    });
    Session {
        prs: dedup(urls),
        title,
        last_activity: last,
    }
}

/// The `"timestamp"` of a log line as Unix seconds, read without parsing the
/// whole line: `YYYY-MM-DDTHH:MM:SS…Z`.
fn message_time(line: &str) -> Option<u64> {
    let key = r#""timestamp":""#;
    let start = line.find(key)? + key.len();
    parse_utc(line.get(start..start + 19)?)
}

/// `YYYY-MM-DDTHH:MM:SS` in UTC to Unix seconds.
pub fn parse_utc(s: &str) -> Option<u64> {
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    // Days from 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3600 + mm * 60 + ss).ok()
}

fn for_each_line(path: &Path, mut f: impl FnMut(&str)) {
    let Ok(file) = File::open(path) else { return };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        f(&line);
    }
}

fn is_gh_pr_command(command: &str) -> bool {
    let words: Vec<&str> = command
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter(|w| !w.is_empty())
        .collect();
    words
        .windows(3)
        .any(|w| w[0] == "gh" && w[1] == "pr" && PR_COMMANDS.contains(&w[2]))
}

/// `https://github.com/<owner>/<repo>/pull/<n>` URLs in `text`.
fn pr_urls(text: &str) -> Vec<String> {
    const PREFIX: &str = "https://github.com/";
    let mut urls = Vec::new();
    for (start, _) in text.match_indices(PREFIX) {
        let rest = &text[start + PREFIX.len()..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || "/._-".contains(c)))
            .unwrap_or(rest.len());
        let parts: Vec<&str> = rest[..end].split('/').collect();
        if parts.len() >= 4
            && parts[2] == "pull"
            && parts[3].bytes().all(|b| b.is_ascii_digit())
            && !parts[3].is_empty()
        {
            urls.push(format!(
                "{PREFIX}{}/{}/pull/{}",
                parts[0], parts[1], parts[3]
            ));
        }
    }
    urls
}

/// What one run learned, persisted for the next: PR states keyed by URL, the
/// PR state of each checked-out branch (`None`: the branch has no PR), and how
/// far each session file has been read with the URLs found so far.
#[derive(Default)]
pub struct Cache {
    prs: HashMap<String, (PrState, u64)>,
    branches: HashMap<String, (BranchPr, u64)>,
    files: HashMap<String, Scanned>,
    reported: HashMap<String, String>,
    /// Where each directory sits in the tree; a checkout's project and
    /// worktree do not change, so this saves a `git` call per agent per run.
    places: HashMap<String, Place>,
    /// Each pane's latest `$heading`, so a turn start can recolour it without
    /// rereading the session.
    headings: HashMap<String, String>,
    /// The `done` or `blocked` each pane holds past Herdr's own status.
    held: HashMap<String, String>,
    /// How far in each pane's heading sits, from the last tree pass.
    depths: HashMap<String, usize>,
}

/// The PR of a checked-out branch (`state` is `None` when there is none) and
/// its checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BranchPr {
    pub state: Option<PrState>,
    pub ci: Option<Ci>,
}

#[derive(Default)]
struct Scanned {
    offset: u64,
    urls: Vec<String>,
    /// Latest title so far, and whether the user set it.
    title: Option<(bool, String)>,
    /// Latest message time so far.
    last: Option<u64>,
}

/// What [`Cache::scan`] has found in a file so far.
struct Found {
    urls: Vec<String>,
    title: Option<String>,
    last: Option<u64>,
}

impl Cache {
    pub fn load(path: &Path) -> Self {
        let Some(v) = fs::read_to_string(path).ok().and_then(|s| parse(&s)) else {
            return Self::default();
        };
        let prs = object(&v["prs"])
            .filter_map(|(url, v)| {
                let state = PrState::parse(v["state"].as_str()?)?;
                Some((url.clone(), (state, v["at"].as_u64()?)))
            })
            .collect();
        let branches = object(&v["branches"])
            .filter_map(|(key, v)| {
                let pr = BranchPr {
                    state: v["state"].as_str().and_then(PrState::parse),
                    ci: v["ci"].as_str().and_then(Ci::parse),
                };
                Some((key.clone(), (pr, v["at"].as_u64()?)))
            })
            .collect();
        let files = object(&v["files"])
            .filter_map(|(path, v)| {
                let urls = v["urls"].as_array()?.iter().filter_map(|u| u.as_str());
                let title = v["title"]
                    .as_str()
                    .map(|t| (v["custom"].as_bool().unwrap_or(false), t.to_string()));
                // Entries from before message times were tracked lack "last";
                // dropping them rereads those files once from the start.
                v.get("last")?;
                let scanned = Scanned {
                    offset: v["offset"].as_u64()?,
                    urls: urls.map(String::from).collect(),
                    title,
                    last: v["last"].as_u64(),
                };
                Some((path.clone(), scanned))
            })
            .collect();
        let reported = object(&v["reported"])
            .filter_map(|(key, v)| Some((key.clone(), v.as_str()?.to_string())))
            .collect();
        let places = object(&v["places"])
            .filter_map(|(cwd, v)| {
                let place = Place {
                    project: v["project"].as_str()?.to_string(),
                    checkout: v["checkout"].as_str()?.to_string(),
                    label: v["label"].as_str()?.to_string(),
                };
                Some((cwd.clone(), place))
            })
            .collect();
        let headings = object(&v["headings"])
            .filter_map(|(pane, v)| Some((pane.clone(), v.as_str()?.to_string())))
            .collect();
        let held = object(&v["held"])
            .filter_map(|(pane, v)| Some((pane.clone(), v.as_str()?.to_string())))
            .collect();
        let depths = object(&v["depths"])
            .filter_map(|(pane, v)| Some((pane.clone(), usize::try_from(v.as_u64()?).ok()?)))
            .collect();
        Self {
            prs,
            branches,
            files,
            reported,
            places,
            headings,
            held,
            depths,
        }
    }

    /// Writes through a temporary file so a concurrent run never reads half of it.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let prs: serde_json::Map<String, Value> = self
            .prs
            .iter()
            .map(|(url, (state, at))| (url.clone(), json!({ "state": state.name(), "at": at })))
            .collect();
        let branches: serde_json::Map<String, Value> = self
            .branches
            .iter()
            .map(|(key, (pr, at))| {
                let entry = json!({
                    "state": pr.state.map(PrState::name),
                    "ci": pr.ci.map(Ci::name),
                    "at": at,
                });
                (key.clone(), entry)
            })
            .collect();
        let files: serde_json::Map<String, Value> = self
            .files
            .iter()
            .map(|(path, s)| {
                let (custom, title) = s.title.clone().unzip();
                let entry =
                    json!({ "offset": s.offset, "urls": s.urls, "title": title, "custom": custom, "last": s.last });
                (path.clone(), entry)
            })
            .collect();
        let places: serde_json::Map<String, Value> = self
            .places
            .iter()
            .map(|(cwd, p)| {
                (
                    cwd.clone(),
                    json!({ "project": p.project, "checkout": p.checkout, "label": p.label }),
                )
            })
            .collect();
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        fs::write(
            &tmp,
            json!({ "prs": prs, "branches": branches, "files": files, "reported": self.reported, "places": places, "headings": self.headings, "held": self.held, "depths": self.depths })
                .to_string(),
        )?;
        fs::rename(&tmp, path)
    }

    /// The PR's state, calling `fetch` only when the cached one may be stale.
    /// A failed fetch keeps the last known state.
    pub fn state(
        &mut self,
        url: &str,
        now: u64,
        fetch: impl FnOnce(&str) -> Option<PrState>,
    ) -> Option<PrState> {
        let cached = self.prs.get(url).copied();
        if let Some((state, at)) = cached
            && (state != PrState::Open || now.saturating_sub(at) < OPEN_TTL_SECS)
        {
            return Some(state);
        }
        match fetch(url) {
            Some(state) => {
                self.prs.insert(url.to_string(), (state, now));
                Some(state)
            }
            None => cached.map(|(state, _)| state),
        }
    }

    /// The PR for a checked-out branch and its checks, keyed by `key` (repo +
    /// branch). Like [`Cache::state`], but a branch may have no PR yet, which
    /// is re-checked as often as an open one. `fetch` returns `None` on failure.
    pub fn branch_state(
        &mut self,
        key: &str,
        now: u64,
        fetch: impl FnOnce() -> Option<BranchPr>,
    ) -> BranchPr {
        let cached = self.branches.get(key).copied();
        if let Some((pr, at)) = cached
            && (matches!(pr.state, Some(PrState::Merged | PrState::Closed))
                || now.saturating_sub(at) < OPEN_TTL_SECS)
        {
            return pr;
        }
        match fetch() {
            Some(pr) => {
                self.branches.insert(key.to_string(), (pr, now));
                pr
            }
            None => cached.map(|(pr, _)| pr).unwrap_or_default(),
        }
    }

    /// The pane's latest heading, if one was computed.
    pub fn heading(&self, pane: &str) -> Option<String> {
        self.headings.get(pane).cloned()
    }

    pub fn depth(&self, pane: &str) -> usize {
        self.depths.get(pane).copied().unwrap_or(0)
    }

    pub fn set_depth(&mut self, pane: &str, depth: usize) {
        self.depths.insert(pane.to_string(), depth);
    }

    pub fn held(&self, pane: &str) -> Option<&str> {
        self.held.get(pane).map(String::as_str)
    }

    pub fn set_held(&mut self, pane: &str, held: Option<&str>) {
        match held {
            Some(held) => self.held.insert(pane.to_string(), held.to_string()),
            None => self.held.remove(pane),
        };
    }

    pub fn set_heading(&mut self, pane: &str, heading: String) {
        self.headings.insert(pane.to_string(), heading);
    }

    /// The cached tree place of `cwd`, if any.
    pub fn place(&self, cwd: &str) -> Option<Place> {
        self.places.get(cwd).cloned()
    }

    pub fn set_place(&mut self, cwd: &str, place: Place) {
        self.places.insert(cwd.to_string(), place);
    }

    /// Whether `value` differs from what was last reported for `key`; records
    /// it if so. Lets a run skip `report-metadata` calls that change nothing.
    pub fn changed(&mut self, key: &str, value: &str) -> bool {
        if self.reported.get(key).is_some_and(|v| v == value) {
            return false;
        }
        self.reported.insert(key.to_string(), value.to_string());
        true
    }

    /// Drops the record of `key`'s last report, e.g. after the report failed.
    pub fn forget_reported(&mut self, key: &str) {
        self.reported.remove(key);
    }

    /// Drops every report record. A restarted Herdr server has lost the
    /// metadata, so startup must send everything again.
    pub fn forget_all_reported(&mut self) {
        self.reported.clear();
    }

    /// What `extract` finds in `path`'s lines, reading only complete lines
    /// appended since the previous scan. Session files are append-only; one
    /// that shrank was rewritten and is read again from the start.
    fn scan(&mut self, path: &Path, mut extract: impl FnMut(&str) -> Vec<Hit>) -> Found {
        let Ok(file) = File::open(path) else {
            return Found {
                urls: Vec::new(),
                title: None,
                last: None,
            };
        };
        let key = path.to_string_lossy().into_owned();
        let scanned = self.files.entry(key).or_default();
        if file.metadata().map_or(0, |m| m.len()) < scanned.offset {
            *scanned = Scanned::default();
        }
        let mut reader = BufReader::new(file);
        if reader.seek(SeekFrom::Start(scanned.offset)).is_ok() {
            let mut line = Vec::new();
            // A line without its newline is still being written; take it next time.
            while matches!(reader.read_until(b'\n', &mut line), Ok(n) if n > 0)
                && line.ends_with(b"\n")
            {
                scanned.offset += line.len() as u64;
                for hit in extract(&String::from_utf8_lossy(&line)) {
                    match hit {
                        Hit::Pr(url) => scanned.urls.push(url),
                        Hit::Activity(t) => scanned.last = scanned.last.max(Some(t)),
                        // A generated title never replaces one the user chose.
                        Hit::Title { custom, text } => {
                            if custom || !scanned.title.as_ref().is_some_and(|(c, _)| *c) {
                                scanned.title = Some((custom, text));
                            }
                        }
                    }
                }
                line.clear();
            }
        }
        Found {
            urls: scanned.urls.clone(),
            title: scanned.title.as_ref().map(|(_, t)| t.clone()),
            last: scanned.last,
        }
    }
}

impl PrState {
    fn name(self) -> &'static str {
        match self {
            Self::Open => "OPEN",
            Self::Merged => "MERGED",
            Self::Closed => "CLOSED",
        }
    }
}

fn object(v: &Value) -> impl Iterator<Item = (&String, &Value)> {
    v.as_object().into_iter().flatten()
}

fn parse(s: &str) -> Option<Value> {
    serde_json::from_str(s).ok()
}

fn dedup(urls: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    urls.into_iter()
        .filter(|u| seen.insert(u.clone()))
        .collect()
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    entries(dir).filter(|p| p.is_dir()).collect()
}

fn files_ending_with(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    entries(dir)
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(suffix))
        })
        .collect()
}

fn entries(dir: &Path) -> impl Iterator<Item = PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    /// JSONL as agents write it: every line, including the last, ends in `\n`.
    fn write(path: &Path, lines: &[Value]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: String = lines.iter().map(|v| format!("{v}\n")).collect();
        fs::write(path, body).unwrap();
    }

    fn link(n: u32) -> Value {
        json!({ "type": "pr-link", "prUrl": format!("https://github.com/o/r/pull/{n}") })
    }

    fn codex_command(command: &str, output: &str) -> Value {
        json!({ "type": "event_msg", "payload": { "type": "item_completed", "item": {
            "type": "CommandExecution",
            "command": ["/bin/zsh", "-lc", command],
            "aggregated_output": output,
        }}})
    }

    #[test]
    fn claude_reads_pr_links_across_repos_once_each() {
        let home = TempDir::new().unwrap();
        let link = |url: &str| json!({ "type": "pr-link", "sessionId": "s1", "prUrl": url });
        write(
            &home.path().join(".claude/projects/-dev-search/s1.jsonl"),
            &[
                link("https://github.com/o/search/pull/1"),
                json!({ "type": "user", "message": "see https://github.com/o/other/pull/9" }),
                link("https://github.com/o/batch/pull/2"),
                link("https://github.com/o/search/pull/1"),
            ],
        );
        let mut cache = Cache::default();
        assert_eq!(
            claude_session(home.path(), "s1", &mut cache).prs,
            [
                "https://github.com/o/search/pull/1",
                "https://github.com/o/batch/pull/2"
            ]
        );
        assert!(
            claude_session(home.path(), "other", &mut cache)
                .prs
                .is_empty()
        );
    }

    #[test]
    fn resolve_follows_a_parked_pane_to_its_background_job() {
        let home = TempDir::new().unwrap();
        let sessions = home.path().join(".claude/sessions");
        fs::create_dir_all(&sessions).unwrap();
        let pane = json!({ "pid": 1, "sessionId": "pane", "parkedJobId": "job1" });
        fs::write(sessions.join("1.json"), pane.to_string()).unwrap();
        fs::write(
            sessions.join("2.json"),
            json!({ "sessionId": "plain" }).to_string(),
        )
        .unwrap();
        let job = home.path().join(".claude/jobs/job1");
        fs::create_dir_all(&job).unwrap();
        fs::write(
            job.join("state.json"),
            json!({ "sessionId": "forked" }).to_string(),
        )
        .unwrap();

        assert_eq!(resolve_claude_session(home.path(), "pane"), "forked");
        assert_eq!(resolve_claude_session(home.path(), "plain"), "plain");
        assert_eq!(resolve_claude_session(home.path(), "unknown"), "unknown");
    }

    #[test]
    fn claude_title_prefers_the_users_rename_over_generated_ones() {
        let home = TempDir::new().unwrap();
        let path = home.path().join(".claude/projects/p/s1.jsonl");
        let ai = |t: &str| json!({ "type": "ai-title", "aiTitle": t });
        write(&path, &[ai("first guess"), ai("better guess")]);
        let mut cache = Cache::default();
        let title = |cache: &mut Cache| claude_session(home.path(), "s1", cache).title;
        assert_eq!(title(&mut cache).as_deref(), Some("better guess"));

        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(
            f,
            "{}",
            json!({ "type": "custom-title", "customTitle": "mine" })
        )
        .unwrap();
        writeln!(f, "{}", ai("later guess")).unwrap();
        assert_eq!(title(&mut cache).as_deref(), Some("mine"));
    }

    #[test]
    fn codex_title_is_the_latest_thread_name_for_the_session() {
        let home = TempDir::new().unwrap();
        let entry = |id: &str, name: &str| json!({ "id": id, "thread_name": name });
        write(
            &home.path().join(".codex/session_index.jsonl"),
            &[
                entry("c1", "old"),
                entry("c2", "other"),
                entry("c1", "renamed"),
            ],
        );
        let session = codex_session(home.path(), "c1", &mut Cache::default());
        assert_eq!(session.title.as_deref(), Some("renamed"));
        assert_eq!(
            codex_session(home.path(), "c9", &mut Cache::default()).title,
            None
        );
    }

    #[test]
    fn scan_reads_only_complete_appended_lines() {
        let home = TempDir::new().unwrap();
        let path = home.path().join(".claude/projects/p/s1.jsonl");
        write(&path, &[link(1)]);
        let mut cache = Cache::default();
        assert_eq!(claude_session(home.path(), "s1", &mut cache).prs.len(), 1);

        // A line still being written (no newline yet) is left for the next run.
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(f, "{}", link(2)).unwrap();
        assert_eq!(claude_session(home.path(), "s1", &mut cache).prs.len(), 1);
        writeln!(f).unwrap();
        assert_eq!(claude_session(home.path(), "s1", &mut cache).prs.len(), 2);

        // The offset survives a save/load, so already-read lines are not read
        // again: editing the first line in place (same length) changes nothing.
        let saved = home.path().join("cache.json");
        cache.save(&saved).unwrap();
        let text = fs::read_to_string(&path)
            .unwrap()
            .replacen("pull/1", "pull/9", 1);
        fs::write(&path, text).unwrap();
        let urls = claude_session(home.path(), "s1", &mut Cache::load(&saved)).prs;
        assert_eq!(
            urls,
            [
                "https://github.com/o/r/pull/1",
                "https://github.com/o/r/pull/2"
            ]
        );
    }

    #[test]
    fn failed_fetch_keeps_last_known_state() {
        let mut cache = Cache::default();
        let url = "https://github.com/o/r/pull/1";
        cache.state(url, 0, |_| Some(PrState::Open));
        assert_eq!(cache.state(url, 1_000, |_| None), Some(PrState::Open));
        assert_eq!(Cache::default().state(url, 0, |_| None), None);
    }

    #[test]
    fn codex_takes_urls_only_from_gh_pr_commands() {
        let home = TempDir::new().unwrap();
        write(
            &home
                .path()
                .join(".codex/sessions/2026/10/17/rollout-2026-10-17T17-29-36-c1.jsonl"),
            &[
                codex_command(
                    "gh pr create --title x",
                    "https://github.com/o/infra/pull/750\n",
                ),
                codex_command("cat notes.md", "old: https://github.com/o/infra/pull/1"),
                codex_command(
                    "gh pr view 2 --json url",
                    "https://github.com/o/infra/pull/2",
                ),
                codex_command(
                    "git push && gh pr comment 751 -b ok",
                    "https://github.com/o/infra/pull/751#c",
                ),
            ],
        );
        assert_eq!(
            codex_session(home.path(), "c1", &mut Cache::default()).prs,
            [
                "https://github.com/o/infra/pull/750",
                "https://github.com/o/infra/pull/751"
            ]
        );
    }

    /// herdr/install.sh downloads the release tagged with the manifest's
    /// version, which dist cuts from Cargo.toml's.
    #[test]
    fn plugin_manifest_version_matches_crate() {
        let manifest = include_str!("../herdr-plugin.toml");
        let line = format!("version = \"{}\"", env!("CARGO_PKG_VERSION"));
        assert!(
            manifest.lines().any(|l| l == line),
            "herdr-plugin.toml must say {line}"
        );
    }

    #[test]
    fn scope_narrows_only_for_understood_turn_events() {
        let ev = Some("pane.agent_status_changed");
        let idle = r#"{"type":"pane_agent_status_changed","pane_id":"w:p1","workspace_id":"w","agent_status":"idle"}"#;
        let pane = Scope::Pane {
            pane: "w:p1".into(),
            workspace: "w".into(),
        };
        assert_eq!(scope(Some("pane.focused"), None), Scope::Status);
        assert_eq!(scope(ev, Some(idle)), pane);
        let wrapped = format!(r#"{{"event":{idle}}}"#);
        assert_eq!(scope(ev, Some(&wrapped)), pane);
        let working = idle.replace("idle", "working");
        assert_eq!(scope(ev, Some(&working)), Scope::Status);
        assert_eq!(scope(ev, Some("not json")), Scope::All);
        assert_eq!(scope(ev, None), Scope::All);
        assert_eq!(scope(Some("startup"), None), Scope::All);
        assert_eq!(scope(Some("worktree.created"), Some(idle)), Scope::All);
    }

    #[test]
    fn changed_skips_repeats_until_forgotten() {
        let mut cache = Cache::default();
        assert!(cache.changed("pane:a", "x"));
        assert!(!cache.changed("pane:a", "x"));
        assert!(cache.changed("pane:a", "y"));
        cache.forget_all_reported();
        assert!(cache.changed("pane:a", "y"));
    }

    #[test]
    fn summary_leads_with_the_state_needing_attention() {
        let counts = |pairs: &[(PrState, usize)]| pairs.iter().copied().collect();
        assert_eq!(summary_value(&counts(&[])), None);
        assert_eq!(
            summary_value(&counts(&[(PrState::Merged, 1)])).unwrap(),
            "\u{f419}"
        );
        let mixed = counts(&[
            (PrState::Merged, 2),
            (PrState::Open, 1),
            (PrState::Closed, 0),
        ]);
        assert_eq!(summary_value(&mixed).unwrap(), "\u{f407} 3");
        let done = counts(&[(PrState::Closed, 1), (PrState::Merged, 1)]);
        assert_eq!(summary_value(&done).unwrap(), "\u{f419} 2");
    }

    #[test]
    fn project_name_follows_worktrees_to_their_repository() {
        assert_eq!(project_name("/dev/search", "/dev/search/.git"), "search");
        let worktree = "/dev/search/.worktrees/search-14527-status";
        assert_eq!(project_name(worktree, "/dev/search/.git"), "search");
        assert_eq!(project_name("/srv/infra", "/srv/repos/infra.git"), "infra");
    }

    #[test]
    fn heading_is_the_logo_then_the_title() {
        assert_eq!(
            heading_value("\u{ec82}", Some("Fix login")),
            "\u{ec82} Fix login"
        );
        assert_eq!(heading_value("\u{ec81}", None), "\u{ec81}");
    }

    #[test]
    fn tree_headers_mark_the_first_agent_of_each_group() {
        let at = |project: &str, checkout: &str, label: &str| {
            Some(Place {
                project: project.into(),
                checkout: checkout.into(),
                label: label.into(),
            })
        };
        let headers = tree_headers(&[
            at("search", "/s", "main"),
            at("search", "/s", "main"),
            at("search", "/s/wt", "fix-x"),
            None,
            at("dotfiles", "/d", "main"),
        ]);
        let some = |s: &str| Some(s.to_string());
        assert_eq!(
            headers,
            [
                (some("search"), some("\u{f418} main")),
                (None, None),
                (some("│"), some("\u{f418} fix-x")),
                (None, None),
                (some("dotfiles"), some("\u{f418} main")),
            ]
        );
    }

    #[test]
    fn state_heading_value_marks_events_after_the_logo() {
        assert_eq!(state_heading_value("L Fix", "done", 0), "L ✓ Fix");
        assert_eq!(state_heading_value("L", "blocked", 0), "L ?");
        assert_eq!(state_heading_value("L Fix", "idle", 2), "\u{200b}    L Fix");
    }

    #[test]
    fn held_status_keeps_done_until_seen_and_blocked_until_working() {
        assert_eq!(held_status("done", None, false), ("done", Some("done")));
        assert_eq!(
            held_status("idle", Some("done"), false),
            ("done", Some("done"))
        );
        assert_eq!(held_status("idle", Some("done"), true), ("idle", None));
        assert_eq!(held_status("done", None, true), ("idle", None));
        assert_eq!(
            held_status("idle", Some("blocked"), true),
            ("blocked", Some("blocked"))
        );
        assert_eq!(
            held_status("working", Some("blocked"), false),
            ("working", None)
        );
        assert_eq!(held_status("idle", None, false), ("idle", None));
        assert_eq!(
            held_status("unknown", Some("done"), false),
            ("unknown", None)
        );
    }

    #[test]
    fn group_rows_head_each_checkout_indent_members_and_fade_stale_groups() {
        let at = |project: &str, checkout: &str, label: &str| {
            Some(Place {
                project: project.into(),
                checkout: checkout.into(),
                label: label.into(),
            })
        };
        let rows = group_rows(
            &[
                at("search", "/s", "main"),
                at("search", "/s", "main"),
                at("search", "/s/wt", "fix-x"),
                at("search", "/s/wt", "fix-x"),
                None,
                at("dotfiles", "/d", "main"),
            ],
            &[false, true, true, true, false, true],
        );
        let row = |group: Option<&str>, stale: bool, gap: bool, depth: usize| GroupRow {
            group: group.map(String::from),
            stale,
            gap: gap.then(|| "\u{200b}".to_string()),
            depth,
        };
        assert_eq!(
            rows,
            [
                row(Some("search  \u{f418} main"), false, false, 0),
                row(None, false, false, 1),
                row(Some("\u{200b}  └─ \u{f418} fix-x"), true, false, 1),
                row(None, false, true, 2),
                GroupRow::default(),
                row(Some("dotfiles"), true, true, 0),
            ]
        );
    }

    #[test]
    fn split_marks_branch_off_the_first_agent_of_a_tab() {
        assert_eq!(
            split_marks(&["t1", "t2", "t2", "t2", "t3", "t3"]),
            [None, None, Some("├─"), Some("└─"), None, Some("└─")]
        );
    }

    #[test]
    fn checkout_headers_mark_each_checkouts_first_agent() {
        let at = |project: &str, checkout: &str, label: &str| {
            Some(Place {
                project: project.into(),
                checkout: checkout.into(),
                label: label.into(),
            })
        };
        let headers = checkout_headers(&[
            at("search", "/s", "main"),
            at("search", "/s", "main"),
            at("search", "/s/wt", "fix-x"),
            None,
        ]);
        assert_eq!(
            headers,
            [
                Some("search  \u{f418} main".to_string()),
                None,
                Some("search  \u{f418} fix-x".to_string()),
                None,
            ]
        );
    }

    #[test]
    fn activity_order_puts_recent_groups_first() {
        let at = |project: &str, checkout: &str, tab: &str, seq: u64| Activity {
            place: Some(Place {
                project: project.into(),
                checkout: checkout.into(),
                label: "main".into(),
            }),
            tab: tab.into(),
            seq,
            stale: false,
        };
        let agents = [
            at("search", "/s", "t1", 1),
            at("search", "/s/status", "t2", 9),
            at("dotfiles", "/d", "t3", 5),
            Activity {
                place: None,
                tab: "t4".into(),
                seq: 20,
                stale: false,
            },
            at("search", "/s/batch", "t5", 3),
            at("search", "/s/status", "t6", 2),
            at("search", "/s/status", "t2", 0),
            // Two idle projects keep apart despite equal seqs.
            at("infra", "/i", "t7", 0),
            at("api", "/a", "t8", 0),
            at("infra", "/i", "t9", 0),
        ];
        // search (latest 9): status (9: tab t2 then t6), batch (3), the main
        // checkout (1); dotfiles (5); the two idle projects and their tabs by
        // name; last the agent without a place.
        assert_eq!(activity_order(&agents), [1, 6, 5, 4, 0, 2, 8, 7, 9, 3]);
    }

    #[test]
    fn activity_order_sinks_stale_tabs_within_their_checkout() {
        let at = |tab: &str, seq: u64, stale: bool| Activity {
            place: Some(Place {
                project: "search".into(),
                checkout: "/s".into(),
                label: "main".into(),
            }),
            tab: tab.into(),
            seq,
            stale,
        };
        let agents = [at("t1", 9, true), at("t2", 1, false), at("t3", 5, false)];
        assert_eq!(activity_order(&agents), [2, 1, 0]);
    }

    #[test]
    fn state_heading_follows_status_then_idle_age() {
        assert_eq!(state_heading_token("working", None), "heading_working");
        assert_eq!(
            state_heading_token("blocked", Some(9_999)),
            "heading_blocked"
        );
        assert_eq!(state_heading_token("done", None), "heading_done");
        assert_eq!(state_heading_token("idle", Some(60)), "heading_idle_fresh");
        assert_eq!(state_heading_token("idle", Some(3600)), "heading_idle");
        assert_eq!(state_heading_token("idle", None), "heading_idle");
        assert_eq!(
            state_heading_token("idle", Some(3 * 3600)),
            "heading_idle_stale"
        );
        assert_eq!(state_heading_token("unknown", None), "heading_unknown");
        assert!(STATE_HEADING_TOKENS.contains(&state_heading_token("idle", Some(60))));
    }

    #[test]
    fn parse_utc_matches_known_instants() {
        assert_eq!(parse_utc("1970-01-01T00:00:00"), Some(0));
        assert_eq!(parse_utc("2000-03-01T00:00:00"), Some(951_868_800));
        assert_eq!(parse_utc("2026-10-19T05:50:46"), Some(1_792_389_046));
        assert_eq!(parse_utc("not a time at all"), None);
    }

    #[test]
    fn claude_activity_is_the_latest_message_not_the_file_time() {
        let home = TempDir::new().unwrap();
        let msg = |t: &str, ts: &str| json!({ "type": t, "timestamp": ts });
        write(
            &home.path().join(".claude/projects/p/s1.jsonl"),
            &[
                msg("user", "2026-10-19T05:00:00.000Z"),
                msg("assistant", "2026-10-19T05:50:46.173Z"),
                msg("file-history-snapshot", "2026-10-19T09:00:00.000Z"),
            ],
        );
        let session = claude_session(home.path(), "s1", &mut Cache::default());
        assert_eq!(session.last_activity, parse_utc("2026-10-19T05:50:46"));
    }

    #[test]
    fn age_value_is_coarse() {
        assert_eq!(age_value(0), "now");
        assert_eq!(age_value(299), "now");
        assert_eq!(age_value(300), "10m");
        assert_eq!(age_value(1199), "10m");
        assert_eq!(age_value(1200), "20m");
        assert_eq!(age_value(3599), "50m");
        assert_eq!(age_value(3600), "1h");
        assert_eq!(age_value(5 * 3600 + 59), "5h");
        assert_eq!(age_value(2 * 86400), "2d");
    }

    #[test]
    fn agent_count_value_shows_count_only_above_one() {
        assert_eq!(agent_count_value("claude", 0), None);
        assert_eq!(agent_count_value("claude", 1).unwrap(), "\u{ec82}");
        assert_eq!(agent_count_value("codex", 2).unwrap(), "\u{ec81} 2");
        assert_eq!(agent_count_value("pi", 1), None);
    }

    #[test]
    fn token_value_shows_count_only_above_one() {
        assert_eq!(token_value(PrState::Open, 0), None);
        assert_eq!(token_value(PrState::Open, 1).unwrap(), "\u{f407}");
        assert_eq!(token_value(PrState::Merged, 3).unwrap(), "\u{f419}3");
    }

    #[test]
    fn cache_refetches_only_stale_open_prs() {
        let mut cache = Cache::default();
        let url = "https://github.com/o/r/pull/1";
        assert_eq!(
            cache.state(url, 100, |_| Some(PrState::Open)),
            Some(PrState::Open)
        );
        assert_eq!(
            cache.state(url, 159, |_| panic!("fresh entry refetched")),
            Some(PrState::Open)
        );
        assert_eq!(
            cache.state(url, 160, |_| Some(PrState::Merged)),
            Some(PrState::Merged)
        );
        assert_eq!(
            cache.state(url, 99_999, |_| panic!("merged entry refetched")),
            Some(PrState::Merged)
        );
    }

    #[test]
    fn branch_state_rechecks_missing_and_open_prs_only() {
        let pr = |state, ci| BranchPr { state, ci };
        let open = pr(Some(PrState::Open), Some(Ci::Pending));
        let merged = pr(Some(PrState::Merged), Some(Ci::Pass));
        let mut cache = Cache::default();
        assert_eq!(
            cache.branch_state("r#b", 0, || Some(pr(None, None))).state,
            None
        );
        assert_eq!(
            cache.branch_state("r#b", 59, || panic!("fresh entry refetched")),
            pr(None, None)
        );
        assert_eq!(cache.branch_state("r#b", 60, || Some(open)), open);
        assert_eq!(cache.branch_state("r#b", 120, || None), open);
        assert_eq!(cache.branch_state("r#b", 180, || Some(merged)), merged);
        assert_eq!(
            cache.branch_state("r#b", 99_999, || panic!("merged refetched")),
            merged
        );
    }

    #[test]
    fn ci_fails_on_any_failure_then_waits_on_unfinished_checks() {
        let rollup = |items: Value| Ci::from_rollup(&items);
        assert_eq!(rollup(json!([])), None);
        assert_eq!(rollup(Value::Null), None);
        let pass = json!({"status": "COMPLETED", "conclusion": "SUCCESS"});
        let skipped = json!({"status": "COMPLETED", "conclusion": "SKIPPED"});
        let running = json!({"status": "IN_PROGRESS", "conclusion": ""});
        let failed = json!({"status": "COMPLETED", "conclusion": "FAILURE"});
        let status_ok = json!({"state": "SUCCESS"});
        let status_pending = json!({"state": "PENDING"});
        assert_eq!(rollup(json!([pass, skipped, status_ok])), Some(Ci::Pass));
        assert_eq!(rollup(json!([pass, running])), Some(Ci::Pending));
        assert_eq!(rollup(json!([pass, status_pending])), Some(Ci::Pending));
        assert_eq!(rollup(json!([running, failed])), Some(Ci::Fail));
    }

    #[test]
    fn diff_value_reads_git_shortstat() {
        let stat = " 3 files changed, 12 insertions(+), 3 deletions(-)";
        assert_eq!(diff_value(stat).unwrap(), "+12 -3");
        assert_eq!(diff_value(" 1 file changed, 1 insertion(+)").unwrap(), "+1");
        assert_eq!(diff_value(" 1 file changed, 4 deletions(-)").unwrap(), "-4");
        assert_eq!(diff_value(""), None);
    }

    #[test]
    fn cache_round_trips_through_disk() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cache.json");
        let mut cache = Cache::default();
        cache.state("u", 5, |_| Some(PrState::Closed));
        let merged = BranchPr {
            state: Some(PrState::Merged),
            ci: Some(Ci::Fail),
        };
        cache.branch_state("r#b", 5, || Some(merged));
        cache.save(&path).unwrap();
        let mut loaded = Cache::load(&path);
        assert_eq!(
            loaded.branch_state("r#b", 1_000, || panic!("not cached")),
            merged
        );
        assert_eq!(
            loaded.state("u", 1_000, |_| panic!("not cached")),
            Some(PrState::Closed)
        );
    }
}
