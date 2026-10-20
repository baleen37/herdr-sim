//! The diff pane, laid out like reviewr: a checkout's uncommitted changes or a
//! PR's, the selected file's diff in one box and the changed files, as a
//! tree, in another. `d` in the Changes panel swaps the panel for it on the
//! right half of the tab; `q` swaps the panel back.

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
const YELLOW: &str = "\x1b[38;2;249;226;175m";
const BOLD: &str = "\x1b[1m";
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

/// Which box the keys move in.
#[derive(PartialEq, Clone, Copy)]
enum Focus {
    Files,
    Diff,
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
    let mut selected = 0usize;
    let mut scroll = 0usize;
    let mut focus = Focus::Files;
    loop {
        let (rows, width) = size();
        let body = rows.saturating_sub(4);
        let diff = files.get(selected).map(diff_lines).unwrap_or_default();
        scroll = scroll.min(diff.len().saturating_sub(body / 2));
        let tree = tree(&files);
        draw(
            &title, &files, &tree, selected, &diff, scroll, focus, rows, width,
        );
        // A file picked in the list shows its diff from the top.
        let pick = |to: usize, selected: &mut usize, scroll: &mut usize| {
            if to < files.len() && to != *selected {
                *selected = to;
                *scroll = 0;
            }
        };
        match events.recv_timeout(WATCH) {
            Ok(Event::Key(b'q')) => break,
            Ok(Event::Key(b'\t')) => {
                focus = if focus == Focus::Files {
                    Focus::Diff
                } else {
                    Focus::Files
                };
            }
            Ok(Event::Key(b'j')) if focus == Focus::Files => {
                pick(selected + 1, &mut selected, &mut scroll)
            }
            Ok(Event::Key(b'k')) if focus == Focus::Files => {
                pick(selected.saturating_sub(1), &mut selected, &mut scroll)
            }
            Ok(Event::Key(b'j')) => scroll += 1,
            Ok(Event::Key(b'k')) => scroll = scroll.saturating_sub(1),
            Ok(Event::Key(4 | b' ')) => scroll += body / 2,
            Ok(Event::Key(21)) => scroll = scroll.saturating_sub(body / 2),
            // f / F: the next or previous file, whichever box has focus.
            Ok(Event::Key(b'f')) => pick(selected + 1, &mut selected, &mut scroll),
            Ok(Event::Key(b'F')) => pick(selected.saturating_sub(1), &mut selected, &mut scroll),
            Ok(Event::Scroll(by)) => scroll = scroll.saturating_add_signed(by),
            // A click picks a file in the tree, or focuses the diff.
            Ok(Event::Press { x, y }) => {
                if x > diff_width(width) {
                    focus = Focus::Files;
                    let at = (y + list_offset(&tree, selected, body)).checked_sub(3);
                    if let Some(Some(i)) = at.and_then(|row| tree.get(row)).map(|t| t.1) {
                        pick(i, &mut selected, &mut scroll);
                    }
                } else {
                    focus = Focus::Diff;
                }
            }
            Ok(_) => {}
            Err(_) if source.starts_with("checkout:") => {
                files = read(&source);
                selected = selected.min(files.len().saturating_sub(1));
            }
            Err(_) => {}
        }
    }
    // The Changes panel back in this pane's place.
    if let (Some(me), Ok(agent)) = (me, std::env::var(FOR_PANE)) {
        reopen(herdr, &agent, &me);
    }
    Ok(())
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

/// The diff box's width; the file tree takes the rest.
fn diff_width(width: usize) -> usize {
    width.saturating_sub((width * 2 / 5).clamp(24, 36))
}

/// A file's diff as reviewr shows it: unchanged runs summarised, each line
/// with its number, changed ones marked.
fn diff_lines(file: &File) -> Vec<Row> {
    let mut out = Vec::new();
    // Where the new file's lines stand after the last hunk shown.
    let mut next = 1;
    for row in &file.rows {
        if row.kind == Kind::Hunk {
            let skipped = row.new.saturating_sub(next);
            if skipped > 0 {
                out.push(Row {
                    kind: Kind::Hunk,
                    old: 0,
                    new: skipped,
                    text: String::new(),
                });
            }
            continue;
        }
        if row.kind != Kind::Removed {
            next = row.new + 1;
        }
        out.push(Row {
            kind: row.kind,
            old: row.old,
            new: row.new,
            text: row.text.clone(),
        });
    }
    out
}

/// The changed files as a tree: a row per folder, then its files, each row
/// with the file it is, if any.
fn tree(files: &[File]) -> Vec<(String, Option<usize>)> {
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by(|&a, &b| files[a].path.cmp(&files[b].path));
    let mut out = Vec::new();
    let mut dir_shown: Option<&str> = None;
    for i in order {
        let (dir, _) = files[i].path.rsplit_once('/').unwrap_or(("", ""));
        if !dir.is_empty() && dir_shown != Some(dir) {
            out.push((format!("{dir}/"), None));
        }
        dir_shown = Some(dir);
        out.push((String::new(), Some(i)));
    }
    out
}

/// The first tree row shown, so the selected file is in view.
fn list_offset(tree: &[(String, Option<usize>)], selected: usize, body: usize) -> usize {
    let at = tree.iter().position(|t| t.1 == Some(selected)).unwrap_or(0);
    at.saturating_sub(body.saturating_sub(1))
}

/// A box's top or bottom: `┌ title ───┐`, the title bright when the box has
/// focus.
fn edge(left: char, title: &str, right: char, width: usize, focused: bool) -> String {
    let inner = width.saturating_sub(2);
    let title = if title.is_empty() {
        String::new()
    } else {
        format!(" {} ", cut(title, inner.saturating_sub(3)))
    };
    let fill = "─".repeat(inner.saturating_sub(cols(&title) + 1));
    let style = if focused { BOLD } else { DIM };
    format!("{DIM}{left}─{RESET}{style}{title}{RESET}{DIM}{fill}{right}{RESET}")
}

/// Draws the header, the diff and file boxes, and the key hints.
#[allow(clippy::too_many_arguments)]
fn draw(
    title: &str,
    files: &[File],
    tree: &[(String, Option<usize>)],
    selected: usize,
    diff: &[Row],
    scroll: usize,
    focus: Focus,
    rows: usize,
    width: usize,
) {
    let dw = diff_width(width);
    let fw = width - dw;
    let body = rows.saturating_sub(4);
    let (added, removed) = files
        .iter()
        .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
    let stats = format!("{} changed  +{added} −{removed}", files.len());
    let gap = width.saturating_sub(cols(title) + cols(&stats) + 2);
    let mut lines = vec![format!(
        " {title}{}{DIM}{} changed{RESET}  {GREEN}+{added}{RESET} {RED}−{removed}{RESET}",
        " ".repeat(gap),
        files.len()
    )];
    let path = files.get(selected).map_or("Diff", |f| f.path.as_str());
    lines.push(
        edge('┌', path, '┐', dw, focus == Focus::Diff)
            + &edge('┌', "Files", '┐', fw, focus == Focus::Files),
    );
    let digits = diff
        .iter()
        .map(|r| r.old.max(r.new))
        .max()
        .unwrap_or(0)
        .to_string()
        .len()
        .max(3);
    let offset = list_offset(tree, selected, body);
    for i in 0..body {
        let left = match diff.get(scroll + i) {
            Some(r) => diff_row(r, digits, dw - 2),
            None if i == 0 && files.is_empty() => format!("{DIM}no changes{RESET}"),
            None => String::new(),
        };
        let right = match tree.get(offset + i) {
            Some((dir, None)) => format!("{DIM}▾ {}{RESET}", cut(dir, fw - 4)),
            Some((_, Some(f))) => tree_row(&files[*f], *f == selected, fw - 2),
            None => String::new(),
        };
        lines.push(format!(
            "{DIM}│{RESET}{left}\x1b[{row};{dw}H{DIM}│{RESET}{DIM}│{RESET}{right}\x1b[{row};{width}H{DIM}│{RESET}",
            row = i + 3,
        ));
    }
    lines.push(edge('└', "", '┘', dw, false) + &edge('└', "", '┘', fw, false));
    let hints = if focus == Focus::Files {
        "j/k file · tab diff · q back to Changes"
    } else {
        "j/k scroll · f/F file · tab files · q back to Changes"
    };
    lines.push(format!(" {DIM}{hints}{RESET}"));

    let mut out = String::from("\x1b[?7l");
    for (i, line) in lines.iter().enumerate() {
        out += &format!("\x1b[{};1H\x1b[K{line}", i + 1);
    }
    let mut stdout = io::stdout();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
}

/// One diff line, `width` columns wide: changed lines get a bar and a tint,
/// and every line its number.
fn diff_row(row: &Row, digits: usize, width: usize) -> String {
    let text_cols = width.saturating_sub(digits + 3);
    let text = cut(&row.text, text_cols);
    match row.kind {
        Kind::Hunk => format!(
            "  {DIM}⋯  {} unmodified line{}{RESET}",
            row.new,
            if row.new == 1 { "" } else { "s" }
        ),
        Kind::Context => format!(" {DIM}{:>digits$}{RESET}  {text}", row.new),
        Kind::Added | Kind::Removed => {
            let (bar, bg, number) = if row.kind == Kind::Added {
                (GREEN, ADDED_BG, row.new)
            } else {
                (RED, REMOVED_BG, row.old)
            };
            let pad = " ".repeat(text_cols.saturating_sub(cols(&text)));
            format!("{bar}▌{RESET}{bg}{DIM}{number:>digits$}{RESET}{bg}  {text}{pad}{RESET}")
        }
    }
}

/// A file in the tree, `width` columns wide: status, name, counts.
fn tree_row(file: &File, selected: bool, width: usize) -> String {
    let colour = match file.status {
        'A' => GREEN,
        'D' => RED,
        'R' => CYAN,
        _ => YELLOW,
    };
    let name = file.path.rsplit('/').next().unwrap_or(&file.path);
    let indent = if file.path.contains('/') { "  " } else { "" };
    let counts = format!("+{} −{}", file.added, file.removed);
    let room = width.saturating_sub(cols(indent) + cols(&counts) + 4);
    // A long name keeps its end, as reviewr does.
    let name = if cols(name) > room {
        let tail: Vec<char> = name.chars().rev().take(room.saturating_sub(1)).collect();
        format!("…{}", tail.into_iter().rev().collect::<String>())
    } else {
        name.to_string()
    };
    let gap = width.saturating_sub(cols(indent) + cols(&name) + cols(&counts) + 3);
    let bg = if selected { SELECTED } else { "" };
    format!(
        "{bg}{indent}{colour}{}{RESET}{bg} {name}{}{GREEN}+{}{RESET}{bg} {RED}−{}{RESET}{bg} {RESET}",
        file.status,
        " ".repeat(gap),
        file.added,
        file.removed
    )
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
