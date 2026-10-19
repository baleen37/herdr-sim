//! Find the pull requests a coding-agent session touched and summarise them as
//! Herdr sidebar tokens.
//!
//! Claude Code records `pr-link` lines in its transcript whenever a session
//! creates, pushes to, or edits a PR. Codex keeps no such record, so its PRs are
//! taken from the output of `gh pr ...` commands in the session rollout.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
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

/// Token value for `count` PRs in one state: the icon, plus the count when
/// there is more than one. `None` clears the token.
pub fn token_value(state: PrState, count: usize) -> Option<String> {
    match count {
        0 => None,
        1 => Some(state.icon().to_string()),
        n => Some(format!("{}{n}", state.icon())),
    }
}

/// PR URLs a Claude Code session linked, in first-seen order.
pub fn claude_prs(home: &Path, session_id: &str) -> Vec<String> {
    let file = format!("{session_id}.jsonl");
    let mut urls = Vec::new();
    for project in subdirs(&home.join(".claude/projects")) {
        for_each_line(&project.join(&file), |line| {
            if !line.contains(r#""type":"pr-link""#) {
                return;
            }
            if let Some(url) = parse(line).and_then(|v| v["prUrl"].as_str().map(String::from)) {
                urls.push(url);
            }
        });
    }
    dedup(urls)
}

/// PR URLs printed by `gh pr ...` commands in a Codex session, in first-seen order.
pub fn codex_prs(home: &Path, session_id: &str) -> Vec<String> {
    let suffix = format!("{session_id}.jsonl");
    let mut urls = Vec::new();
    for year in subdirs(&home.join(".codex/sessions")) {
        for month in subdirs(&year) {
            for day in subdirs(&month) {
                for rollout in files_ending_with(&day, &suffix) {
                    for_each_line(&rollout, |line| {
                        if !line.contains(r#""CommandExecution""#) || !line.contains("/pull/") {
                            return;
                        }
                        let Some(v) = parse(line) else { return };
                        let item = &v["payload"]["item"];
                        if is_gh_pr_command(&item["command"].to_string()) {
                            urls.extend(pr_urls(item["aggregated_output"].as_str().unwrap_or("")));
                        }
                    });
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

/// PR states keyed by URL, persisted between runs.
#[derive(Default)]
pub struct Cache {
    entries: HashMap<String, (PrState, u64)>,
}

impl Cache {
    pub fn load(path: &Path) -> Self {
        let Some(Value::Object(map)) = fs::read_to_string(path).ok().and_then(|s| parse(&s)) else {
            return Self::default();
        };
        let entries = map
            .into_iter()
            .filter_map(|(url, v)| {
                let state = PrState::parse(v["state"].as_str()?)?;
                Some((url, (state, v["at"].as_u64()?)))
            })
            .collect();
        Self { entries }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let map: serde_json::Map<String, Value> = self
            .entries
            .iter()
            .map(|(url, (state, at))| {
                let state = match state {
                    PrState::Open => "OPEN",
                    PrState::Merged => "MERGED",
                    PrState::Closed => "CLOSED",
                };
                (url.clone(), json!({ "state": state, "at": at }))
            })
            .collect();
        fs::write(path, Value::Object(map).to_string())
    }

    /// The PR's state, calling `fetch` only when the cached one may be stale.
    pub fn state(
        &mut self,
        url: &str,
        now: u64,
        fetch: impl FnOnce(&str) -> Option<PrState>,
    ) -> Option<PrState> {
        if let Some(&(state, at)) = self.entries.get(url)
            && (state != PrState::Open || now.saturating_sub(at) < OPEN_TTL_SECS)
        {
            return Some(state);
        }
        let state = fetch(url)?;
        self.entries.insert(url.to_string(), (state, now));
        Some(state)
    }
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

fn for_each_line(path: &Path, mut f: impl FnMut(&str)) {
    let Ok(file) = File::open(path) else { return };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        f(&line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path, lines: &[Value]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: Vec<String> = lines.iter().map(Value::to_string).collect();
        fs::write(path, body.join("\n")).unwrap();
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
        assert_eq!(
            claude_prs(home.path(), "s1"),
            [
                "https://github.com/o/search/pull/1",
                "https://github.com/o/batch/pull/2"
            ]
        );
        assert!(claude_prs(home.path(), "other").is_empty());
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
            codex_prs(home.path(), "c1"),
            [
                "https://github.com/o/infra/pull/750",
                "https://github.com/o/infra/pull/751"
            ]
        );
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
    fn cache_round_trips_through_disk() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cache.json");
        let mut cache = Cache::default();
        cache.state("u", 5, |_| Some(PrState::Closed));
        cache.save(&path).unwrap();
        let mut loaded = Cache::load(&path);
        assert_eq!(
            loaded.state("u", 1_000, |_| panic!("not cached")),
            Some(PrState::Closed)
        );
    }
}
