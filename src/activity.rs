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
    /// The one changed file the event is about, which clicking can open.
    pub path: Option<String>,
}

/// What `git stash` writes to HEAD's reflog, as a user's `git reset --hard
/// HEAD` also does.
const STASH_RESET: &str = "reset: moving to HEAD";

/// Turns a reflog entry into a timeline event, or `None` for noise such as
/// individual rebase picks.
pub fn classify(e: &ReflogEntry) -> Option<ActivityEvent> {
    let event = |kind, text: String| {
        Some(ActivityEvent {
            time: e.time,
            kind,
            text,
            rev: Some(e.short.clone()),
            path: None,
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
        } else if action.contains("(abort)") {
            event(ActivityKind::Rebase, "rebase aborted".into())
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

/// A file event for `paths` as they stand in `snap`: `path +a −r` for one
/// changed path, `path discarded` for one no longer changed, or
/// `N files changed` / `N files discarded`.
pub fn files_event(paths: &[String], snap: &Snapshot, time: i64) -> ActivityEvent {
    let find = |p: &String| snap.changes.iter().find(|c| &c.path == p);
    let (text, path) = match paths {
        [p] => match find(p) {
            Some(c) => {
                let mut text = p.clone();
                let (a, r) = c.counts.known().unwrap_or((0, 0));
                if a > 0 {
                    text.push_str(&format!(" +{a}"));
                }
                if r > 0 {
                    text.push_str(&format!(" −{r}"));
                }
                (text, Some(p.clone()))
            }
            None => (format!("{p} discarded"), None),
        },
        _ if paths.iter().all(|p| find(p).is_none()) => {
            (format!("{} discarded", plural(paths.len(), "file")), None)
        }
        _ => (format!("{} changed", plural(paths.len(), "file")), None),
    };
    ActivityEvent {
        time,
        kind: ActivityKind::Files,
        text,
        rev: None,
        path,
    }
}

/// HEAD's newest reflog entry.
fn newest_head(snap: &Snapshot) -> Option<&ReflogEntry> {
    snap.reflog.iter().find(|r| r.refname == "HEAD")
}

/// Events describing what changed between two snapshots, plus the paths
/// whose status or line counts changed (for the change pulse). `gone` says
/// whether a path no longer exists on disk: an untracked path that leaves
/// the list but still exists was ignored, not discarded.
pub fn diff_snapshots(
    prev: &Snapshot,
    next: &Snapshot,
    now: i64,
    gone: &dyn Fn(&str) -> bool,
) -> (Vec<ActivityEvent>, Vec<String>) {
    let before: HashMap<&str, &Change> =
        prev.changes.iter().map(|c| (c.path.as_str(), c)).collect();
    let changed: Vec<(&Change, Option<&&Change>)> = next
        .changes
        .iter()
        .map(|c| (c, before.get(c.path.as_str())))
        .filter(|(c, old)| old.is_none_or(|o| o != c))
        .collect();

    // Paths that left the list, unless a commit, reset, checkout or stash
    // explains it, or the list is capped and a path may only have moved
    // past the cap.
    let explained = newest_head(prev) != newest_head(next)
        || prev.oid != next.oid
        || prev.stash_count != next.stash_count;
    let capped = prev.changes_omitted > 0 || next.changes_omitted > 0;
    let left: Vec<&Change> = if explained || capped {
        Vec::new()
    } else {
        let now_paths: HashSet<&str> = next.changes.iter().map(|c| c.path.as_str()).collect();
        prev.changes
            .iter()
            .filter(|c| !now_paths.contains(c.path.as_str()))
            .filter(|c| !c.untracked() || gone(&c.path))
            .collect()
    };

    let mut events = Vec::new();
    let event = |kind, text: String| ActivityEvent {
        time: now,
        kind,
        text,
        rev: None,
        path: None,
    };
    let paths: Vec<String> = changed
        .iter()
        .map(|(c, _)| c.path.clone())
        .chain(left.iter().map(|c| c.path.clone()))
        .collect();
    // A path that only moved into or out of the index, the rest unchanged.
    let moved = |(c, old): &(&Change, Option<&&Change>), to_index: bool| {
        old.is_some_and(|o| {
            o.counts == c.counts && (o.x == ' ') == to_index && (c.x == ' ') != to_index
        })
    };
    let staged = changed.iter().filter(|c| moved(c, true)).count();
    let unstaged = changed.iter().filter(|c| moved(c, false)).count();
    // Stage rows come with an edit in the same refresh, as `git add`
    // right after a save often does.
    if staged > 0 {
        events.push(event(
            ActivityKind::Stage,
            format!("staged {}", plural(staged, "file")),
        ));
    }
    if unstaged > 0 {
        events.push(event(
            ActivityKind::Stage,
            format!("unstaged {}", plural(unstaged, "file")),
        ));
    }
    let edited: Vec<String> = changed
        .iter()
        .filter(|c| !moved(c, true) && !moved(c, false))
        .map(|(c, _)| c.path.clone())
        .chain(left.iter().map(|c| c.path.clone()))
        .collect();
    if !edited.is_empty() {
        events.push(files_event(&edited, next, now));
    }
    if next.stash_count > prev.stash_count {
        // The stash takes the time of the reset `git stash` wrote, which
        // `merged` then leaves out.
        let reset = next
            .reflog
            .iter()
            .filter(|r| r.refname == "HEAD" && !prev.reflog.contains(r))
            .find(|r| r.message == STASH_RESET);
        events.push(ActivityEvent {
            time: reset.map_or(now, |r| r.time),
            ..event(ActivityKind::Stash, "stash saved".into())
        });
    } else if next.stash_count < prev.stash_count {
        events.push(event(
            ActivityKind::Stash,
            "stash applied or dropped".into(),
        ));
    }
    (events, paths)
}

/// The timeline shown in the Activity section, newest first.
pub fn merged(reflog: &[ReflogEntry], live: &[ActivityEvent], limit: usize) -> Vec<ActivityEvent> {
    let stashes: HashSet<i64> = live
        .iter()
        .filter(|e| e.kind == ActivityKind::Stash)
        .map(|e| e.time)
        .collect();
    let mut all: Vec<ActivityEvent> = reflog
        .iter()
        .filter(|r| !(r.message == STASH_RESET && stashes.contains(&r.time)))
        .filter_map(classify)
        .chain(live.iter().cloned())
        .collect();
    all.sort_by_key(|e| std::cmp::Reverse(e.time));
    let mut seen = HashSet::new();
    all.retain(|e| seen.insert((e.time, e.text.clone())));
    all.truncate(limit);
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Counts;

    /// Treats every path that left the list as gone from disk.
    fn diff(a: &Snapshot, b: &Snapshot, now: i64) -> (Vec<ActivityEvent>, Vec<String>) {
        diff_snapshots(a, b, now, &|_| true)
    }

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
            counts: Counts::lines(a, r),
            modified: None,
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
    fn a_stage_and_an_edit_in_one_refresh_are_both_shown() {
        let before = snap_with(vec![ch("a.rs", ' ', 'M', 1, 0), ch("b.rs", ' ', 'M', 1, 0)]);
        let after = snap_with(vec![ch("a.rs", 'M', ' ', 1, 0), ch("b.rs", ' ', 'M', 3, 0)]);
        let (events, paths) = diff(&before, &after, 50);
        let texts: Vec<&str> = events.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["staged 1 file", "b.rs +3"]);
        assert_eq!(paths, vec!["a.rs", "b.rs"]);
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
        let (ev, paths) = diff(&a, &b, 5);
        assert_eq!(
            ev,
            vec![ActivityEvent {
                time: 5,
                kind: ActivityKind::Files,
                text: "2 files changed".into(),
                rev: None,
                path: None,
            }]
        );
        assert_eq!(paths, vec!["a", "b"]);
        let c = snap_with(vec![ch("a", ' ', 'M', 4, 1), ch("b", '?', '?', 1, 0)]);
        assert_eq!(diff(&b, &c, 6).0[0].text, "a +4 −1");
    }

    #[test]
    fn single_file_event_omits_zero_counts() {
        let a = snap_with(vec![]);
        let b = snap_with(vec![ch("new.md", '?', '?', 1, 0)]);
        assert_eq!(diff(&a, &b, 5).0[0].text, "new.md +1");
        let c = snap_with(vec![ch("gone.rs", ' ', 'D', 0, 7)]);
        assert_eq!(diff(&a, &c, 5).0[0].text, "gone.rs −7");
    }

    #[test]
    fn diff_reports_staging() {
        let s1 = snap_with(vec![ch("a", ' ', 'M', 3, 0), ch("b", ' ', 'M', 1, 0)]);
        let s2 = snap_with(vec![ch("a", 'M', ' ', 3, 0), ch("b", 'M', ' ', 1, 0)]);
        let (ev, _) = diff(&s1, &s2, 5);
        assert_eq!(
            (ev[0].kind, ev[0].text.as_str()),
            (ActivityKind::Stage, "staged 2 files")
        );
        let (ev, _) = diff(&s2, &s1, 5);
        assert_eq!(ev[0].text, "unstaged 2 files");
    }

    #[test]
    fn diff_quiet_on_commit_and_no_change() {
        let a = snap_with(vec![ch("a", 'M', ' ', 3, 0)]);
        let b = Snapshot {
            oid: Some("o2".into()),
            ..Snapshot::default()
        };
        assert!(diff(&a, &b, 5).0.is_empty());
        assert!(diff(&a, &a, 5).0.is_empty());
    }

    #[test]
    fn diff_reports_stash() {
        let a = Snapshot::default();
        let b = Snapshot {
            stash_count: 1,
            ..Snapshot::default()
        };
        assert_eq!(diff(&a, &b, 5).0[0].text, "stash saved");
        assert_eq!(diff(&b, &a, 5).0[0].text, "stash applied or dropped");
    }

    fn at(time: i64, m: &str) -> ReflogEntry {
        ReflogEntry {
            time,
            ..e("HEAD", m)
        }
    }

    #[test]
    fn stash_is_not_also_shown_as_a_reset() {
        let prev = Snapshot::default();
        let next = Snapshot {
            stash_count: 1,
            reflog: vec![at(9, "reset: moving to HEAD")],
            ..Snapshot::default()
        };
        let (events, _) = diff(&prev, &next, 12);
        let texts: Vec<_> = merged(&next.reflog, &events, 10)
            .into_iter()
            .map(|e| (e.time, e.text))
            .collect();
        assert_eq!(texts, vec![(9, "stash saved".to_string())]);
        // A reset to HEAD of its own still shows.
        let texts: Vec<_> = merged(&next.reflog, &[], 10)
            .into_iter()
            .map(|e| e.text)
            .collect();
        assert_eq!(texts, vec!["reset to HEAD"]);
    }

    #[test]
    fn aborted_rebase_is_shown() {
        let ev = classify(&e("HEAD", "rebase (abort): returning to refs/heads/side")).unwrap();
        assert_eq!(
            (ev.kind, ev.text.as_str()),
            (ActivityKind::Rebase, "rebase aborted")
        );
    }

    #[test]
    fn files_leaving_the_list_are_reported() {
        let a = snap_with(vec![ch("a", ' ', 'M', 1, 0), ch("b", '?', '?', 2, 0)]);
        let b = snap_with(vec![ch("b", '?', '?', 2, 0)]);
        let (ev, paths) = diff(&a, &b, 5);
        assert_eq!(ev[0].text, "a discarded");
        assert_eq!(paths, vec!["a"]);
        let (ev, _) = diff(&a, &snap_with(vec![]), 5);
        assert_eq!(ev[0].text, "2 files discarded");
        // An untracked file still on disk was ignored, not discarded.
        let only_a = snap_with(vec![ch("a", ' ', 'M', 1, 0)]);
        let (ev, paths) = diff_snapshots(&a, &only_a, 5, &|_| false);
        assert_eq!((ev, paths), (vec![], Vec::<String>::new()));
        let (ev, _) = diff_snapshots(&a, &snap_with(vec![]), 5, &|p| p != "b");
        assert_eq!(ev[0].text, "a discarded");
        // A new HEAD reflog entry (commit, reset, checkout) explains it.
        let reset = Snapshot {
            reflog: vec![at(4, "reset: moving to HEAD")],
            ..snap_with(vec![])
        };
        assert!(diff(&a, &reset, 5).0.is_empty());
    }

    #[test]
    fn single_path_events_carry_the_path() {
        let a = snap_with(vec![]);
        let b = snap_with(vec![ch("a", ' ', 'M', 1, 0)]);
        assert_eq!(diff(&a, &b, 5).0[0].path.as_deref(), Some("a"));
        let c = snap_with(vec![ch("a", ' ', 'M', 1, 0), ch("b", ' ', 'M', 1, 0)]);
        assert_eq!(diff(&a, &c, 5).0[0].path, None);
    }

    #[test]
    fn merged_sorted_deduped_limited() {
        let live = vec![
            ActivityEvent {
                time: 20,
                kind: ActivityKind::Files,
                text: "x".into(),
                rev: None,
                path: None,
            },
            ActivityEvent {
                time: 20,
                kind: ActivityKind::Files,
                text: "x".into(),
                rev: None,
                path: None,
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
