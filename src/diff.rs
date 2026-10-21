//! The diff pane, laid out like reviewr: a checkout's uncommitted changes or a
//! PR's, the selected file's diff in one box and the changed files, as a
//! tree, in another. `d` in the Changes panel swaps the panel for it on the
//! right half of the tab; `q` swaps the panel back.

use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::Duration;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::changes::{
    DIM, Event, FOR_PANE, GREEN, RED, RESET, SELECTED, cols, cut, fit_width, listen_for_clicks,
    reopen, size, wait_gone,
};

/// The environment variable naming what to show: `checkout:<dir>` or
/// `pr:<url>`.
pub const SOURCE: &str = "HERDR_SIM_DIFF_SOURCE";
/// How often a checkout's diff is read again, so an agent's edits show up.
const WATCH: Duration = Duration::from_secs(2);
/// Quiet ticks between full re-reads, which also find new untracked files.
const RESCAN_TICKS: u32 = 5;
const CYAN: &str = "\x1b[38;2;137;220;235m";
const YELLOW: &str = "\x1b[38;2;249;226;175m";
const BOLD: &str = "\x1b[1m";
/// Added and removed lines' backgrounds, as delta tints them.
const ADDED_BG: &str = "\x1b[48;2;33;58;43m";
const REMOVED_BG: &str = "\x1b[48;2;63;34;44m";

/// One file of a unified diff.
#[derive(Debug, PartialEq, Clone)]
struct File {
    path: String,
    /// `M`odified, `A`dded, `D`eleted or `R`enamed.
    status: char,
    added: usize,
    removed: usize,
    rows: Vec<Row>,
}

/// A line of a file's diff, with its numbers in the old and new file.
#[derive(Debug, PartialEq, Clone)]
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
    let wake = events_in.clone();
    let mut watch = crate::watch::Watch::new(events_in.clone());
    if let (Some(watch), Some(dir)) = (watch.as_mut(), source.strip_prefix("checkout:")) {
        watch.add(dir);
    }
    listen_for_clicks(events_in);
    // Loading the syntaxes takes a moment; start now, off this thread.
    std::thread::spawn(|| {
        highlighting();
    });
    // Highlighted lines come back from a worker, for a file and the version
    // of the diff they were made from.
    let (coloured_in, coloured) = mpsc::channel::<(usize, u64, Vec<Shown>)>();
    let mut version = 0u64;
    // The title and the diff come from separate git or gh calls; ask both at
    // once.
    let asked = source.clone();
    let title = std::thread::spawn(move || title(&asked));
    let mut files = read(&source);
    let title = title.join().unwrap_or_default();
    let mut selected = 0usize;
    let mut scroll = 0usize;
    let mut focus = Focus::Files;
    let mut tree_shown = true;
    let mut pending_g = false;
    // What a checkout looked like when last read, and quiet ticks since.
    let mut last_print = 0u64;
    let mut quiet = 0u32;
    // The selected file's highlighted lines, kept until it or the diff
    // changes: highlighting is the costly part of a frame.
    let mut shown: Option<(usize, Vec<Shown>)> = None;
    loop {
        let (rows, width) = size();
        let body = rows.saturating_sub(4);
        while let Ok((at, made_from, lines)) = coloured.try_recv() {
            if at == selected && made_from == version {
                shown = Some((at, lines));
            }
        }
        // A file not shown yet: plain at once, highlighted when ready.
        if shown.as_ref().is_none_or(|(at, _)| *at != selected) {
            let file = files.get(selected).cloned();
            shown = Some((
                selected,
                file.as_ref()
                    .map(|f| diff_lines(f, false))
                    .unwrap_or_default(),
            ));
            if let Some(file) = file {
                let (done, wake, at, made_from) =
                    (coloured_in.clone(), wake.clone(), selected, version);
                std::thread::spawn(move || {
                    let _ = done.send((at, made_from, diff_lines(&file, true)));
                    let _ = wake.send(Event::Wake);
                });
            }
        }
        let diff = shown.as_ref().map_or(&[][..], |(_, lines)| &lines[..]);
        scroll = scroll.min(diff.len().saturating_sub(body / 2));
        let tree = tree(&files);
        let view = View {
            selected,
            scroll,
            focus,
            tree_shown,
            rows,
            width,
        };
        draw(&title, &files, &tree, diff, view);
        // A file picked in the list shows its diff from the top.
        let pick = |to: usize, selected: &mut usize, scroll: &mut usize| {
            if to < files.len() && to != *selected {
                *selected = to;
                *scroll = 0;
            }
        };
        let event = events.recv_timeout(WATCH);
        // gg: a first g waits for the second.
        let after_g = std::mem::take(&mut pending_g);
        match event {
            Ok(Event::Key(b'q')) => break,
            Ok(Event::Key(b'g')) if !after_g => pending_g = true,
            // gg / G: the first or last file in the tree, the top or end of
            // the diff.
            Ok(Event::Key(b'g')) if focus == Focus::Files => pick(0, &mut selected, &mut scroll),
            Ok(Event::Key(b'g')) => scroll = 0,
            Ok(Event::Key(b'G')) if focus == Focus::Files => {
                pick(files.len().saturating_sub(1), &mut selected, &mut scroll)
            }
            Ok(Event::Key(b'G')) => scroll = usize::MAX,
            // e: the file in the editor, at the first line on screen.
            Ok(Event::Key(b'e')) => {
                if let (Some(dir), Some(file)) =
                    (source.strip_prefix("checkout:"), files.get(selected))
                {
                    let line = diff[scroll..]
                        .iter()
                        .find(|r| r.row.kind != Kind::Hunk)
                        // The working tree's number; a removed line has none.
                        .map_or(1, |r| {
                            if r.row.new > 0 {
                                r.row.new
                            } else {
                                r.row.old.max(1)
                            }
                        });
                    edit(herdr, dir, &file.path, line);
                }
            }
            // z hides the file tree, or shows it again with the focus in it.
            Ok(Event::Key(b'z')) => {
                tree_shown = !tree_shown;
                focus = if tree_shown { Focus::Files } else { Focus::Diff };
            }
            Ok(Event::Key(b'\t')) if tree_shown => {
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
            // Vim's scrolling, wherever the focus: Ctrl-d/u (or d/u) half a
            // screen, Ctrl-f/b (or space) a screen, Ctrl-e/y a line; ]/[
            // the next or previous hunk.
            Ok(Event::Key(4 | b'd')) => scroll += body / 2,
            Ok(Event::Key(21 | b'u')) => scroll = scroll.saturating_sub(body / 2),
            Ok(Event::Key(6 | b' ')) => scroll += body,
            Ok(Event::Key(2)) => scroll = scroll.saturating_sub(body),
            Ok(Event::Key(5)) => scroll += 1,
            Ok(Event::Key(25)) => scroll = scroll.saturating_sub(1),
            Ok(Event::Key(b']')) => {
                let next = (scroll + 1..diff.len()).find(|&i| diff[i].row.kind == Kind::Hunk);
                scroll = next.unwrap_or(scroll);
            }
            Ok(Event::Key(b'[')) => {
                let prev = (0..scroll).rev().find(|&i| diff[i].row.kind == Kind::Hunk);
                scroll = prev.unwrap_or(0);
            }
            // f / F: the next or previous file, whichever box has focus.
            Ok(Event::Key(b'f')) => pick(selected + 1, &mut selected, &mut scroll),
            Ok(Event::Key(b'F')) => pick(selected.saturating_sub(1), &mut selected, &mut scroll),
            Ok(Event::Scroll(by)) => scroll = scroll.saturating_add_signed(by),
            // A click picks a file in the tree, or focuses the diff.
            Ok(Event::Press { x, y }) => {
                if x > diff_width(width, tree_shown) {
                    focus = Focus::Files;
                    let at = (y + list_offset(&tree, selected, body)).checked_sub(3);
                    if let Some(Some(i)) = at.and_then(|row| tree.get(row)).map(|t| t.1) {
                        pick(i, &mut selected, &mut scroll);
                    }
                } else {
                    focus = Focus::Diff;
                }
            }
            // A checkout is read again when its files change: file-system
            // events, or without them an mtime fingerprint each tick; and
            // every few quiet ticks regardless.
            Ok(Event::Wake) | Err(_) if source.starts_with("checkout:") => {
                let dir = source.trim_start_matches("checkout:");
                let changed = match &watch {
                    Some(watch) => watch.take(dir),
                    None => {
                        let print = fingerprint(dir, &files);
                        std::mem::replace(&mut last_print, print) != print
                    }
                };
                quiet += u32::from(event.is_err());
                if !changed && quiet < RESCAN_TICKS {
                    continue;
                }
                quiet = 0;
                let fresh = read(&source);
                if fresh != files {
                    files = fresh;
                    shown = None;
                    version += 1;
                }
                selected = selected.min(files.len().saturating_sub(1));
            }
            Ok(_) | Err(_) => {}
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

/// A cheap summary of a checkout's state: the modification times of its
/// index and of the files in its diff. Unchanged, the diff is too (bar new
/// untracked files).
fn fingerprint(dir: &str, files: &[File]) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hash = DefaultHasher::new();
    let modified = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
    modified(&Path::new(dir).join(".git/index")).hash(&mut hash);
    for file in files {
        file.path.hash(&mut hash);
        modified(&Path::new(dir).join(&file.path)).hash(&mut hash);
    }
    hash.finish()
}

/// The unified diff `source` names, split into files.
/// In path order, as the tree shows them, so keys move through the tree.
fn read(source: &str) -> Vec<File> {
    let mut files = if let Some(dir) = source.strip_prefix("checkout:") {
        parse(&checkout_diff(dir))
    } else if let Some(url) = source.strip_prefix("pr:") {
        crate::gh(&["pr", "diff", url, "--color", "never"], None)
            .map_or_else(Vec::new, |o| parse(&String::from_utf8_lossy(&o.stdout)))
    } else {
        Vec::new()
    };
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

/// The checkout's uncommitted changes against `HEAD`, new untracked files
/// included, as one unified diff.
fn checkout_diff(dir: &str) -> String {
    // Paths as they are, not C-quoted, so non-ASCII names parse.
    let git = |args: &[&str]| {
        Command::new("git")
            .args(["-c", "core.quotePath=false", "-C", dir])
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    let mut diff = git(&["diff", "HEAD", "--no-color", "--no-ext-diff"]);
    let untracked = git(&["ls-files", "-z", "--others", "--exclude-standard"]);
    for file in untracked.split('\0').filter(|f| !f.is_empty()) {
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
            let path = rest.rsplit(" b/").next().unwrap_or(rest);
            let path = clean(&unquote(path.trim_start_matches("\"b/")));
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
            } else if let Some(path) = line
                .strip_prefix("+++ b/")
                .or_else(|| line.strip_prefix("+++ \"b/"))
            {
                file.path = clean(&unquote(path));
            } else if let Some(path) = line
                .strip_prefix("--- a/")
                .or_else(|| line.strip_prefix("--- \"a/"))
                && file.status == 'D'
            {
                file.path = clean(&unquote(path));
            }
            continue;
        }
        // Tabs stay: TSV columns are split on them; drawing expands them.
        let text = clean(line.get(1..).unwrap_or(""));
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

/// `text` safe to draw: control characters (bar tab) become `�`, so a file
/// cannot send escape sequences to the terminal or break the layout.
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() && c != '\t' {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

/// A path git C-quoted (the rest of `"…"`, opening quote already gone):
/// octal byte escapes and `\"`, `\\`, `\t`, `\n` undone. Others as is.
fn unquote(path: &str) -> String {
    let Some(inner) = path.strip_suffix('"') else {
        return path.to_string();
    };
    let mut bytes = Vec::new();
    let mut chars = inner.bytes().peekable();
    while let Some(b) = chars.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match chars.next() {
            Some(d @ b'0'..=b'7') => {
                let mut n = u32::from(d - b'0');
                for _ in 0..2 {
                    if let Some(&d @ b'0'..=b'7') = chars.peek() {
                        n = n * 8 + u32::from(d - b'0');
                        chars.next();
                    }
                }
                bytes.push(n as u8);
            }
            Some(b't') => bytes.push(b'\t'),
            Some(b'n') => bytes.push(b'\n'),
            Some(other) => bytes.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The first line number in a hunk header's `-12,3` or `+12,3`.
fn start(range: Option<&str>) -> usize {
    range
        .and_then(|r| r.get(1..)?.split(',').next()?.parse().ok())
        .unwrap_or(0)
}

/// Opens `path` in `dir` at `line` in the user's editor, zoomed over this
/// pane; quitting the editor comes back here, the diff already re-read.
fn edit(herdr: &str, dir: &str, path: &str, line: usize) {
    let Ok(me) = std::env::var("HERDR_PANE_ID") else {
        return;
    };
    let _ = Command::new(herdr)
        .args(["plugin", "pane", "open", "--plugin", "baleen37.herdr-sim"])
        .args(["--entrypoint", "edit", "--placement", "zoomed", "--focus"])
        .args(["--target-pane", &me, "--cwd", dir])
        .args(["--env", &format!("HERDR_SIM_EDIT_FILE={path}")])
        .args(["--env", &format!("HERDR_SIM_EDIT_LINE={line}")])
        .output();
}

/// The `edit` pane: the file in the user's editor at the line. The login
/// shell gives PATH, VISUAL and EDITOR as the user set them, whatever the
/// shell is; the editor itself runs without a shell, so no quoting matters.
pub fn edit_pane() -> anyhow::Result<()> {
    let file = std::env::var("HERDR_SIM_EDIT_FILE")?;
    let line = std::env::var("HERDR_SIM_EDIT_LINE").unwrap_or_else(|_| "1".into());
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let login = Command::new(&shell)
        .args(["-l", "-c", "env"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let var = |name: &str| {
        login
            .lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix('=').map(String::from))
            .or_else(|| std::env::var(name).ok())
            .filter(|v| !v.is_empty())
    };
    let editor = var("VISUAL")
        .or_else(|| var("EDITOR"))
        .unwrap_or_else(|| "vi".into());
    let mut words = editor.split_whitespace();
    let program = words.next().unwrap_or("vi");
    let mut command = Command::new(program);
    command.args(words).arg(format!("+{line}")).arg(&file);
    if let Some(path) = var("PATH") {
        command.env("PATH", path);
    }
    command.status()?;
    Ok(())
}

/// The diff box's width; the file tree, if shown, takes the rest.
fn diff_width(width: usize, tree_shown: bool) -> usize {
    if !tree_shown {
        return width;
    }
    width.saturating_sub((width * 2 / 5).clamp(24, 36))
}

/// A diff line as drawn: the row, and its text split into coloured runs.
struct Shown {
    row: Row,
    spans: Vec<(String, String)>,
}

/// The syntaxes and colour theme highlighting uses, loaded once.
fn highlighting() -> &'static (SyntaxSet, Theme) {
    static LOADED: OnceLock<(SyntaxSet, Theme)> = OnceLock::new();
    LOADED.get_or_init(|| {
        let themes = ThemeSet::load_defaults();
        let theme = themes.themes["base16-ocean.dark"].clone();
        (SyntaxSet::load_defaults_newlines(), theme)
    })
}

/// A file's diff as reviewr shows it: unchanged runs summarised, each line
/// with its number, its text highlighted for the file's language when
/// `colour`.
fn diff_lines(file: &File, colour: bool) -> Vec<Shown> {
    // CSV and TSV: each column its own colour, as Rainbow CSV does.
    let delimiter = match Path::new(&file.path).extension().and_then(|e| e.to_str()) {
        Some(e) if e.eq_ignore_ascii_case("csv") => Some(','),
        Some(e) if e.eq_ignore_ascii_case("tsv") => Some('\t'),
        _ => None,
    };
    let mut colours = (colour && delimiter.is_none()).then(|| {
        let (syntaxes, theme) = highlighting();
        let syntax = Path::new(&file.path)
            .extension()
            .and_then(|e| syntaxes.find_syntax_by_extension(&e.to_string_lossy()))
            .or_else(|| {
                let name = file.path.rsplit('/').next().unwrap_or(&file.path);
                syntaxes.find_syntax_by_extension(name)
            })
            .unwrap_or_else(|| syntaxes.find_syntax_plain_text());
        (HighlightLines::new(syntax, theme), syntaxes)
    });
    let mut out = Vec::new();
    // Where the new file's lines stand after the last hunk shown.
    let mut next = 1;
    for row in &file.rows {
        if row.kind == Kind::Hunk {
            let skipped = row.new.saturating_sub(next);
            if skipped > 0 {
                let row = Row {
                    kind: Kind::Hunk,
                    old: 0,
                    new: skipped,
                    text: String::new(),
                };
                out.push(Shown {
                    row,
                    spans: Vec::new(),
                });
            }
            continue;
        }
        if row.kind != Kind::Removed {
            next = row.new + 1;
        }
        // Old and new lines run through one highlighter in diff order: close
        // enough for a diff, and what a reader sees.
        let plain = || vec![(String::new(), row.text.clone())];
        if let Some(delimiter) = delimiter.filter(|_| colour) {
            out.push(Shown {
                row: row.clone(),
                spans: columns(&row.text, delimiter),
            });
            continue;
        }
        let Some((colours, syntaxes)) = colours.as_mut() else {
            out.push(Shown {
                row: row.clone(),
                spans: plain(),
            });
            continue;
        };
        let line = format!("{}\n", row.text);
        let spans = colours
            .highlight_line(&line, syntaxes)
            .map(|runs| {
                runs.into_iter()
                    .map(|(style, text)| {
                        let c = style.foreground;
                        (
                            format!("\x1b[38;2;{};{};{}m", c.r, c.g, c.b),
                            text.trim_end_matches('\n').to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_else(|_| plain());
        out.push(Shown {
            row: Row {
                kind: row.kind,
                old: row.old,
                new: row.new,
                text: row.text.clone(),
            },
            spans,
        });
    }
    out
}

/// Columns a tab takes when drawn.
const TAB_COLS: usize = 4;

/// Colours for CSV columns, in turn.
const COLUMN_COLOURS: [&str; 6] = [
    "\x1b[38;2;137;180;250m",
    "\x1b[38;2;166;227;161m",
    "\x1b[38;2;249;226;175m",
    "\x1b[38;2;203;166;247m",
    "\x1b[38;2;137;220;235m",
    "\x1b[38;2;250;179;135m",
];

/// A CSV line as coloured runs, a colour per column, the delimiters dim.
/// Quoted fields keep their delimiters.
fn columns(line: &str, delimiter: char) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut column = 0;
    for c in line.chars() {
        if c == '"' {
            quoted = !quoted;
        }
        if c == delimiter && !quoted {
            let colour = COLUMN_COLOURS[column % COLUMN_COLOURS.len()];
            out.push((colour.to_string(), std::mem::take(&mut field)));
            out.push((DIM.to_string(), c.to_string()));
            column += 1;
        } else {
            field.push(c);
        }
    }
    let colour = COLUMN_COLOURS[column % COLUMN_COLOURS.len()];
    out.push((colour.to_string(), field));
    out
}

/// Coloured runs cut to `width` columns, each run's colour on `bg`; and the
/// columns used.
fn paint(spans: &[(String, String)], width: usize, bg: &str) -> (String, usize) {
    let mut out = String::new();
    let mut used = 0;
    for (colour, text) in spans {
        out += &format!("{colour}{bg}");
        for c in text.chars() {
            let (c, w) = match c {
                '\t' => (' ', TAB_COLS),
                c if c.is_ascii() => (c, 1),
                c => (c, cols(&c.to_string())),
            };
            if used + w > width {
                out += "…";
                return (out + RESET, width);
            }
            if c == ' ' && w > 1 {
                out += &" ".repeat(w);
            } else {
                out.push(c);
            }
            used += w;
        }
    }
    (out + RESET, used)
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

/// What the pane draws, beside the files themselves.
#[derive(Clone, Copy)]
struct View {
    selected: usize,
    scroll: usize,
    focus: Focus,
    tree_shown: bool,
    rows: usize,
    width: usize,
}

/// Draws the header, the diff and file boxes, and the key hints.
fn draw(title: &str, files: &[File], tree: &[(String, Option<usize>)], diff: &[Shown], view: View) {
    let View {
        selected,
        scroll,
        focus,
        tree_shown,
        rows,
        width,
    } = view;
    let dw = diff_width(width, tree_shown);
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
            + &if tree_shown {
                edge('┌', "Files", '┐', fw, focus == Focus::Files)
            } else {
                String::new()
            },
    );
    let digits = diff
        .iter()
        .map(|r| r.row.old.max(r.row.new))
        .max()
        .unwrap_or(0)
        .to_string()
        .len()
        .max(3);
    let offset = list_offset(tree, selected, body);
    for i in 0..body {
        let left = match diff.get(scroll + i) {
            Some(r) => diff_row(r, digits, dw.saturating_sub(2)),
            None if i == 0 && files.is_empty() => format!("{DIM}no changes{RESET}"),
            None => String::new(),
        };
        let right = match tree.get(offset + i).filter(|_| tree_shown) {
            Some((dir, None)) => format!("{DIM}▾ {}{RESET}", cut(dir, fw.saturating_sub(4))),
            Some((_, Some(f))) => tree_row(&files[*f], *f == selected, fw.saturating_sub(2)),
            _ => String::new(),
        };
        lines.push(format!(
            "{DIM}│{RESET}{left}\x1b[{row};{dw}H{DIM}│{RESET}{}",
            if tree_shown {
                format!("{DIM}│{RESET}{right}\x1b[{};{width}H{DIM}│{RESET}", i + 3)
            } else {
                String::new()
            },
            row = i + 3,
        ));
    }
    let bottom = if tree_shown {
        edge('└', "", '┘', fw, false)
    } else {
        String::new()
    };
    lines.push(edge('└', "", '┘', dw, false) + &bottom);
    let hints = if focus == Focus::Files {
        "j/k file · gg/G · ^d/^u scroll · e edit · z hide · tab diff · q"
    } else {
        "j/k · ^d/^u · gg/G · ]/[ hunk · f/F file · e edit · z · tab · q"
    };
    lines.push(format!(" {DIM}{hints}{RESET}"));

    let mut out = String::from("\x1b[?7l");
    for (i, line) in lines.iter().enumerate() {
        out += &format!("\x1b[{};1H\x1b[K{line}", i + 1);
    }
    let mut stdout = io::stdout();
    // One synchronized update (DEC mode 2026): terminals that know it show
    // the frame whole, never half drawn.
    let _ = stdout.write_all(format!("\x1b[?2026h{out}\x1b[?2026l").as_bytes());
    let _ = stdout.flush();
}

/// One diff line, `width` columns wide: changed lines get a bar and a tint,
/// and every line its number.
fn diff_row(shown: &Shown, digits: usize, width: usize) -> String {
    let row = &shown.row;
    let text_cols = width.saturating_sub(digits + 3);
    match row.kind {
        Kind::Hunk => format!(
            "  {DIM}⋯  {} unmodified line{}{RESET}",
            row.new,
            if row.new == 1 { "" } else { "s" }
        ),
        Kind::Context => {
            let (text, _) = paint(&shown.spans, text_cols, "");
            format!(" {DIM}{:>digits$}{RESET}  {text}", row.new)
        }
        Kind::Added | Kind::Removed => {
            let (bar, bg, number) = if row.kind == Kind::Added {
                (GREEN, ADDED_BG, row.new)
            } else {
                (RED, REMOVED_BG, row.old)
            };
            let (text, used) = paint(&shown.spans, text_cols, bg);
            let pad = " ".repeat(text_cols.saturating_sub(used));
            format!("{bar}▌{RESET}{bg}{DIM}{number:>digits$}{RESET}{bg}  {text}{bg}{pad}{RESET}")
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
    fn quoted_paths_and_control_characters() {
        let diff = "diff --git \"a/caf\\303\\251.txt\" \"b/caf\\303\\251.txt\"\n\
                    --- \"a/caf\\303\\251.txt\"\n+++ \"b/caf\\303\\251.txt\"\n\
                    @@ -1 +1 @@\n-a\n+b\u{1b}]52;c;x\u{7}\n";
        let files = parse(diff);
        assert_eq!(files[0].path, "café.txt");
        assert_eq!(files[0].rows[2].text, "b\u{fffd}]52;c;x\u{fffd}");
    }

    #[test]
    fn tsv_keeps_its_tabs_to_split_columns() {
        let diff =
            "diff --git a/t.tsv b/t.tsv\n--- a/t.tsv\n+++ b/t.tsv\n@@ -1 +1 @@\n-a\tb\n+a\tc\n";
        let lines = diff_lines(&parse(diff)[0], true);
        let texts: Vec<&str> = lines[1].spans.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(texts, ["a", "\t", "c"]);
        assert_eq!(paint(&lines[1].spans, 20, "").1, 2 + TAB_COLS);
    }

    #[test]
    fn columns_colour_each_field_and_keep_quoted_commas() {
        let runs = columns("a,\"b,c\",d", ',');
        let texts: Vec<&str> = runs.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(texts, ["a", ",", "\"b,c\"", ",", "d"]);
        assert_ne!(runs[0].0, runs[2].0);
    }

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
