//! The diff pane: a checkout's uncommitted changes or a PR's, as Claude Code
//! shows them — every file in one stream, each under a header that folds it,
//! unchanged runs summarised, added and removed lines tinted. `d` in the
//! Changes panel swaps the panel for it on the right half of the tab; `q`
//! swaps the panel back.

use std::collections::HashSet;
use std::io::{self, Write};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use crate::changes::{
    DIM, Event, FOR_PANE, GREEN, RED, RESET, SELECTED, cols, cut, fit_width, listen_for_clicks,
    reopen, size, wait_gone,
};

/// The environment variable naming what to show: `checkout:<dir>` or
/// `pr:<url>`.
pub const SOURCE: &str = "HERDR_SIM_DIFF_SOURCE";
/// How often a checkout's diff is read again, so an agent's edits show up.
const WATCH: Duration = Duration::from_secs(2);
const CYAN: &str = "\x1b[38;2;137;220;235m";
/// Added and removed lines' backgrounds, as delta tints them.
const ADDED_BG: &str = "\x1b[48;2;33;58;43m";
const REMOVED_BG: &str = "\x1b[48;2;63;34;44m";

/// One file of a unified diff.
#[derive(Debug, PartialEq)]
struct File {
    path: String,
    /// `M`odified, `A`dded, `D`eleted or `R`enamed.
    status: char,
    added: usize,
    removed: usize,
    rows: Vec<Row>,
}

/// A line of a file's diff, with its numbers in the old and new file.
#[derive(Debug, PartialEq)]
struct Row {
    kind: Kind,
    old: usize,
    new: usize,
    text: String,
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Kind {
    /// A hunk's start; its text is the hunk header's context.
    Hunk,
    Context,
    Added,
    Removed,
}

/// Runs the pane until `q`.
pub fn run(herdr: &str) -> anyhow::Result<()> {
    let source = std::env::var(SOURCE).unwrap_or_default();
    let me = std::env::var("HERDR_PANE_ID").ok();
    // The right half of the tab, once the panel it replaces has gone.
    if let Some(me) = &me {
        wait_gone(herdr);
        fit_width(herdr, me, None);
    }
    let (events_in, events) = mpsc::channel::<Event>();
    listen_for_clicks(events_in);
    let title = title(&source);
    let mut files = read(&source);
    let mut folded: HashSet<String> = HashSet::new();
    let mut scroll = 0usize;
    loop {
        let (rows, width) = size();
        let body = rows.saturating_sub(2);
        let lines = stream(&files, &folded, width);
        scroll = scroll.min(lines.len().saturating_sub(body));
        draw(&title, &files, &lines, scroll, rows, width);
        let headers: Vec<usize> = (0..lines.len()).filter(|&i| lines[i].1.is_some()).collect();
        match events.recv_timeout(WATCH) {
            Ok(Event::Key(b'q')) => break,
            Ok(Event::Key(b'j')) => scroll += 1,
            Ok(Event::Key(b'k')) => scroll = scroll.saturating_sub(1),
            Ok(Event::Key(4 | b'J' | b' ')) => scroll += body / 2,
            Ok(Event::Key(21 | b'K')) => scroll = scroll.saturating_sub(body / 2),
            Ok(Event::Key(b'g')) => scroll = 0,
            Ok(Event::Key(b'G')) => scroll = usize::MAX,
            // f / F: the next or previous file.
            Ok(Event::Key(b'f')) => {
                scroll = headers
                    .iter()
                    .copied()
                    .find(|&h| h > scroll)
                    .unwrap_or(scroll);
            }
            Ok(Event::Key(b'F')) => {
                scroll = headers
                    .iter()
                    .rev()
                    .copied()
                    .find(|&h| h < scroll)
                    .unwrap_or(0);
            }
            // Enter or o folds the file at the top, or unfolds it.
            Ok(Event::Key(b'\n' | b'\r' | b'o')) => {
                let at = headers.iter().rev().copied().find(|&h| h <= scroll);
                if let Some(path) = at.and_then(|h| lines[h].1).map(|i| files[i].path.clone()) {
                    toggle(&mut folded, path);
                }
            }
            Ok(Event::Scroll(by)) => scroll = scroll.saturating_add_signed(by),
            // A click on a file's header folds or unfolds it.
            Ok(Event::Press { y, .. }) => {
                let line = (y + scroll).checked_sub(2);
                if let Some(i) = line.and_then(|l| lines.get(l)).and_then(|l| l.1) {
                    toggle(&mut folded, files[i].path.clone());
                }
            }
            Ok(_) => {}
            Err(_) if source.starts_with("checkout:") => files = read(&source),
            Err(_) => {}
        }
    }
    // The Changes panel back in this pane's place.
    if let (Some(me), Ok(agent)) = (me, std::env::var(FOR_PANE)) {
        reopen(herdr, &agent, &me);
    }
    Ok(())
}

fn toggle(folded: &mut HashSet<String>, path: String) {
    if !folded.remove(&path) {
        folded.insert(path);
    }
}

/// The top bar: the branch and what it is compared with.
fn title(source: &str) -> String {
    if let Some(dir) = source.strip_prefix("checkout:") {
        let branch = Command::new("git")
            .args(["-C", dir, "branch", "--show-current"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        return format!("{branch} {DIM}· uncommitted{RESET}");
    }
    let Some(url) = source.strip_prefix("pr:") else {
        return String::new();
    };
    let refs = crate::gh(
        &["pr", "view", url, "--json", "baseRefName,headRefName"],
        None,
    )
    .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok());
    match refs {
        Some(v) => format!(
            "{} {DIM}→{RESET} {}",
            v["baseRefName"].as_str().unwrap_or(""),
            v["headRefName"].as_str().unwrap_or("")
        ),
        None => url.to_string(),
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

/// Splits a unified diff into its files, numbering their lines.
fn parse(diff: &str) -> Vec<File> {
    let mut files: Vec<File> = Vec::new();
    let (mut old, mut new) = (0, 0);
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // `a/path b/path`; the `+++` line, if any, names it better.
            let path = rest.split(" b/").last().unwrap_or(rest).to_string();
            files.push(File {
                path,
                status: 'M',
                added: 0,
                removed: 0,
                rows: Vec::new(),
            });
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if file.rows.is_empty() && !line.starts_with("@@") {
            // The file's header lines.
            if line.starts_with("new file") {
                file.status = 'A';
            } else if line.starts_with("deleted file") {
                file.status = 'D';
            } else if line.starts_with("rename from") {
                file.status = 'R';
            } else if let Some(path) = line.strip_prefix("+++ b/") {
                file.path = path.to_string();
            } else if let Some(path) = line.strip_prefix("--- a/")
                && file.status == 'D'
            {
                file.path = path.to_string();
            }
            continue;
        }
        let text = line.get(1..).unwrap_or("").replace('\t', "    ");
        let row = |kind, old, new, text| Row {
            kind,
            old,
            new,
            text,
        };
        match line.chars().next() {
            Some('@') => {
                // `@@ -old,n +new,m @@ context`
                let mut nums = line.split(' ').skip(1);
                old = start(nums.next());
                new = start(nums.next());
                let context = line.splitn(3, "@@").nth(2).unwrap_or("").trim().to_string();
                file.rows.push(row(Kind::Hunk, old, new, context));
            }
            Some('+') => {
                file.added += 1;
                file.rows.push(row(Kind::Added, 0, new, text));
                new += 1;
            }
            Some('-') => {
                file.removed += 1;
                file.rows.push(row(Kind::Removed, old, 0, text));
                old += 1;
            }
            Some(' ') => {
                file.rows.push(row(Kind::Context, old, new, text));
                old += 1;
                new += 1;
            }
            _ => {}
        }
    }
    files
}

/// The first line number in a hunk header's `-12,3` or `+12,3`.
fn start(range: Option<&str>) -> usize {
    range
        .and_then(|r| r.get(1..)?.split(',').next()?.parse().ok())
        .unwrap_or(0)
}

/// Every file's lines, one under another, `width` columns wide; each line
/// with the file it heads, if it is a file's header.
fn stream(files: &[File], folded: &HashSet<String>, width: usize) -> Vec<(String, Option<usize>)> {
    let digits = files
        .iter()
        .flat_map(|f| f.rows.iter().map(|r| r.old.max(r.new)))
        .max()
        .unwrap_or(0)
        .to_string()
        .len()
        .max(2);
    let mut out: Vec<(String, Option<usize>)> = Vec::new();
    for (i, file) in files.iter().enumerate() {
        let open = !folded.contains(&file.path);
        out.push((header(file, open, width), Some(i)));
        if !open {
            continue;
        }
        // Where the new file's lines stand after the last hunk shown.
        let mut next = 1;
        for row in &file.rows {
            if row.kind == Kind::Hunk {
                let skipped = row.new.saturating_sub(next);
                if skipped > 0 {
                    let text = format!(
                        "  {DIM}⌃ {skipped} unmodified line{}{RESET}",
                        if skipped == 1 { "" } else { "s" }
                    );
                    out.push((text, None));
                }
                continue;
            }
            if row.kind != Kind::Removed {
                next = row.new + 1;
            }
            out.push((line(row, digits, width), None));
        }
        out.push((String::new(), None));
    }
    out
}

/// A file's header: fold mark, name, its folder dimmed, and counts, on a
/// raised background across the pane.
fn header(file: &File, open: bool, width: usize) -> String {
    let (dir, name) = match file.path.rsplit_once('/') {
        Some((dir, name)) => (dir, name),
        None => ("", file.path.as_str()),
    };
    let mark = if open { '▾' } else { '▸' };
    let status = match file.status {
        'A' => format!(" {GREEN}new{RESET}{SELECTED}"),
        'D' => format!(" {RED}deleted{RESET}{SELECTED}"),
        'R' => format!(" {CYAN}renamed{RESET}{SELECTED}"),
        _ => String::new(),
    };
    let counts = format!("+{} -{}", file.added, file.removed);
    let room = width.saturating_sub(cols(name) + cols(&counts) + 8);
    let dir = cut(dir, room);
    let text_cols = cols(name)
        + cols(&dir)
        + cols(&counts)
        + 5
        + match file.status {
            'A' => 4,
            'D' => 8,
            'R' => 8,
            _ => 0,
        };
    format!(
        "{SELECTED} {DIM}{mark}{RESET}{SELECTED} {name} {DIM}{dir}{RESET}{SELECTED}{status} \
         {GREEN}+{}{RESET}{SELECTED} {RED}-{}{RESET}{SELECTED}{}{RESET}",
        file.added,
        file.removed,
        " ".repeat(width.saturating_sub(text_cols))
    )
}

/// One diff line: its number, `+` or `-`, and the text, added and removed
/// lines tinted across the pane with a bar at the edge.
fn line(row: &Row, digits: usize, width: usize) -> String {
    let text_cols = width.saturating_sub(digits + 4);
    let text = cut(&row.text, text_cols);
    match row.kind {
        Kind::Added | Kind::Removed => {
            let (bar, bg, sign, number) = if row.kind == Kind::Added {
                (GREEN, ADDED_BG, '+', row.new)
            } else {
                (RED, REMOVED_BG, '-', row.old)
            };
            let pad = " ".repeat(text_cols.saturating_sub(cols(&text)));
            format!(
                "{bar}▌{RESET}{bg}{DIM}{number:>digits$}{RESET}{bg} {bar}{sign}{RESET}{bg} {text}{pad}{RESET}"
            )
        }
        _ => format!(" {DIM}{:>digits$}{RESET}   {text}", row.new),
    }
}

/// Draws the top bar, the stream from line `scroll`, and the key hints.
fn draw(
    title: &str,
    files: &[File],
    lines: &[(String, Option<usize>)],
    scroll: usize,
    rows: usize,
    width: usize,
) {
    let (added, removed) = files
        .iter()
        .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
    let stats = format!("{} files  +{added} -{removed}", files.len());
    let gap = width.saturating_sub(cols(title) + cols(&stats) + 2);
    let mut out = String::from("\x1b[?7l");
    out += &format!(
        "\x1b[1;1H\x1b[K {title}{}{DIM}{} files{RESET}  {GREEN}+{added}{RESET} {RED}-{removed}{RESET}",
        " ".repeat(gap),
        files.len()
    );
    let body = rows.saturating_sub(2);
    for i in 0..body {
        let text = match lines.get(scroll + i) {
            Some((text, _)) => text.as_str(),
            None if i == 0 && files.is_empty() => "  No changes.",
            None => "",
        };
        out += &format!("\x1b[{};1H\x1b[K{text}", i + 2);
    }
    out += &format!(
        "\x1b[{rows};1H\x1b[K {DIM}j/k scroll · f/F file · o fold · q back to Changes{RESET}"
    );
    let mut stdout = io::stdout();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_numbers_lines_and_reads_status() {
        let diff = "diff --git a/a.rs b/a.rs\nindex 1..2 100644\n--- a/a.rs\n+++ b/a.rs\n\
                    @@ -10,3 +10,3 @@ fn main() {\n ctx\n-old\n+new\n+more\n\
                    diff --git a/b.md b/b.md\nnew file mode 100644\n--- /dev/null\n+++ b/b.md\n\
                    @@ -0,0 +1 @@\n+hi\n";
        let files = parse(diff);
        assert_eq!(files.len(), 2);
        let a = &files[0];
        assert_eq!(
            (a.path.as_str(), a.status, a.added, a.removed),
            ("a.rs", 'M', 2, 1)
        );
        let row = |kind, old, new, text: &str| Row {
            kind,
            old,
            new,
            text: text.into(),
        };
        assert_eq!(
            a.rows,
            vec![
                row(Kind::Hunk, 10, 10, "fn main() {"),
                row(Kind::Context, 10, 10, "ctx"),
                row(Kind::Removed, 11, 0, "old"),
                row(Kind::Added, 0, 11, "new"),
                row(Kind::Added, 0, 12, "more"),
            ]
        );
        assert_eq!((files[1].path.as_str(), files[1].status), ("b.md", 'A'));
    }
}
