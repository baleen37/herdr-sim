//! The diff pane: a checkout's uncommitted changes or a PR's, as a file list
//! beside the selected file's diff, like reviewr. The Changes panel opens it
//! full screen with `d`; `q` closes it.

use std::io::{self, Write};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use crate::changes::{DIM, Event, GREEN, RED, RESET, SELECTED, cols, cut, listen_for_clicks, size};

/// The environment variable naming what to show: `checkout:<dir>` or
/// `pr:<url>`.
pub const SOURCE: &str = "HERDR_SIM_DIFF_SOURCE";
/// How often a checkout's diff is read again, so an agent's edits show up.
const WATCH: Duration = Duration::from_secs(2);
const CYAN: &str = "\x1b[38;2;137;220;235m";

/// One file of a unified diff: its path, lines added and removed, and its
/// hunks' lines (`@@` headers and `+`, `-`, ` ` lines).
#[derive(Debug, PartialEq)]
struct File {
    path: String,
    added: usize,
    removed: usize,
    lines: Vec<String>,
}

/// Runs the pane until `q`.
pub fn run() -> anyhow::Result<()> {
    let source = std::env::var(SOURCE).unwrap_or_default();
    let (events_in, events) = mpsc::channel::<Event>();
    listen_for_clicks(events_in);
    let mut files = read(&source);
    let mut selected = 0usize;
    let mut scroll = 0usize;
    loop {
        let (rows, width) = size();
        draw(&files, selected, scroll, rows, width);
        match events.recv_timeout(WATCH) {
            Ok(Event::Key(b'q')) => return Ok(()),
            Ok(Event::Key(b'j')) if selected + 1 < files.len() => {
                selected += 1;
                scroll = 0;
            }
            Ok(Event::Key(b'k')) if selected > 0 => {
                selected -= 1;
                scroll = 0;
            }
            // The diff scrolls by half a screen, or a wheel notch.
            Ok(Event::Key(4 | b'J' | b' ')) => scroll += rows / 2,
            Ok(Event::Key(21 | b'K')) => scroll = scroll.saturating_sub(rows / 2),
            Ok(Event::Scroll(by)) => scroll = scroll.saturating_add_signed(by),
            // A click on the file list picks that file.
            Ok(Event::Press { x, y }) if x <= list_width(width) => {
                if y >= 1 && y - 1 < files.len() {
                    selected = y - 1;
                    scroll = 0;
                }
            }
            Ok(_) => {}
            // Quiet: a checkout's diff is read again.
            Err(_) if source.starts_with("checkout:") => {
                files = read(&source);
                selected = selected.min(files.len().saturating_sub(1));
            }
            Err(_) => {}
        }
        let lines = files.get(selected).map_or(0, |f| f.lines.len());
        scroll = scroll.min(lines.saturating_sub(rows / 2));
    }
}

/// The unified diff `source` names, split into files.
fn read(source: &str) -> Vec<File> {
    if let Some(dir) = source.strip_prefix("checkout:") {
        return parse(&checkout_diff(dir));
    }
    let Some(url) = source.strip_prefix("pr:") else {
        return Vec::new();
    };
    crate::gh(&["pr", "diff", url, "--color", "never"], None)
        .map_or_else(Vec::new, |o| parse(&String::from_utf8_lossy(&o.stdout)))
}

/// The checkout's uncommitted changes against `HEAD`, new untracked files
/// included, as one unified diff.
fn checkout_diff(dir: &str) -> String {
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    let mut diff = git(&["diff", "HEAD", "--no-color", "--no-ext-diff"]);
    for file in git(&["ls-files", "--others", "--exclude-standard"]).lines() {
        diff += &git(&["diff", "--no-index", "--no-color", "--", "/dev/null", file]);
    }
    diff
}

/// Splits a unified diff into its files.
fn parse(diff: &str) -> Vec<File> {
    let mut files: Vec<File> = Vec::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // `a/path b/path`; the `+++` line, if any, names it better.
            let path = rest.split(" b/").last().unwrap_or(rest).to_string();
            files.push(File {
                path,
                added: 0,
                removed: 0,
                lines: Vec::new(),
            });
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if let Some(path) = line.strip_prefix("+++ b/") {
            file.path = path.to_string();
        } else if line.starts_with("@@") || file.lines.last().is_some() {
            if line.starts_with('+') {
                file.added += 1;
            } else if line.starts_with('-') {
                file.removed += 1;
            }
            file.lines.push(line.replace('\t', "    "));
        }
    }
    files
}

/// The file list's width for a pane `width` columns wide.
fn list_width(width: usize) -> usize {
    (width / 3).clamp(20, 48)
}

/// Draws the file list and the selected file's diff from line `scroll`.
fn draw(files: &[File], selected: usize, scroll: usize, rows: usize, width: usize) {
    let left = list_width(width);
    let right = width.saturating_sub(left + 1);
    let diff = files.get(selected).map(|f| &f.lines[..]).unwrap_or(&[]);
    let mut out = String::from("\x1b[?7l");
    for row in 0..rows {
        // File list: path, then its counts on the right.
        let entry = match files.get(row) {
            Some(f) => {
                let counts = format!("+{} -{}", f.added, f.removed);
                let path = cut(&f.path, left.saturating_sub(cols(&counts) + 2));
                let gap = left.saturating_sub(cols(&path) + cols(&counts) + 1);
                let bg = if row == selected { SELECTED } else { "" };
                format!(
                    "{bg} {path}{}{GREEN}+{}{RESET}{bg} {RED}-{}{RESET}",
                    " ".repeat(gap),
                    f.added,
                    f.removed
                )
            }
            None if row == 0 => format!(" {DIM}No changes.{RESET}"),
            None => String::new(),
        };
        let line = diff.get(scroll + row).map_or_else(String::new, |l| {
            let colour = match l.chars().next() {
                Some('+') => GREEN,
                Some('-') => RED,
                Some('@') => CYAN,
                _ => "",
            };
            format!("{colour}{}{RESET}", cut(l, right))
        });
        out += &format!(
            "\x1b[{row};1H\x1b[K{entry}\x1b[{row};{}H{DIM}│{RESET}{line}",
            left + 1,
            row = row + 1
        );
    }
    let mut stdout = io::stdout();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_splits_files_and_counts_lines() {
        let diff = "diff --git a/a.rs b/a.rs\nindex 1..2 100644\n--- a/a.rs\n+++ b/a.rs\n\
                    @@ -1,2 +1,2 @@\n ctx\n-old\n+new\n+more\n\
                    diff --git a/b.md b/b.md\n--- a/b.md\n+++ b/b.md\n@@ -1 +0,0 @@\n-gone\n";
        let files = parse(diff);
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0],
            File {
                path: "a.rs".into(),
                added: 2,
                removed: 1,
                lines: vec![
                    "@@ -1,2 +1,2 @@".into(),
                    " ctx".into(),
                    "-old".into(),
                    "+new".into(),
                    "+more".into()
                ],
            }
        );
        assert_eq!((files[1].path.as_str(), files[1].removed), ("b.md", 1));
    }
}
