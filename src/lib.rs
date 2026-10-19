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

/// PR URLs a Claude Code session linked, in first-seen order.
pub fn claude_prs(home: &Path, session_id: &str, cache: &mut Cache) -> Vec<String> {
    let file = format!("{session_id}.jsonl");
    let mut urls = Vec::new();
    for project in subdirs(&home.join(".claude/projects")) {
        urls.extend(cache.scan(&project.join(&file), |line| {
            if !line.contains(r#""type":"pr-link""#) {
                return Vec::new();
            }
            parse(line)
                .and_then(|v| v["prUrl"].as_str().map(String::from))
                .into_iter()
                .collect()
        }));
    }
    dedup(urls)
}

/// PR URLs printed by `gh pr ...` commands in a Codex session, in first-seen order.
pub fn codex_prs(home: &Path, session_id: &str, cache: &mut Cache) -> Vec<String> {
    let suffix = format!("{session_id}.jsonl");
    let mut urls = Vec::new();
    for year in subdirs(&home.join(".codex/sessions")) {
        for month in subdirs(&year) {
            for day in subdirs(&month) {
                for rollout in files_ending_with(&day, &suffix) {
                    urls.extend(cache.scan(&rollout, |line| {
                        if !line.contains(r#""CommandExecution""#) || !line.contains("/pull/") {
                            return Vec::new();
                        }
                        let Some(v) = parse(line) else {
                            return Vec::new();
                        };
                        let item = &v["payload"]["item"];
                        if !is_gh_pr_command(&item["command"].to_string()) {
                            return Vec::new();
                        }
                        pr_urls(item["aggregated_output"].as_str().unwrap_or(""))
                    }));
                }
            }
        }
    }
    dedup(urls)
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
    branches: HashMap<String, (Option<PrState>, u64)>,
    files: HashMap<String, Scanned>,
}

#[derive(Default)]
struct Scanned {
    offset: u64,
    urls: Vec<String>,
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
                let state = v["state"].as_str().and_then(PrState::parse);
                Some((key.clone(), (state, v["at"].as_u64()?)))
            })
            .collect();
        let files = object(&v["files"])
            .filter_map(|(path, v)| {
                let urls = v["urls"].as_array()?.iter().filter_map(|u| u.as_str());
                let scanned = Scanned {
                    offset: v["offset"].as_u64()?,
                    urls: urls.map(String::from).collect(),
                };
                Some((path.clone(), scanned))
            })
            .collect();
        Self {
            prs,
            branches,
            files,
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
            .map(|(key, (state, at))| {
                (
                    key.clone(),
                    json!({ "state": state.map(PrState::name), "at": at }),
                )
            })
            .collect();
        let files: serde_json::Map<String, Value> = self
            .files
            .iter()
            .map(|(path, s)| (path.clone(), json!({ "offset": s.offset, "urls": s.urls })))
            .collect();
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        fs::write(
            &tmp,
            json!({ "prs": prs, "branches": branches, "files": files }).to_string(),
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

    /// State of the PR for a checked-out branch, keyed by `key` (repo + branch).
    /// Like [`Cache::state`], but a branch may have no PR yet, which is
    /// re-checked as often as an open one. `fetch` returns `None` on failure,
    /// `Some(None)` when the branch has no PR.
    pub fn branch_state(
        &mut self,
        key: &str,
        now: u64,
        fetch: impl FnOnce() -> Option<Option<PrState>>,
    ) -> Option<PrState> {
        let cached = self.branches.get(key).copied();
        if let Some((state, at)) = cached
            && (matches!(state, Some(PrState::Merged | PrState::Closed))
                || now.saturating_sub(at) < OPEN_TTL_SECS)
        {
            return state;
        }
        match fetch() {
            Some(state) => {
                self.branches.insert(key.to_string(), (state, now));
                state
            }
            None => cached.and_then(|(state, _)| state),
        }
    }

    /// URLs `extract` finds in `path`'s lines, reading only complete lines
    /// appended since the previous scan. Session files are append-only; one
    /// that shrank was rewritten and is read again from the start.
    fn scan(&mut self, path: &Path, mut extract: impl FnMut(&str) -> Vec<String>) -> Vec<String> {
        let Ok(file) = File::open(path) else {
            return Vec::new();
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
                scanned
                    .urls
                    .extend(extract(&String::from_utf8_lossy(&line)));
                line.clear();
            }
        }
        scanned.urls.clone()
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
            claude_prs(home.path(), "s1", &mut cache),
            [
                "https://github.com/o/search/pull/1",
                "https://github.com/o/batch/pull/2"
            ]
        );
        assert!(claude_prs(home.path(), "other", &mut cache).is_empty());
    }

    #[test]
    fn scan_reads_only_complete_appended_lines() {
        let home = TempDir::new().unwrap();
        let path = home.path().join(".claude/projects/p/s1.jsonl");
        write(&path, &[link(1)]);
        let mut cache = Cache::default();
        assert_eq!(claude_prs(home.path(), "s1", &mut cache).len(), 1);

        // A line still being written (no newline yet) is left for the next run.
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(f, "{}", link(2)).unwrap();
        assert_eq!(claude_prs(home.path(), "s1", &mut cache).len(), 1);
        writeln!(f).unwrap();
        assert_eq!(claude_prs(home.path(), "s1", &mut cache).len(), 2);

        // The offset survives a save/load, so already-read lines are not read
        // again: editing the first line in place (same length) changes nothing.
        let saved = home.path().join("cache.json");
        cache.save(&saved).unwrap();
        let text = fs::read_to_string(&path)
            .unwrap()
            .replacen("pull/1", "pull/9", 1);
        fs::write(&path, text).unwrap();
        let urls = claude_prs(home.path(), "s1", &mut Cache::load(&saved));
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
            codex_prs(home.path(), "c1", &mut Cache::default()),
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
        let mut cache = Cache::default();
        assert_eq!(cache.branch_state("r#b", 0, || Some(None)), None);
        assert_eq!(
            cache.branch_state("r#b", 59, || panic!("fresh entry refetched")),
            None
        );
        assert_eq!(
            cache.branch_state("r#b", 60, || Some(Some(PrState::Open))),
            Some(PrState::Open)
        );
        assert_eq!(cache.branch_state("r#b", 120, || None), Some(PrState::Open));
        assert_eq!(
            cache.branch_state("r#b", 180, || Some(Some(PrState::Merged))),
            Some(PrState::Merged)
        );
        assert_eq!(
            cache.branch_state("r#b", 99_999, || panic!("merged refetched")),
            Some(PrState::Merged)
        );
    }

    #[test]
    fn cache_round_trips_through_disk() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cache.json");
        let mut cache = Cache::default();
        cache.state("u", 5, |_| Some(PrState::Closed));
        cache.branch_state("r#b", 5, || Some(Some(PrState::Merged)));
        cache.save(&path).unwrap();
        let mut loaded = Cache::load(&path);
        assert_eq!(
            loaded.branch_state("r#b", 1_000, || panic!("not cached")),
            Some(PrState::Merged)
        );
        assert_eq!(
            loaded.state("u", 1_000, |_| panic!("not cached")),
            Some(PrState::Closed)
        );
    }
}
