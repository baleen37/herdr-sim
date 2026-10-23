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

pub mod layout;

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
    let (added, removed) = shortstat_counts(shortstat);
    diff_text(added, removed)
}

/// Insertions and deletions in `git diff --shortstat` output.
pub fn shortstat_counts(shortstat: &str) -> (u64, u64) {
    let count = |word: &str| {
        shortstat
            .split(',')
            .find(|part| part.contains(word))
            .and_then(|part| part.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
    };
    (count("insertion"), count("deletion"))
}

/// `+<added> -<removed>`, leaving out a zero side; `None` for neither.
pub fn diff_text(added: u64, removed: u64) -> Option<String> {
    match (added, removed) {
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
    /// A linked worktree rather than the repository's main checkout.
    pub worktree: bool,
}

/// Sidebar token with a one-line checkout header, referenced as
/// `$tree_header`: `<repo icon> <project>  <branch icon> <branch>`.
pub const TREE_HEADER_TOKEN: &str = "tree_header";
/// `$tree_header` on a Space with no agent in it, so it can be dimmed.
pub const TREE_HEADER_IDLE_TOKEN: &str = "tree_header_idle";

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

/// Pane token the activity order sorts on, referenced as `$sort_rank`: the
/// agent's position in [`layout::layout`], zero-padded since the Agents view
/// sorts tokens as strings. Herdr shows exactly the order computed here, and
/// the token only changes when the order does.
pub const SORT_RANK_TOKEN: &str = "sort_rank";

/// Sidebar tokens for herdr-radar's tree over the Agents list (see [`layout`]):
/// `$tree_group` the workspace header on its first session (`$tree_group_stale`
/// once every session in it is stale), `$tree_group_parent` the repository on
/// a row of its own over a worktree whose checkout has no session, and
/// `$tree_gap` a blank row after the group.
pub const TREE_GROUP_TOKEN: &str = "tree_group";
pub const TREE_GROUP_STALE_TOKEN: &str = "tree_group_stale";
pub const TREE_GROUP_PARENT_TOKEN: &str = "tree_group_parent";
pub const TREE_GAP_TOKEN: &str = "tree_gap";

pub use layout::{ZWSP, indent};

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

/// Workspace token for whether agents of one kind run there, referenced as
/// `$agent_claude` / `$agent_codex` so each can have its own colour.
pub fn workspace_agent_token(kind: &str) -> String {
    format!("agent_{kind}")
}

/// The kind's logo and name when any of it runs, as herdr-radar's Spaces
/// list does. `None` clears it.
pub fn agent_logo_value(kind: &str, running: bool) -> Option<String> {
    let icon = agent_icon(kind)?;
    running.then(|| format!("{icon} {kind}"))
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

/// The state's mark — `◐` working, `✓` a finished turn not yet seen, `?` a
/// question, `○` idle — or none when Herdr cannot tell, rather than one that
/// says idle.
fn state_mark(status: &str) -> Option<&'static str> {
    match status {
        "working" => Some("◐"),
        "done" => Some("✓"),
        "blocked" => Some("?"),
        "idle" => Some("○"),
        _ => None,
    }
}

/// A state token's value: the indent, the state's mark, then the heading.
pub fn state_heading_value(heading: &str, status: &str, depth: usize) -> String {
    // The state leads, as Herdr's own state icon would: the heading then
    // stands alone in its row, so an indent before it opens no gap.
    match state_mark(status) {
        Some(mark) => format!("{}{mark} {heading}", indent(depth)),
        None => format!("{}{heading}", indent(depth)),
    }
}

/// The logo in a token of its own, as in herdr-radar: the logo says whose
/// agent it is in its vendor's colour, and the title beside it, in a
/// `$label_*` token, says what it is doing. Referenced as `$icon_claude` and
/// `$icon_codex`, or `$icon_stale` for a session idle over two hours, so the
/// whole row can dim. The row's tree indent leads `$label_*`, so a row can
/// leave the logo out.
pub const ICON_TOKENS: [&str; 3] = ["icon_claude", "icon_codex", "icon_stale"];

/// Which of [`ICON_TOKENS`] holds an agent's logo.
pub fn icon_token(kind: &str, stale: bool) -> String {
    if stale {
        "icon_stale".into()
    } else {
        format!("icon_{kind}")
    }
}

/// `$heading_*` without the logo, for rows that put it in `$icon_*`:
/// `$label_working` and so on, one per state heading token.
pub fn state_label_token(state: &str) -> String {
    state.replacen("heading", "label", 1)
}

/// A `$label_*` value: the state's mark and the title. `None` when there is
/// neither, which clears the token.
pub fn state_label_value(title: Option<&str>, status: &str) -> Option<String> {
    match (state_mark(status), title) {
        (Some(mark), Some(title)) => Some(format!("{mark} {title}")),
        (mark, title) => mark.or(title).map(String::from),
    }
}

/// A Space's header in a token per the state of the agents in it, so the
/// Spaces list reads like the Agents list: `$header_working` and so on, and
/// `$header_none` for a Space without an agent. Each leads with the state's
/// mark (`◦` for none), so leave Herdr's `state_icon` out of that row.
pub const SPACE_HEADER_TOKENS: [&str; 6] = [
    "header_working",
    "header_blocked",
    "header_done",
    "header_idle",
    "header_unknown",
    "header_none",
];

/// The state a Space shows, from the shown states of its agents: whichever
/// most needs you — a question, then work, a finished turn, idle. `none`
/// without agents.
pub fn space_status<'a>(statuses: impl IntoIterator<Item = &'a str>) -> &'static str {
    const ORDER: [&str; 5] = ["blocked", "working", "done", "idle", "unknown"];
    let statuses: Vec<&str> = statuses.into_iter().collect();
    if statuses.is_empty() {
        return "none";
    }
    ORDER
        .into_iter()
        .find(|s| statuses.contains(s))
        .unwrap_or("unknown")
}

/// Workspace tokens about the session behind a Space's state, under its
/// name: `$space_title` its title, `$space_age` how long ago it last had a
/// message (as `$age`), so the Spaces list's second row reads like an Agents
/// row.
pub const SPACE_TITLE_TOKEN: &str = "space_title";
pub const SPACE_AGE_TOKEN: &str = "space_age";

/// Which of a Space's sessions, given their shown statuses, is behind the
/// state it shows: the first in that state.
pub fn space_lead(statuses: &[&str]) -> Option<usize> {
    let status = space_status(statuses.iter().copied());
    statuses.iter().position(|s| *s == status)
}

/// The PRs a workspace's sessions touched, as one icon (`$pr`'s summary),
/// at the right end of its header: `$space_pr` on the Space,
/// `$group_pr` on the Agents list's header agent.
pub const SPACE_PR_TOKEN: &str = "space_pr";
pub const GROUP_PR_TOKEN: &str = "group_pr";

/// `$space_title` on a working Space instead, behind the spinner (the
/// working mark until the animator turns it), so the loading shows on the
/// session's line as in the Agents list rather than on the Space's name.
pub const SPACE_TITLE_WORKING_TOKEN: &str = "space_title_working";
/// The bare title, which the animator turns its spinner in front of; not
/// meant for display.
pub const SPACE_TITLE_TEXT_TOKEN: &str = "space_title_text";

/// The three title tokens of a Space, given its title and state: the title
/// in `$space_title`, or behind `◐` in `$space_title_working` while it works.
pub fn space_title_tokens(title: Option<&str>, status: &str) -> [(String, Option<String>); 3] {
    let working = status == "working";
    [
        (
            SPACE_TITLE_TOKEN.into(),
            title.filter(|_| !working).map(String::from),
        ),
        (
            SPACE_TITLE_WORKING_TOKEN.into(),
            title.filter(|_| working).map(|t| format!("◐ {t}")),
        ),
        (SPACE_TITLE_TEXT_TOKEN.into(), title.map(String::from)),
    ]
}

/// What the animator writes on step `frame` into a Space's title tokens:
/// the spinner in `$space_title_working`, or a clear when a write raced the
/// end of the work and left it beside `$space_title`. `None` when the Space
/// does not work.
pub fn space_title_animation(tokens: &Value, frame: usize) -> Option<(String, Option<String>)> {
    if tokens[SPACE_TITLE_WORKING_TOKEN].is_null() {
        return None;
    }
    let value = match (
        &tokens[SPACE_TITLE_TOKEN],
        tokens[SPACE_TITLE_TEXT_TOKEN].as_str(),
    ) {
        (Value::Null, Some(title)) => Some(format!("{} {title}", SPINNER[frame % SPINNER.len()])),
        _ => None,
    };
    Some((SPACE_TITLE_WORKING_TOKEN.into(), value))
}

/// A `$header_*` value: the state's mark, then the header.
pub fn space_header_value(header: &str, status: &str) -> String {
    let mark = state_mark(status).unwrap_or("◦");
    format!("{mark} {header}")
}

/// The braille spinner herdr-radar turns ahead of a working title, one step
/// per [`SPIN_MS`].
pub const SPINNER: [char; 8] = ['⣷', '⣯', '⣟', '⡿', '⢿', '⣻', '⣽', '⣾'];
/// One spinner step: faster reads as a blur, slower as a stutter (herdr-radar).
pub const SPIN_MS: u64 = 150;

/// Spinner steps per half beat of the question mark's pulse, about 0.75 s.
pub const PULSE_STEPS: usize = 5;

/// The title in an agent's `$heading` (its logo, then the title).
pub fn heading_title(heading: &str) -> &str {
    heading.split_once(' ').map_or("", |(_, title)| title)
}

/// What the animator writes on step `frame` into one pane's `$label_*` or
/// Space's `$header_*` tokens (`prefix`), given those tokens and the text
/// behind the mark: a spinner step in the working token, as herdr-radar
/// turns one; the question mark pulsing in the blocked token. A write that
/// raced a state change leaves the token beside the new state's, so it is
/// cleared instead. `None` when the state does not move.
pub fn animation(
    tokens: &Value,
    prefix: &str,
    text: &str,
    frame: usize,
) -> Option<(String, Option<String>)> {
    let key = |state: &str| format!("{prefix}_{state}");
    let (state, mark) = if !tokens[key("working")].is_null() {
        ("working", SPINNER[frame % SPINNER.len()])
    } else if !tokens[key("blocked")].is_null() {
        // A braille blank keeps the title in place where `?` was.
        let on = (frame / PULSE_STEPS).is_multiple_of(2);
        ("blocked", if on { '?' } else { '\u{2800}' })
    } else {
        return None;
    };
    let others = [
        "working",
        "blocked",
        "done",
        "idle_fresh",
        "idle",
        "idle_stale",
        "unknown",
        "none",
    ]
    .into_iter()
    .filter(|s| *s != state)
    .any(|s| !tokens[key(s)].is_null());
    // The row's tree indent, which leads the value, stays where it is.
    let current = tokens[key(state)].as_str().unwrap_or_default();
    let lead = &current[..current.len() - current.trim_start_matches([' ', '\u{200b}']).len()];
    let value = match (others, text) {
        (true, _) => None,
        (false, "") => Some(format!("{lead}{mark}")),
        (false, text) => Some(format!("{lead}{mark} {text}")),
    };
    Some((key(state), value))
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
    /// Directories the session worked in, first seen first: where it ran,
    /// edited files, or `cd`'d to.
    pub dirs: Vec<String>,
}

/// One thing found in a session file line.
enum Hit {
    Pr(String),
    /// A message's time (Unix seconds).
    Activity(u64),
    /// Something the user asked, a title for a session Claude has not titled.
    Prompt(String),
    /// A directory the session worked in.
    Dir(String),
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

/// The name Claude Code gave a session in `~/.claude/sessions/<pid>.json`
/// (such as `search-4b`): something to show before it has a title or a
/// question.
pub fn claude_session_name(home: &Path, session_id: &str) -> Option<String> {
    entries(&home.join(".claude/sessions")).find_map(|path| {
        if path.extension().is_none_or(|e| e != "json") {
            return None;
        }
        let v = parse(&fs::read_to_string(&path).ok()?)?;
        if v["sessionId"].as_str() != Some(session_id) {
            return None;
        }
        v["name"]
            .as_str()
            .filter(|n| !n.is_empty())
            .map(String::from)
    })
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
                let mut hits: Vec<Hit> =
                    message_time(line).map(Hit::Activity).into_iter().collect();
                if line.contains(r#""type":"user""#) {
                    hits.extend(user_prompt(line).map(Hit::Prompt));
                }
                if line.contains(r#""tool_use""#) {
                    hits.extend(work_dirs(line, home).into_iter().map(Hit::Dir));
                }
                return hits;
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
        // Until Claude titles the session, its first question stands in.
        session.title = session.title.or(found.title).or(found.prompt);
        session.last_activity = session.last_activity.max(found.last);
        session.dirs.extend(found.dirs);
    }
    session.prs = dedup(session.prs);
    session.dirs = dedup(session.dirs);
    session
}

/// A Codex session: PRs printed by `gh pr ...` commands in its rollout, and
/// its title from the latest `thread_name` in `~/.codex/session_index.jsonl`.
pub fn codex_session(home: &Path, session_id: &str, cache: &mut Cache) -> Session {
    let suffix = format!("{session_id}.jsonl");
    let mut urls = Vec::new();
    let mut last = None;
    let mut dirs = Vec::new();
    for year in subdirs(&home.join(".codex/sessions")) {
        for month in subdirs(&year) {
            for day in subdirs(&month) {
                for rollout in files_ending_with(&day, &suffix) {
                    let found = cache.scan(&rollout, |line| {
                        let mut hits: Vec<Hit> = Vec::new();
                        if line.contains(r#""type":"response_item""#) {
                            hits.extend(message_time(line).map(Hit::Activity));
                        }
                        let command = line.contains(r#""CommandExecution""#);
                        if !command && !line.contains(r#""FileChange""#) {
                            return hits;
                        }
                        let Some(v) = parse(line) else {
                            return Vec::new();
                        };
                        let item = &v["payload"]["item"];
                        // Where commands ran and which files patches touched.
                        if let Some(cwd) = item["cwd"].as_str() {
                            hits.push(Hit::Dir(
                                cwd.strip_prefix("file://").unwrap_or(cwd).to_string(),
                            ));
                        }
                        for file in object(&item["changes"]).map(|(file, _)| file) {
                            hits.extend(
                                Path::new(file)
                                    .parent()
                                    .map(|d| Hit::Dir(d.display().to_string())),
                            );
                        }
                        if !command || !line.contains("/pull/") {
                            return hits;
                        }
                        if !is_gh_pr_command(&item["command"].to_string()) {
                            return Vec::new();
                        }
                        let output = item["aggregated_output"].as_str().unwrap_or("");
                        hits.extend(pr_urls(output).into_iter().map(Hit::Pr));
                        hits
                    });
                    urls.extend(found.urls);
                    last = last.max(found.last);
                    dirs.extend(found.dirs);
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
        dirs: dedup(dirs),
    }
}

/// The text of a user message that is a real question: not a slash command,
/// command output or tool result (those start with `<` or are not plain
/// text). Cut to 60 characters on a char boundary.
fn user_prompt(line: &str) -> Option<String> {
    let v = parse(line)?;
    let content = &v["message"]["content"];
    let text = match content.as_str() {
        Some(text) => text.to_string(),
        None => content
            .as_array()?
            .iter()
            .find(|part| part["type"] == "text")?["text"]
            .as_str()?
            .to_string(),
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() || text.starts_with('<') {
        return None;
    }
    Some(text.chars().take(60).collect())
}

/// Directories an assistant message's tool calls worked in: the session's
/// `cwd`, the folder of each file it edited, and each absolute path a shell
/// command `cd`'d or `git -C`'d to.
fn work_dirs(line: &str, home: &Path) -> Vec<String> {
    let Some(v) = parse(line) else {
        return Vec::new();
    };
    let mut dirs: Vec<String> = v["cwd"].as_str().map(String::from).into_iter().collect();
    let expand = |p: &str| -> Option<String> {
        let p = p.trim_matches(|c| c == '"' || c == '\'' || c == ';' || c == ')');
        let p = match p.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                format!("{}{rest}", home.display())
            }
            _ => p.to_string(),
        };
        p.starts_with('/').then_some(p)
    };
    for call in v["message"]["content"].as_array().into_iter().flatten() {
        if call["type"] != "tool_use" {
            continue;
        }
        let input = &call["input"];
        if let Some(file) = input["file_path"]
            .as_str()
            .or(input["notebook_path"].as_str())
        {
            dirs.extend(Path::new(file).parent().map(|d| d.display().to_string()));
        }
        if let Some(command) = input["command"].as_str() {
            let words: Vec<&str> = command.split_whitespace().collect();
            for pair in words.windows(2) {
                if matches!(pair[0], "cd" | "-C") {
                    dirs.extend(expand(pair[1]));
                }
            }
        }
    }
    dirs
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
    /// The first thing the user asked.
    prompt: Option<String>,
    /// Directories worked in, deduplicated.
    dirs: Vec<String>,
}

/// What [`Cache::scan`] has found in a file so far.
struct Found {
    urls: Vec<String>,
    title: Option<String>,
    last: Option<u64>,
    prompt: Option<String>,
    dirs: Vec<String>,
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
                // Likewise for "prompt", the untitled-session fallback, and
                // "dirs", where the session worked.
                v.get("prompt")?;
                let dirs = v.get("dirs")?.as_array()?.iter();
                let scanned = Scanned {
                    offset: v["offset"].as_u64()?,
                    urls: urls.map(String::from).collect(),
                    title,
                    last: v["last"].as_u64(),
                    prompt: v["prompt"].as_str().map(String::from),
                    dirs: dirs.filter_map(|d| d.as_str().map(String::from)).collect(),
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
                    // Entries from before this field are dropped and looked up again.
                    worktree: v["worktree"].as_bool()?,
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
                    json!({ "offset": s.offset, "urls": s.urls, "title": title, "custom": custom, "last": s.last, "prompt": s.prompt, "dirs": s.dirs });
                (path.clone(), entry)
            })
            .collect();
        let places: serde_json::Map<String, Value> = self
            .places
            .iter()
            .map(|(cwd, p)| {
                (
                    cwd.clone(),
                    json!({ "project": p.project, "checkout": p.checkout, "label": p.label, "worktree": p.worktree }),
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
                prompt: None,
                dirs: Vec::new(),
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
                        Hit::Prompt(p) => {
                            scanned.prompt.get_or_insert(p);
                        }
                        Hit::Dir(d) => {
                            if !scanned.dirs.contains(&d) {
                                scanned.dirs.push(d);
                            }
                        }
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
            prompt: scanned.prompt.clone(),
            dirs: scanned.dirs.clone(),
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
                worktree: checkout.contains("/wt"),
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
        assert_eq!(state_heading_value("L Fix", "done", 0), "✓ L Fix");
        assert_eq!(state_heading_value("L", "blocked", 0), "? L");
        assert_eq!(state_heading_value("L Fix", "idle", 1), "\u{200b}  ○ L Fix");
        assert_eq!(state_heading_value("L Fix", "unknown", 0), "L Fix");
    }

    #[test]
    fn label_is_the_mark_then_the_title_and_the_logo_stands_apart() {
        assert_eq!(state_label_value(Some("Fix"), "done"), Some("✓ Fix".into()));
        assert_eq!(state_label_value(None, "blocked"), Some("?".into()));
        assert_eq!(
            state_label_value(Some("Fix"), "unknown"),
            Some("Fix".into())
        );
        assert_eq!(state_label_value(None, "unknown"), None);
        assert_eq!(state_label_token("heading_idle_stale"), "label_idle_stale");
        assert_eq!(icon_token("codex", false), "icon_codex");
        assert_eq!(icon_token("claude", true), "icon_stale");
    }

    #[test]
    fn space_shows_the_state_that_most_needs_you() {
        assert_eq!(space_status(["idle", "working", "done"]), "working");
        assert_eq!(space_status(["working", "blocked"]), "blocked");
        assert_eq!(space_status(["idle", "done"]), "done");
        assert_eq!(space_status([]), "none");
        assert_eq!(space_header_value("search", "done"), "✓ search");
        assert_eq!(space_header_value("search", "none"), "◦ search");
    }

    #[test]
    fn space_lead_is_the_first_session_in_the_spaces_state() {
        assert_eq!(space_lead(&["idle", "blocked", "blocked"]), Some(1));
        assert_eq!(space_lead(&[]), None);
    }

    #[test]
    fn a_working_space_spins_on_its_title_line() {
        let [plain, working, text] = space_title_tokens(Some("Fix"), "working");
        assert_eq!(plain.1, None);
        assert_eq!(working.1.as_deref(), Some("◐ Fix"));
        assert_eq!(text.1.as_deref(), Some("Fix"));
        let [plain, working, _] = space_title_tokens(Some("Fix"), "idle");
        assert_eq!((plain.1.as_deref(), working.1), (Some("Fix"), None));
        let spinning = json!({ "space_title_working": "◐ Fix", "space_title_text": "Fix" });
        assert_eq!(
            space_title_animation(&spinning, 9),
            Some(("space_title_working".into(), Some("⣯ Fix".into())))
        );
        let raced = json!({ "space_title_working": "⣷ Fix", "space_title": "Fix" });
        assert_eq!(
            space_title_animation(&raced, 0),
            Some(("space_title_working".into(), None))
        );
        assert_eq!(
            space_title_animation(&json!({ "space_title": "Fix" }), 0),
            None
        );
    }

    #[test]
    fn animation_spins_work_pulses_questions_and_lets_go_after_a_race() {
        let working = json!({ "label_working": "◐ Fix" });
        assert_eq!(
            animation(&working, "label", "Fix", 9),
            Some(("label_working".into(), Some("⣯ Fix".into())))
        );
        let blocked = json!({ "header_blocked": "? search" });
        assert_eq!(
            animation(&blocked, "header", "search", 0),
            Some(("header_blocked".into(), Some("? search".into())))
        );
        assert_eq!(
            animation(&blocked, "header", "search", PULSE_STEPS),
            Some(("header_blocked".into(), Some("\u{2800} search".into())))
        );
        let raced = json!({ "label_working": "⣷ Fix", "label_idle": "○ Fix" });
        assert_eq!(
            animation(&raced, "label", "Fix", 0),
            Some(("label_working".into(), None))
        );
        assert_eq!(
            animation(&json!({ "label_idle": "○ Fix" }), "label", "Fix", 0),
            None
        );
        let indented = json!({ "label_working": "\u{200b}    ◐ Fix" });
        assert_eq!(
            animation(&indented, "label", "Fix", 9),
            Some(("label_working".into(), Some("\u{200b}    ⣯ Fix".into())))
        );
        assert_eq!(heading_title("\u{ec82} Fix login"), "Fix login");
        assert_eq!(heading_title("\u{ec82}"), "");
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
    fn checkout_headers_mark_each_checkouts_first_agent() {
        let at = |project: &str, checkout: &str, label: &str| {
            Some(Place {
                project: project.into(),
                checkout: checkout.into(),
                label: label.into(),
                worktree: checkout.contains("/wt"),
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
    fn work_dirs_reads_cwd_edits_and_cd() {
        let line = json!({
            "type": "assistant",
            "cwd": "/w/wiki",
            "message": { "content": [
                { "type": "tool_use", "name": "Edit", "input": { "file_path": "/w/sim/src/lib.rs" } },
                { "type": "tool_use", "name": "Bash", "input": {
                    "command": "cd ~/dotfiles && git -C \"/w/other\" diff; cd relative; cd $X" } },
                { "type": "text", "text": "cd /not/a/tool" },
            ] },
        })
        .to_string();
        assert_eq!(
            work_dirs(&line, Path::new("/home/u")),
            ["/w/wiki", "/w/sim/src", "/home/u/dotfiles", "/w/other"]
        );
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
    fn untitled_claude_session_takes_its_first_real_question() {
        let home = TempDir::new().unwrap();
        let user = |c: Value| json!({ "type": "user", "message": { "content": c } });
        let path = home.path().join(".claude/projects/p/s1.jsonl");
        write(
            &path,
            &[
                user(json!("<command-name>/plan</command-name>")),
                user(json!([{ "type": "tool_result", "content": "x" }])),
                user(json!([{ "type": "text", "text": "  fix the\n login   bug " }])),
                user(json!("second question")),
            ],
        );
        let mut cache = Cache::default();
        let title = |cache: &mut Cache| claude_session(home.path(), "s1", cache).title;
        assert_eq!(title(&mut cache).as_deref(), Some("fix the login bug"));

        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(
            f,
            "{}",
            json!({ "type": "ai-title", "aiTitle": "Login fix" })
        )
        .unwrap();
        assert_eq!(title(&mut cache).as_deref(), Some("Login fix"));
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
    fn agent_logo_value_is_the_logo_and_name_when_any_run() {
        assert_eq!(agent_logo_value("claude", false), None);
        assert_eq!(agent_logo_value("claude", true).unwrap(), "\u{ec82} claude");
        assert_eq!(agent_logo_value("codex", true).unwrap(), "\u{ec81} codex");
        assert_eq!(agent_logo_value("pi", true), None);
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
