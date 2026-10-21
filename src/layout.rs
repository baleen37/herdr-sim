//! The Agents list's shape, after herdr-radar: one group per Herdr workspace,
//! a linked git worktree hanging under the checkout it was cut from, and the
//! order by when each session last worked. Pure, so the whole layout is tested
//! without a server.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

/// Herdr trims leading whitespace off token values; a zero-width space is not
/// whitespace, so it keeps an indent (and a blank gap row) alive.
pub const ZWSP: &str = "\u{200b}";

/// Ahead of a worktree's name in its header: Nerd Font's git-branch glyph.
const WORKTREE_MARK: &str = "\u{f418} ";

/// The indent `depth` levels deep, two columns a level, as herdr-radar.
pub fn indent(depth: usize) -> String {
    if depth == 0 {
        String::new()
    } else {
        format!("{ZWSP}{}", "  ".repeat(depth))
    }
}

/// One agent pane, in Herdr's list order.
#[derive(Clone, Debug, Default)]
pub struct Pane {
    pub workspace: String,
    pub tab: String,
    /// The minute the session last worked; `None` sorts last.
    pub last_minute: Option<u64>,
    /// Idle over two hours.
    pub stale: bool,
    /// How much the session needs you: 3 a question, 2 a finished turn not
    /// yet seen, 1 at work, 0 idle.
    pub urgency: u8,
}

/// [`Pane::urgency`] for a session's shown status.
pub fn urgency(status: &str) -> u8 {
    match status {
        "blocked" => 3,
        "done" => 2,
        "working" => 1,
        _ => 0,
    }
}

/// One Herdr workspace.
#[derive(Clone, Debug, Default)]
pub struct Space {
    pub id: String,
    pub label: String,
    /// Herdr's `worktree.repo_key`: workspaces sharing it are one repository.
    pub repo_key: Option<String>,
    /// Herdr's `worktree.is_linked_worktree`.
    pub linked: bool,
    /// Herdr's `worktree.repo_name`.
    pub repo_name: Option<String>,
}

/// What one pane shows of the tree. Only a workspace's first pane carries a
/// header; only its last the gap.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Row {
    /// The workspace's header: its label, behind the worktree's corner.
    pub group: Option<String>,
    /// Every session in the workspace has gone stale: fade the header.
    pub group_stale: bool,
    /// The repository's name on a row of its own, over a worktree whose
    /// checkout has no session on the list.
    pub group_parent: Option<String>,
    /// A blank row after the group.
    pub gap: bool,
    /// How far the session's own row sits in, on top of Herdr's: Herdr indents
    /// an entry's continuation rows, so a head's row arrives one level in and
    /// the others need one of ours; a worktree's sessions one level further.
    pub depth: usize,
}

/// The panes in display order, as indices into `panes`, and each one's row in
/// that order. `ordered` is the plugin's own activity order; without it the
/// list keeps Herdr's order and a worktree not right under its family is drawn
/// without a corner.
pub fn layout(panes: &[Pane], spaces: &[Space], ordered: bool) -> (Vec<usize>, Vec<Row>) {
    let present: HashSet<&str> = panes.iter().map(|p| p.workspace.as_str()).collect();
    let space = |id: &str| spaces.iter().find(|s| s.id == id);

    // Linked worktrees hang under the repository's main checkout, but only
    // when both are on the list; one whose checkout is absent is an orphan
    // and gets the repository's name on a row of its own.
    let mut parent_of: HashMap<&str, &str> = HashMap::new();
    let mut orphan: HashMap<&str, String> = HashMap::new();
    for s in spaces
        .iter()
        .filter(|s| s.linked && present.contains(s.id.as_str()))
    {
        let main = spaces
            .iter()
            .find(|m| !m.linked && m.repo_key.is_some() && m.repo_key == s.repo_key);
        match main {
            Some(m) if present.contains(m.id.as_str()) => {
                parent_of.insert(&s.id, &m.id);
            }
            _ => {
                if let Some(name) = &s.repo_name {
                    orphan.insert(&s.id, name.clone());
                }
            }
        }
    }

    let order = if ordered {
        activity_order(panes, &parent_of)
    } else {
        (0..panes.len()).collect()
    };

    // Workspaces in display order.
    let mut seen = HashSet::new();
    let shown: Vec<&str> = order
        .iter()
        .map(|&i| panes[i].workspace.as_str())
        .filter(|ws| seen.insert(*ws))
        .collect();
    // In Herdr's order a worktree can land away from its family; a corner
    // there would hang it off whatever sits above, so it goes without one.
    // A detached sibling leaves the family, so it cannot vouch for the next
    // one: a run of strays stays a run of strays.
    let mut detached = HashSet::new();
    let mut previous: Option<&str> = None;
    for &ws in &shown {
        if let Some(&parent) = parent_of.get(ws) {
            let next_to_family = previous == Some(parent)
                || previous.and_then(|p| parent_of.get(p)) == Some(&parent);
            if !next_to_family {
                parent_of.remove(ws);
                detached.insert(ws);
            }
        }
        previous = Some(ws);
    }
    let nested = |ws: &str| parent_of.contains_key(ws) || orphan.contains_key(ws);
    let last_child: HashMap<&str, &str> = shown
        .iter()
        .filter_map(|&ws| parent_of.get(ws).map(|&p| (p, ws)))
        .collect();

    let head: HashSet<usize> = {
        let mut seen = HashSet::new();
        order
            .iter()
            .copied()
            .filter(|&i| seen.insert(&panes[i].workspace))
            .collect()
    };
    let tail: HashSet<usize> = {
        let mut last = HashMap::new();
        for &i in &order {
            last.insert(&panes[i].workspace, i);
        }
        last.into_values().collect()
    };

    let rows = order
        .iter()
        .enumerate()
        .map(|(pos, &i)| {
            let ws = panes[i].workspace.as_str();
            let is_head = head.contains(&i);
            let label = space(ws).map_or(ws, |s| s.label.as_str());
            let corner = if let Some(&parent) = parent_of.get(ws) {
                let last = last_child.get(parent) == Some(&ws);
                format!(
                    "{}{} {WORKTREE_MARK}",
                    indent(1),
                    if last { "└─" } else { "├─" }
                )
            } else if orphan.contains_key(ws) {
                // Under its own parent row Herdr indents this row already.
                format!("└─ {WORKTREE_MARK}")
            } else if detached.contains(ws) {
                WORKTREE_MARK.to_string()
            } else {
                String::new()
            };
            // The family stays one block: no gap before its next member.
            let next_ws = order[pos + 1..]
                .iter()
                .map(|&j| panes[j].workspace.as_str())
                .find(|w| *w != ws);
            let gap = tail.contains(&i)
                && next_ws.is_none_or(|n| family_of(&parent_of, n) != family_of(&parent_of, ws));
            Row {
                group: is_head.then(|| format!("{corner}{label}")),
                group_stale: is_head && panes.iter().filter(|p| p.workspace == ws).all(|p| p.stale),
                group_parent: is_head.then(|| orphan.get(ws).cloned()).flatten(),
                gap,
                depth: usize::from(!is_head) + usize::from(nested(ws)),
            }
        })
        .collect();
    (order, rows)
}

/// The worktree family a workspace belongs to: its main checkout's id, or its
/// own.
fn family_of<'a>(parent_of: &HashMap<&str, &'a str>, ws: &'a str) -> &'a str {
    parent_of.get(ws).copied().unwrap_or(ws)
}

/// Indices of `panes` in herdr-radar's activity order, what needs you first:
/// a worktree family by its most urgent session, then its newest, its main
/// checkout first; then each workspace the same way; then each tab as one
/// unit, the pane it was split from first; then each pane. Never-worked panes sort last at every level; ties keep
/// Herdr's order.
fn activity_order(panes: &[Pane], parent_of: &HashMap<&str, &str>) -> Vec<usize> {
    // Each family, workspace and tab: its most urgent session's urgency and
    // newest minute, and where it first shows in Herdr's list, which keeps it
    // together and breaks ties.
    type Rank = (u8, Option<u64>);
    let mut family_rank: HashMap<&str, (Rank, usize)> = HashMap::new();
    let mut ws_rank: HashMap<&str, (Rank, usize)> = HashMap::new();
    let mut tab_rank: HashMap<&str, (Rank, usize)> = HashMap::new();
    for (i, p) in panes.iter().enumerate() {
        for (map, name) in [
            (&mut family_rank, family_of(parent_of, &p.workspace)),
            (&mut ws_rank, p.workspace.as_str()),
            (&mut tab_rank, p.tab.as_str()),
        ] {
            let (rank, _) = map.entry(name).or_insert(((0, None), i));
            *rank = (*rank).max((p.urgency, p.last_minute));
        }
    }
    let mut order: Vec<usize> = (0..panes.len()).collect();
    // `Reverse` puts the most urgent, then the newest, first and `None`, the
    // lowest, last.
    order.sort_by_key(|&i| {
        let p = &panes[i];
        let family = family_of(parent_of, &p.workspace);
        let (family_rank, family_seen) = family_rank[family];
        let (ws_rank, ws_seen) = ws_rank[p.workspace.as_str()];
        let (tab_rank, tab_seen) = tab_rank[p.tab.as_str()];
        (
            Reverse(family_rank),
            family_seen,
            family != p.workspace,
            Reverse(ws_rank),
            ws_seen,
            Reverse(tab_rank),
            tab_seen,
            tab_seen != i,
            Reverse((p.urgency, p.last_minute)),
        )
    });
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(ws: &str, tab: &str, last: Option<u64>) -> Pane {
        Pane {
            workspace: ws.into(),
            tab: tab.into(),
            last_minute: last,
            stale: false,
            urgency: 0,
        }
    }

    fn space(id: &str, label: &str) -> Space {
        Space {
            id: id.into(),
            label: label.into(),
            ..Default::default()
        }
    }

    fn worktree(id: &str, label: &str, repo: &str, linked: bool) -> Space {
        Space {
            id: id.into(),
            label: label.into(),
            repo_key: Some(format!("/{repo}/.git")),
            linked,
            repo_name: Some(repo.into()),
        }
    }

    #[test]
    fn groups_are_workspaces_by_latest_activity_with_a_gap_after_each() {
        let panes = [
            pane("w1", "t1", Some(5)),
            pane("w2", "t2", Some(9)),
            pane("w1", "t3", Some(7)),
        ];
        let spaces = [space("w1", "api"), space("w2", "web")];
        let (order, rows) = layout(&panes, &spaces, true);
        assert_eq!(order, [1, 2, 0]);
        assert_eq!(
            rows,
            [
                Row {
                    group: Some("web".into()),
                    gap: true,
                    ..Default::default()
                },
                Row {
                    group: Some("api".into()),
                    ..Default::default()
                },
                Row {
                    gap: true,
                    depth: 1,
                    ..Default::default()
                },
            ]
        );
    }

    #[test]
    fn worktrees_hang_under_their_checkout_as_one_block() {
        let panes = [
            pane("main", "t1", Some(1)),
            pane("wt-a", "t2", Some(9)),
            pane("other", "t3", Some(5)),
            pane("wt-b", "t4", Some(3)),
            pane("wt-b", "t5", Some(2)),
        ];
        let spaces = [
            worktree("main", "search", "search", false),
            worktree("wt-a", "fix-a", "search", true),
            worktree("wt-b", "fix-b", "search", true),
            space("other", "dotfiles"),
        ];
        let (order, rows) = layout(&panes, &spaces, true);
        // The family ranks by its newest (wt-a, 9), checkout first.
        assert_eq!(order, [0, 1, 3, 4, 2]);
        let groups: Vec<_> = rows.iter().map(|r| r.group.clone()).collect();
        assert_eq!(
            groups,
            [
                Some("search".into()),
                Some("\u{200b}  ├─ \u{f418} fix-a".into()),
                Some("\u{200b}  └─ \u{f418} fix-b".into()),
                None,
                Some("dotfiles".into()),
            ]
        );
        let gaps: Vec<_> = rows.iter().map(|r| r.gap).collect();
        assert_eq!(gaps, [false, false, false, true, true]);
        let depths: Vec<_> = rows.iter().map(|r| r.depth).collect();
        assert_eq!(depths, [0, 1, 1, 2, 0]);
    }

    #[test]
    fn a_worktree_without_its_checkout_gets_the_repo_on_a_row_of_its_own() {
        let panes = [pane("wt", "t1", Some(1))];
        let spaces = [
            worktree("main", "search", "search", false),
            worktree("wt", "fix-a", "search", true),
        ];
        let (_, rows) = layout(&panes, &spaces, true);
        assert_eq!(
            rows,
            [Row {
                group: Some("└─ \u{f418} fix-a".into()),
                group_parent: Some("search".into()),
                gap: true,
                depth: 1,
                ..Default::default()
            }]
        );
    }

    #[test]
    fn in_herdrs_order_a_worktree_away_from_its_family_has_no_corner() {
        let panes = [
            pane("main", "t1", None),
            pane("other", "t2", None),
            pane("wt", "t3", None),
        ];
        let spaces = [
            worktree("main", "search", "search", false),
            worktree("wt", "fix-a", "search", true),
            space("other", "dotfiles"),
        ];
        let (order, rows) = layout(&panes, &spaces, false);
        assert_eq!(order, [0, 1, 2]);
        assert_eq!(rows[2].group.as_deref(), Some("\u{f418} fix-a"));
        assert_eq!(rows[2].depth, 0);
        assert!(rows[0].gap);
    }

    #[test]
    fn a_split_tab_ranks_as_one_unit_and_never_worked_panes_sort_last() {
        let panes = [
            pane("w", "t1", Some(9)),
            pane("w", "t2", Some(5)),
            pane("w", "t1", Some(1)),
            pane("w", "t3", None),
        ];
        let (order, _) = layout(&panes, &[space("w", "api")], true);
        assert_eq!(order, [0, 2, 1, 3]);
    }

    #[test]
    fn ties_keep_herdrs_order_and_each_group_together() {
        let panes = [
            pane("wa", "t1", Some(5)),
            pane("wb", "t2", Some(5)),
            pane("wa", "t3", None),
            pane("wb", "t4", None),
        ];
        let (order, _) = layout(&panes, &[space("wa", "a"), space("wb", "b")], true);
        assert_eq!(order, [0, 2, 1, 3]);
    }

    #[test]
    fn a_run_of_stray_worktrees_all_lose_the_corner() {
        let panes = [
            pane("main", "t1", None),
            pane("other", "t2", None),
            pane("wt1", "t3", None),
            pane("wt2", "t4", None),
        ];
        let spaces = [
            worktree("main", "search", "search", false),
            worktree("wt1", "fix-a", "search", true),
            worktree("wt2", "fix-b", "search", true),
            space("other", "dotfiles"),
        ];
        let (_, rows) = layout(&panes, &spaces, false);
        assert_eq!(rows[3].group.as_deref(), Some("\u{f418} fix-b"));
    }

    #[test]
    fn urgency_ranks_a_question_over_a_finished_turn_over_work() {
        let ranks = ["blocked", "done", "working", "idle", "unknown"].map(urgency);
        assert_eq!(ranks, [3, 2, 1, 0, 0]);
    }

    #[test]
    fn what_needs_you_comes_first_then_the_newest() {
        let at = |ws: &str, tab: &str, last: u64, urgency: u8| Pane {
            urgency,
            ..pane(ws, tab, Some(last))
        };
        let panes = [
            at("idle", "t1", 9, 0),
            at("asks", "t2", 1, 3),
            at("done", "t3", 5, 2),
            at("works", "t4", 7, 1),
            at("asks", "t5", 8, 0),
        ];
        let spaces = ["idle", "asks", "done", "works"].map(|w| space(w, w));
        let (order, _) = layout(&panes, &spaces, true);
        // Groups by their most urgent session; within one, the same.
        assert_eq!(order, [1, 4, 2, 3, 0]);
    }

    #[test]
    fn a_group_fades_only_when_every_session_in_it_is_stale() {
        let mut panes = [
            pane("w1", "t1", Some(2)),
            pane("w1", "t2", Some(1)),
            pane("w2", "t3", Some(0)),
        ];
        panes[0].stale = true;
        panes[2].stale = true;
        let (_, rows) = layout(&panes, &[space("w1", "api"), space("w2", "web")], true);
        let stale: Vec<_> = rows.iter().map(|r| r.group_stale).collect();
        assert_eq!(stale, [false, false, true]);
    }
}
