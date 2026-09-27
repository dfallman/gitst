//! The Activity timeline: reflog entries plus events derived by comparing
//! consecutive snapshots.

use std::collections::{HashMap, HashSet};

use crate::model::{Change, ReflogEntry, Snapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityKind {
    Commit,
    Amend,
    Checkout,
    Merge,
    Rebase,
    Reset,
    Pull,
    CherryPick,
    Push,
    Fetch,
    Files,
    Stage,
    Stash,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityEvent {
    /// Unix seconds.
    pub time: i64,
    pub kind: ActivityKind,
    pub text: String,
    /// Commit the event refers to, when clicking it can open one.
    pub rev: Option<String>,
}

/// Turns a reflog entry into a timeline event, or `None` for noise such as
/// individual rebase picks.
pub fn classify(e: &ReflogEntry) -> Option<ActivityEvent> {
    let event = |kind, text: String| {
        Some(ActivityEvent {
            time: e.time,
            kind,
            text,
            rev: Some(e.short.clone()),
        })
    };
    if let Some(remote) = e.refname.strip_prefix("refs/remotes/") {
        return if e.message == "update by push" {
            event(ActivityKind::Push, format!("pushed {remote} {}", e.short))
        } else if e.message.starts_with("fetch") || e.message.starts_with("pull") {
            event(ActivityKind::Fetch, format!("fetched {remote} {}", e.short))
        } else {
            None
        };
    }

    let (action, rest) = e
        .message
        .split_once(": ")
        .unwrap_or((e.message.as_str(), ""));
    let short = &e.short;
    if action.starts_with("rebase") || action.starts_with("pull --rebase") {
        return if action.contains("(start)") {
            event(ActivityKind::Rebase, "rebase started".into())
        } else if action.contains("(finish)") {
            event(ActivityKind::Rebase, "rebase finished".into())
        } else {
            None
        };
    }
    match action {
        "commit (amend)" => event(ActivityKind::Amend, format!("amend {short} {rest}")),
        "commit (merge)" => event(ActivityKind::Merge, format!("merge {short} {rest}")),
        a if a.starts_with("commit") => {
            event(ActivityKind::Commit, format!("commit {short} {rest}"))
        }
        a if a.starts_with("merge ") => event(ActivityKind::Merge, a.to_string()),
        "checkout" => {
            let target = rest.rsplit_once(" to ").map_or(rest, |(_, t)| t);
            event(ActivityKind::Checkout, format!("checkout {target}"))
        }
        "reset" => event(
            ActivityKind::Reset,
            format!("reset to {}", rest.trim_start_matches("moving to ")),
        ),
        a if a.starts_with("pull") => event(ActivityKind::Pull, format!("pull {short}")),
        "cherry-pick" => event(
            ActivityKind::CherryPick,
            format!("cherry-pick {short} {rest}"),
        ),
        _ => event(ActivityKind::Other, e.message.clone()),
    }
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// Events describing what changed between two snapshots, plus the paths
/// whose status or line counts changed (for the change pulse).
pub fn diff_snapshots(
    prev: &Snapshot,
    next: &Snapshot,
    now: i64,
) -> (Vec<ActivityEvent>, Vec<String>) {
    let before: HashMap<&str, &Change> =
        prev.changes.iter().map(|c| (c.path.as_str(), c)).collect();
    let changed: Vec<(&Change, Option<&&Change>)> = next
        .changes
        .iter()
        .map(|c| (c, before.get(c.path.as_str())))
        .filter(|(c, old)| old.is_none_or(|o| o != c))
        .collect();

    let mut events = Vec::new();
    let event = |kind, text: String| ActivityEvent {
        time: now,
        kind,
        text,
        rev: None,
    };
    if !changed.is_empty() {
        let moved_to_index = |to_index: bool| {
            changed.iter().all(|(c, old)| {
                old.is_some_and(|o| {
                    o.added == c.added
                        && o.removed == c.removed
                        && (o.x == ' ') == to_index
                        && (c.x == ' ') != to_index
                })
            })
        };
        if moved_to_index(true) {
            events.push(event(
                ActivityKind::Stage,
                format!("staged {}", plural(changed.len(), "file")),
            ));
        } else if moved_to_index(false) {
            events.push(event(
                ActivityKind::Stage,
                format!("unstaged {}", plural(changed.len(), "file")),
            ));
        } else if let [(c, _)] = changed[..] {
            let text = match (c.added, c.removed) {
                (Some(a), Some(r)) => format!("{} +{a} −{r}", c.path),
                _ => c.path.clone(),
            };
            events.push(event(ActivityKind::Files, text));
        } else {
            events.push(event(
                ActivityKind::Files,
                format!("{} changed", plural(changed.len(), "file")),
            ));
        }
    }
    if next.stash_count > prev.stash_count {
        events.push(event(ActivityKind::Stash, "stash saved".into()));
    } else if next.stash_count < prev.stash_count {
        events.push(event(
            ActivityKind::Stash,
            "stash applied or dropped".into(),
        ));
    }
    let paths = changed.into_iter().map(|(c, _)| c.path.clone()).collect();
    (events, paths)
}

/// The timeline shown in the Activity section, newest first.
pub fn merged(reflog: &[ReflogEntry], live: &[ActivityEvent], limit: usize) -> Vec<ActivityEvent> {
    let mut all: Vec<ActivityEvent> = reflog
        .iter()
        .filter_map(classify)
        .chain(live.iter().cloned())
        .collect();
    all.sort_by(|a, b| b.time.cmp(&a.time));
    let mut seen = HashSet::new();
    all.retain(|e| seen.insert((e.time, e.text.clone())));
    all.truncate(limit);
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(r: &str, m: &str) -> ReflogEntry {
        ReflogEntry {
            refname: r.into(),
            short: "abc1234".into(),
            time: 10,
            message: m.into(),
        }
    }

    fn ch(path: &str, x: char, y: char, a: u32, r: u32) -> Change {
        Change {
            path: path.into(),
            orig_path: None,
            x,
            y,
            added: Some(a),
            removed: Some(r),
        }
    }

    fn snap_with(changes: Vec<Change>) -> Snapshot {
        Snapshot {
            changes,
            oid: Some("o1".into()),
            ..Snapshot::default()
        }
    }

    #[test]
    fn classify_head_messages() {
        assert_eq!(
            classify(&e("HEAD", "commit: icons")).unwrap().text,
            "commit abc1234 icons"
        );
        assert_eq!(
            classify(&e("HEAD", "commit (initial): root")).unwrap().kind,
            ActivityKind::Commit
        );
        assert_eq!(
            classify(&e("HEAD", "commit (amend): x")).unwrap().kind,
            ActivityKind::Amend
        );
        assert_eq!(
            classify(&e("HEAD", "commit (merge): Merge x"))
                .unwrap()
                .kind,
            ActivityKind::Merge
        );
        assert_eq!(
            classify(&e("HEAD", "merge feat: Fast-forward"))
                .unwrap()
                .text,
            "merge feat"
        );
        assert_eq!(
            classify(&e("HEAD", "checkout: moving from main to feat"))
                .unwrap()
                .text,
            "checkout feat"
        );
        assert_eq!(
            classify(&e("HEAD", "rebase (finish): returning to refs/heads/x"))
                .unwrap()
                .text,
            "rebase finished"
        );
        assert!(classify(&e("HEAD", "rebase (pick): x")).is_none());
        assert_eq!(
            classify(&e("HEAD", "reset: moving to HEAD~1"))
                .unwrap()
                .text,
            "reset to HEAD~1"
        );
        assert_eq!(
            classify(&e("HEAD", "pull: Fast-forward")).unwrap().kind,
            ActivityKind::Pull
        );
        assert_eq!(
            classify(&e("HEAD", "cherry-pick: fix")).unwrap().kind,
            ActivityKind::CherryPick
        );
        assert_eq!(
            classify(&e("HEAD", "weird thing")).unwrap().kind,
            ActivityKind::Other
        );
    }

    #[test]
    fn reflog_events_carry_their_commit() {
        assert_eq!(
            classify(&e("HEAD", "commit: x")).unwrap().rev.as_deref(),
            Some("abc1234")
        );
        assert_eq!(
            classify(&e("refs/remotes/origin/main", "update by push"))
                .unwrap()
                .rev
                .as_deref(),
            Some("abc1234")
        );
    }

    #[test]
    fn classify_remote_messages() {
        assert_eq!(
            classify(&e("refs/remotes/origin/main", "update by push"))
                .unwrap()
                .text,
            "pushed origin/main abc1234"
        );
        let f = classify(&e("refs/remotes/origin/main", "fetch: fast-forward")).unwrap();
        assert_eq!(
            (f.kind, f.text.as_str()),
            (ActivityKind::Fetch, "fetched origin/main abc1234")
        );
        assert!(classify(&e("refs/remotes/origin/main", "branch: Created from HEAD")).is_none());
    }

    #[test]
    fn diff_reports_changed_files() {
        let a = snap_with(vec![ch("a", ' ', 'M', 1, 0)]);
        let b = snap_with(vec![ch("a", ' ', 'M', 3, 0), ch("b", '?', '?', 1, 0)]);
        let (ev, paths) = diff_snapshots(&a, &b, 5);
        assert_eq!(
            ev,
            vec![ActivityEvent {
                time: 5,
                kind: ActivityKind::Files,
                text: "2 files changed".into(),
                rev: None
            }]
        );
        assert_eq!(paths, vec!["a", "b"]);
        let c = snap_with(vec![ch("a", ' ', 'M', 4, 1), ch("b", '?', '?', 1, 0)]);
        assert_eq!(diff_snapshots(&b, &c, 6).0[0].text, "a +4 −1");
    }

    #[test]
    fn diff_reports_staging() {
        let s1 = snap_with(vec![ch("a", ' ', 'M', 3, 0), ch("b", ' ', 'M', 1, 0)]);
        let s2 = snap_with(vec![ch("a", 'M', ' ', 3, 0), ch("b", 'M', ' ', 1, 0)]);
        let (ev, _) = diff_snapshots(&s1, &s2, 5);
        assert_eq!(
            (ev[0].kind, ev[0].text.as_str()),
            (ActivityKind::Stage, "staged 2 files")
        );
        let (ev, _) = diff_snapshots(&s2, &s1, 5);
        assert_eq!(ev[0].text, "unstaged 2 files");
    }

    #[test]
    fn diff_quiet_on_commit_and_no_change() {
        let a = snap_with(vec![ch("a", 'M', ' ', 3, 0)]);
        let b = Snapshot {
            oid: Some("o2".into()),
            ..Snapshot::default()
        };
        assert!(diff_snapshots(&a, &b, 5).0.is_empty());
        assert!(diff_snapshots(&a, &a, 5).0.is_empty());
    }

    #[test]
    fn diff_reports_stash() {
        let a = Snapshot::default();
        let b = Snapshot {
            stash_count: 1,
            ..Snapshot::default()
        };
        assert_eq!(diff_snapshots(&a, &b, 5).0[0].text, "stash saved");
        assert_eq!(
            diff_snapshots(&b, &a, 5).0[0].text,
            "stash applied or dropped"
        );
    }

    #[test]
    fn merged_sorted_deduped_limited() {
        let live = vec![
            ActivityEvent {
                time: 20,
                kind: ActivityKind::Files,
                text: "x".into(),
                rev: None,
            },
            ActivityEvent {
                time: 20,
                kind: ActivityKind::Files,
                text: "x".into(),
                rev: None,
            },
        ];
        let m = merged(
            &[e("HEAD", "commit: a"), e("HEAD", "rebase (pick): q")],
            &live,
            10,
        );
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].time, 20);
        assert_eq!(merged(&[e("HEAD", "commit: a")], &live, 1).len(), 1);
    }
}
