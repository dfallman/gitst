//! Finds possible secrets in the working tree and in unpushed commits,
//! running git through a function the caller supplies. A commit never
//! changes, so each is scanned once and its findings kept by oid.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, PoisonError};

use ignore::gitignore::Gitignore;

use super::GitError;
use super::parse::{self, AddedFile};
use crate::leaks::{self, Hit};
use crate::model::{Change, Leak, LeakSource, ReflogEntry};

/// Runs git with the given arguments and returns its stdout.
pub type Runner<'a> = &'a dyn Fn(&[&str]) -> Result<Vec<u8>, GitError>;

/// Most unpushed commits scanned.
pub const MAX_SCANNED_COMMITS: usize = 50;

/// How far back pushes count when gitst starts.
const RECENT_PUSHES: i64 = 24 * 60 * 60;

/// The reflog message of a remote-tracking ref that `git push` moved.
const PUSH: &str = "update by push";

/// Which of `oids` no remote-tracking ref has.
fn not_on_remotes(git: Runner<'_>, oids: &[&str]) -> Result<HashSet<String>, GitError> {
    // `--no-walk`: the commits themselves, not their history.
    let mut args = vec!["rev-list", "--no-walk"];
    args.extend(oids);
    args.extend(["--not", "--remotes"]);
    let out = git(&args)?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .map(str::to_string)
        .collect())
}

/// Whether git failed because it does not have the object asked about.
fn unknown_object(e: &GitError) -> bool {
    e.message.contains("bad object") || e.message.contains("bad revision")
}

/// Goes before a command that reads patches: a blob over 1 MiB shows as
/// binary, unread, as an untracked file over that size is.
const BIG_FILES_AS_BINARY: [&str; 2] = ["-c", "core.bigFileThreshold=1m"];

/// Added lines only, no helpers, and `a/` `b/` prefixes whatever
/// `diff.noprefix` or `diff.mnemonicPrefix` say.
const PATCH_ARGS: [&str; 7] = [
    "-U0",
    "--no-ext-diff",
    "--no-textconv",
    "--no-color",
    "--no-renames",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

#[derive(Clone, Debug)]
pub struct LeakScanConfig {
    pub enabled: bool,
    /// `leak_allow`, rooted at the repository.
    pub allow: Gitignore,
}

impl Default for LeakScanConfig {
    fn default() -> Self {
        LeakScanConfig {
            enabled: true,
            allow: Gitignore::empty(),
        }
    }
}

#[derive(Default)]
struct Cache {
    /// Findings per unpushed commit, including commits with none.
    commits: HashMap<String, Vec<Leak>>,
    /// Unpushed commits whose patch alone is too large to read.
    too_large: HashSet<String>,
    /// Commits seen pushed this session whose patch is too large to read:
    /// never scanned, so said on every refresh.
    pushed_too_large: HashSet<String>,
    /// The last staged and unstaged findings, kept while a diff fails.
    worktree: [Vec<Leak>; 2],
    /// Findings of flagged commits seen reaching a remote this session.
    pushed: Vec<Leak>,
    /// The last unpushed list git gave, newest first.
    last: Vec<String>,
    /// When pushes start to count: a day before the first refresh.
    started: Option<i64>,
    /// The newest reflog entry of each remote-tracking ref already read:
    /// its time and hash, since times are whole seconds and two pushes can
    /// share one.
    pushes_seen: HashMap<String, (i64, String)>,
}

pub struct LeakScanner {
    cfg: LeakScanConfig,
    cache: Mutex<Cache>,
}

impl LeakScanner {
    pub fn new(cfg: LeakScanConfig) -> Self {
        LeakScanner {
            cfg,
            cache: Mutex::default(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    fn allowed(&self, path: &str) -> bool {
        leaks::is_allowed(&self.cfg.allow, path)
    }

    /// Content findings in an untracked file's text.
    pub fn untracked_file(&self, path: &str, text: &str) -> Vec<Leak> {
        if self.allowed(path) {
            return Vec::new();
        }
        let lines = text.lines().zip(1..).map(|(l, n)| (n, l));
        leaks::scan_lines(lines)
            .into_iter()
            .map(|h| from_hit(path, &LeakSource::Untracked, h))
            .collect()
    }

    /// Filename findings for paths new to the repository (untracked, or
    /// staged as added, renamed or copied), and, when `content` is set,
    /// content findings in the staged and unstaged diffs. A diff git cannot
    /// give adds to `errors`, and its last findings stand in.
    pub fn worktree(
        &self,
        git: Runner<'_>,
        changes: &[Change],
        content: bool,
        errors: &mut Vec<String>,
    ) -> Vec<Leak> {
        let mut out = Vec::new();
        for c in changes {
            let source = if c.untracked() {
                LeakSource::Untracked
            } else if matches!(c.x, 'A' | 'R' | 'C') {
                LeakSource::Staged
            } else {
                continue;
            };
            if let Some((rule, label)) = leaks::check_path(&c.path)
                && !self.allowed(&c.path)
            {
                out.push(Leak {
                    rule,
                    label,
                    path: c.path.clone(),
                    line: None,
                    snippet: None,
                    source,
                });
            }
        }
        if content {
            let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
            for (i, source) in [LeakSource::Staged, LeakSource::Unstaged]
                .into_iter()
                .enumerate()
            {
                let mut args = BIG_FILES_AS_BINARY.to_vec();
                args.push("diff");
                if source == LeakSource::Staged {
                    args.push("--cached");
                }
                args.extend(PATCH_ARGS);
                match git(&args) {
                    Ok(raw) => {
                        let found = self.scan_files(parse::parse_added_lines(&raw), &source, false);
                        cache.worktree[i] = found;
                    }
                    Err(e) => errors.push(e.message),
                }
                out.extend(cache.worktree[i].iter().cloned());
            }
        }
        out
    }

    /// Scans the patches of `oids` into the cache. A batch too large to
    /// read is read one commit at a time, and a commit too large alone is
    /// remembered as such rather than read again on every refresh.
    fn scan_commits(
        &self,
        cache: &mut Cache,
        git: Runner<'_>,
        oids: &[&str],
        errors: &mut Vec<String>,
    ) {
        let mut args = BIG_FILES_AS_BINARY.to_vec();
        args.extend(["log", "--no-walk=unsorted", "-p", "--format=%x00%H"]);
        args.extend(PATCH_ARGS);
        args.extend(oids);
        args.push("--");
        match git(&args) {
            Ok(raw) => {
                for (oid, patch) in parse::split_commit_patches(&raw) {
                    let source = LeakSource::Commit(oid.clone());
                    let found = self.scan_files(parse::parse_added_lines(patch), &source, true);
                    cache.commits.insert(oid, found);
                }
                // Commits without a patch, such as merges, have nothing to scan.
                for oid in oids {
                    cache.commits.entry(oid.to_string()).or_default();
                }
            }
            Err(e) if e.is_too_large() && oids.len() > 1 => {
                for oid in oids {
                    self.scan_commits(cache, git, &[oid], errors);
                }
            }
            Err(e) if e.is_too_large() => {
                cache.too_large.extend(oids.iter().map(|o| o.to_string()));
            }
            Err(e) => errors.push(e.message),
        }
    }

    /// Findings in the unpushed commits (newest first), then in commits
    /// seen pushed this session. `unpushed` is `None` when git could not
    /// list them, and the last list stands in. A flagged commit that left
    /// the list was either pushed, and is kept as `Pushed`, or rewritten
    /// away, and is dropped; while git cannot tell which, it stays.
    /// Commits that could not be read add to `errors`.
    pub fn commits(
        &self,
        git: Runner<'_>,
        unpushed: Option<&[String]>,
        errors: &mut Vec<String>,
    ) -> Vec<Leak> {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        let cache = &mut *cache;
        if let Some(u) = unpushed {
            cache.last = u.to_vec();
        }
        let unpushed = cache.last.clone();
        let new: Vec<&str> = unpushed
            .iter()
            .map(String::as_str)
            .filter(|o| !cache.commits.contains_key(*o) && !cache.too_large.contains(*o))
            .collect();
        if !new.is_empty() {
            self.scan_commits(cache, git, &new, errors);
        }
        let current: HashSet<&str> = unpushed.iter().map(String::as_str).collect();
        cache.too_large.retain(|o| current.contains(o.as_str()));
        if !cache.too_large.is_empty() {
            let n = cache.too_large.len();
            let s = if n == 1 { "" } else { "s" };
            errors.push(format!("{n} commit{s} too large to read"));
        }
        if !cache.pushed_too_large.is_empty() {
            let n = cache.pushed_too_large.len();
            let s = if n == 1 { "" } else { "s" };
            errors.push(format!("{n} pushed commit{s} too large to read"));
        }
        let mut gone: Vec<String> = cache
            .commits
            .keys()
            .filter(|o| !current.contains(o.as_str()))
            .cloned()
            .collect();
        gone.sort();
        // A clean commit needs no check.
        gone.retain(|oid| {
            let clean = cache.commits.get(oid).is_some_and(Vec::is_empty);
            if clean {
                cache.commits.remove(oid);
            }
            !clean
        });
        if !gone.is_empty() {
            // One check for all: those git still lists are on no remote,
            // so rewritten away, and dropped; the rest were pushed. If git
            // fails on one, an object it no longer has, each is asked about
            // alone, so that one cannot hold back the rest.
            let gone_refs: Vec<&str> = gone.iter().map(String::as_str).collect();
            let mut on_remote: HashSet<&str> = HashSet::new();
            // Unknown for now: kept, and asked again next refresh.
            let mut keep: HashSet<&str> = HashSet::new();
            match not_on_remotes(git, &gone_refs) {
                Ok(local) => {
                    on_remote.extend(gone_refs.iter().filter(|o| !local.contains(**o)));
                }
                Err(batch) => {
                    for oid in &gone_refs {
                        // Alone, the batch's answer is the oid's own.
                        let one = if gone_refs.len() == 1 {
                            Err(batch.clone())
                        } else {
                            not_on_remotes(git, std::slice::from_ref(oid))
                        };
                        match one {
                            Ok(local) if local.is_empty() => {
                                on_remote.insert(oid);
                            }
                            // Still local, or pruned: nowhere a remote has.
                            Ok(_) => {}
                            Err(e) if unknown_object(&e) => {}
                            Err(e) => {
                                errors.push(e.message);
                                keep.insert(oid);
                            }
                        }
                    }
                }
            }
            for oid in &gone {
                if keep.contains(oid.as_str()) {
                    continue;
                }
                let found = cache.commits.remove(oid).unwrap_or_default();
                let known = cache.pushed.iter().any(|l| l.source.oid() == Some(oid));
                if on_remote.contains(oid.as_str()) && !known {
                    cache.pushed.extend(found.into_iter().map(|l| Leak {
                        source: LeakSource::Pushed(oid.clone()),
                        ..l
                    }));
                }
            }
        }
        let mut out: Vec<Leak> = unpushed
            .iter()
            .chain(&gone)
            .filter_map(|o| cache.commits.get(o))
            .flatten()
            .cloned()
            .collect();
        out.extend(cache.pushed.iter().cloned());
        out
    }

    /// Reads the commits that pushes from here sent to remote-tracking
    /// refs since the last call, or, the first time, in the last day, so
    /// that a commit pushed before a refresh saw it unpushed is still read.
    /// `commits` then finds them on a remote and reports them as pushed.
    /// Every ref under `refs/remotes/` is read, in one `git log -g`: a
    /// push can go to a ref that is no branch's upstream.
    pub fn recent_pushes(&self, git: Runner<'_>, now: i64, errors: &mut Vec<String>) {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        let cache = &mut *cache;
        let start = *cache.started.get_or_insert(now - RECENT_PUSHES);
        let fmt = format!("--format={}", parse::REFLOGS_FORMAT);
        let raw = match git(&[
            "log",
            "-g",
            "--date=unix",
            &fmt,
            "--glob=refs/remotes/",
            "--",
        ]) {
            Ok(raw) => raw,
            Err(e) => {
                errors.push(e.message);
                return;
            }
        };
        // Per ref, newest first, each with the value it replaced after it.
        let mut by_ref: Vec<(&str, Vec<&ReflogEntry>)> = Vec::new();
        let entries = parse::parse_reflogs(&raw);
        for e in &entries {
            match by_ref.iter_mut().find(|(r, _)| *r == e.refname) {
                Some((_, v)) => v.push(e),
                None => by_ref.push((&e.refname, vec![e])),
            }
        }
        for (refname, entries) in by_ref {
            let seen = cache.pushes_seen.get(refname);
            let (mut tips, mut bases) = (Vec::new(), Vec::new());
            for (i, e) in entries.iter().enumerate() {
                // Up to the entry read last time, or, should that have
                // expired, to the first older one.
                let read = match seen {
                    Some((t, oid)) => e.time < *t || (e.time == *t && e.short == *oid),
                    None => e.time <= start,
                };
                if read {
                    break;
                }
                if e.message == PUSH {
                    tips.push(e.short.as_str());
                    bases.extend(entries.get(i + 1).map(|b| b.short.as_str()));
                }
            }
            let Some(newest) = entries.first() else {
                continue;
            };
            let newest = (newest.time, newest.short.clone());
            if tips.is_empty() {
                cache.pushes_seen.insert(refname.to_string(), newest);
                continue;
            }
            // Not what the ref held before, nor what other remote refs
            // already had.
            let short = refname.trim_start_matches("refs/remotes/");
            let exclude = format!("--exclude={short}");
            let n = format!("-n{MAX_SCANNED_COMMITS}");
            let mut args = vec!["rev-list", n.as_str()];
            args.extend(&tips);
            args.push("--not");
            args.extend(&bases);
            args.extend([exclude.as_str(), "--remotes"]);
            match git(&args) {
                Ok(out) => {
                    let out = String::from_utf8_lossy(&out);
                    let new: Vec<&str> = out
                        .lines()
                        .filter(|o| {
                            !cache.commits.contains_key(*o)
                                && !cache.too_large.contains(*o)
                                && !cache.pushed_too_large.contains(*o)
                                && !cache.pushed.iter().any(|l| l.source.oid() == Some(o))
                        })
                        .collect();
                    if !new.is_empty() {
                        self.scan_commits(cache, git, &new, errors);
                        // Not unpushed, so `commits` would forget them.
                        for oid in new {
                            if cache.too_large.remove(oid) {
                                cache.pushed_too_large.insert(oid.to_string());
                            }
                        }
                    }
                    cache.pushes_seen.insert(refname.to_string(), newest);
                }
                Err(e) => errors.push(e.message),
            }
        }
    }

    /// Content findings in added lines, and, with `filenames`, filename
    /// findings for files the patch creates.
    fn scan_files(&self, files: Vec<AddedFile>, source: &LeakSource, filenames: bool) -> Vec<Leak> {
        let mut out = Vec::new();
        for f in files {
            if self.allowed(&f.path) {
                continue;
            }
            if filenames
                && f.new_file
                && let Some((rule, label)) = leaks::check_path(&f.path)
            {
                out.push(Leak {
                    rule,
                    label,
                    path: f.path.clone(),
                    line: None,
                    snippet: None,
                    source: source.clone(),
                });
            }
            let lines = f.lines.iter().map(|(n, t)| (*n, t.as_str()));
            out.extend(
                leaks::scan_lines(lines)
                    .into_iter()
                    .map(|h| from_hit(&f.path, source, h)),
            );
        }
        out
    }
}

fn from_hit(path: &str, source: &LeakSource, h: Hit) -> Leak {
    Leak {
        rule: h.rule,
        label: h.label,
        path: path.to_string(),
        line: Some(h.line),
        snippet: Some(h.snippet),
        source: source.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn key() -> String {
        format!("{}{}", "AK", "IAQ7LM2XRT5VBN8KWD")
    }

    /// One commit's part of `git log --format=%x00%H -p`: a new file.
    fn patch(oid: &str, path: &str, line: &str) -> String {
        format!(
            "\0{oid}\n\ndiff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1 @@\n+{line}\n"
        )
    }

    /// Answers `log` with the patches of the oids asked for (or, past
    /// `max_patches` of them, as too large), and `rev-list … --not
    /// --remotes` with the oids not in `remote` (or an error while
    /// `check_fails`); records every call, without any leading `-c`.
    #[derive(Default)]
    struct Fake {
        patches: HashMap<String, String>,
        remote: Vec<String>,
        check_fails: Cell<bool>,
        max_patches: Option<usize>,
        calls: RefCell<Vec<String>>,
    }

    impl Fake {
        fn with(patches: &[(&str, String)]) -> Fake {
            Fake {
                patches: patches
                    .iter()
                    .map(|(o, p)| (o.to_string(), p.clone()))
                    .collect(),
                ..Fake::default()
            }
        }

        fn run(&self, args: &[&str]) -> Result<Vec<u8>, GitError> {
            let args = match args {
                ["-c", _, rest @ ..] => rest,
                _ => args,
            };
            self.calls.borrow_mut().push(args.join(" "));
            let out: String = match args[0] {
                "log" => {
                    let found: Vec<&String> =
                        args.iter().filter_map(|a| self.patches.get(*a)).collect();
                    if self.max_patches.is_some_and(|m| found.len() > m) {
                        return Err(GitError::too_large("log"));
                    }
                    found.into_iter().cloned().collect()
                }
                "rev-list" if self.check_fails.get() => {
                    return Err(GitError::new("git rev-list timed out"));
                }
                "rev-list" => args[1..]
                    .iter()
                    .take_while(|a| **a != "--not")
                    .filter(|a| !a.starts_with("--"))
                    .filter(|o| !self.remote.iter().any(|r| r == *o))
                    .map(|o| format!("{o}\n"))
                    .collect(),
                _ => String::new(),
            };
            Ok(out.into_bytes())
        }

        fn calls_to(&self, cmd: &str) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .filter(|c| c.starts_with(cmd))
                .cloned()
                .collect()
        }
    }

    fn oids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn change(path: &str, x: char, y: char) -> Change {
        Change {
            path: path.into(),
            orig_path: None,
            x,
            y,
            counts: Default::default(),
            modified: None,
        }
    }

    #[test]
    fn unpushed_commits_are_scanned_once() {
        let fake = Fake::with(&[
            ("c1", patch("c1", "deploy.txt", &key())),
            ("c2", patch("c2", ".env", "A=1")),
        ]);
        let s = LeakScanner::new(LeakScanConfig::default());
        let run = |a: &[&str]| fake.run(a);
        let first = s.commits(&run, Some(&oids(&["c2", "c1"])), &mut Vec::new());
        let got: Vec<(&str, Option<&str>, Option<u32>)> = first
            .iter()
            .map(|l| (l.rule, l.source.oid(), l.line))
            .collect();
        assert_eq!(
            got,
            vec![
                ("env-file", Some("c2"), None),
                ("aws-access-key", Some("c1"), Some(1)),
            ]
        );
        assert_eq!(
            s.commits(&run, Some(&oids(&["c2", "c1"])), &mut Vec::new()),
            first
        );
        assert_eq!(
            fake.calls_to("log").len(),
            1,
            "cached commits are not read again"
        );
    }

    #[test]
    fn a_commit_that_reaches_a_remote_stays_listed_as_pushed() {
        let mut fake = Fake::with(&[("c1", patch("c1", "deploy.txt", &key()))]);
        fake.remote = oids(&["c1"]);
        let s = LeakScanner::new(LeakScanConfig::default());
        let run = |a: &[&str]| fake.run(a);
        s.commits(&run, Some(&oids(&["c1"])), &mut Vec::new());
        let after = s.commits(&run, Some(&[]), &mut Vec::new());
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].source, LeakSource::Pushed("c1".into()));
        assert_eq!(
            fake.calls_to("rev-list"),
            vec!["rev-list --no-walk c1 --not --remotes"],
            "only the commit itself is looked at"
        );
        assert_eq!(
            s.commits(&run, Some(&[]), &mut Vec::new()),
            after,
            "kept for the session"
        );
    }

    #[test]
    fn a_failed_push_check_keeps_the_finding_and_retries() {
        let mut fake = Fake::with(&[("c1", patch("c1", "deploy.txt", &key()))]);
        fake.remote = oids(&["c1"]);
        let s = LeakScanner::new(LeakScanConfig::default());
        let run = |a: &[&str]| fake.run(a);
        s.commits(&run, Some(&oids(&["c1"])), &mut Vec::new());
        fake.check_fails.set(true);
        let mut errors = Vec::new();
        let kept = s.commits(&run, Some(&[]), &mut errors);
        assert_eq!(kept.len(), 1, "a failed check must not drop the finding");
        assert_eq!(kept[0].source, LeakSource::Commit("c1".into()));
        assert_eq!(errors, vec!["git rev-list timed out"]);
        fake.check_fails.set(false);
        let after = s.commits(&run, Some(&[]), &mut Vec::new());
        assert_eq!(after[0].source, LeakSource::Pushed("c1".into()));
        assert_eq!(fake.calls_to("rev-list").len(), 2, "checked again");
    }

    #[test]
    fn an_unknown_range_keeps_the_last_findings() {
        let fake = Fake::with(&[("c1", patch("c1", "deploy.txt", &key()))]);
        let s = LeakScanner::new(LeakScanConfig::default());
        let run = |a: &[&str]| fake.run(a);
        let first = s.commits(&run, Some(&oids(&["c1"])), &mut Vec::new());
        assert_eq!(s.commits(&run, None, &mut Vec::new()), first);
        assert!(
            fake.calls_to("rev-list").is_empty(),
            "nothing left the list"
        );
    }

    #[test]
    fn a_rewritten_commit_is_dropped_and_clean_ones_need_no_check() {
        let fake = Fake::with(&[
            ("c1", patch("c1", "deploy.txt", &key())),
            ("c2", patch("c2", "a.txt", "fine")),
            ("c3", patch("c3", "b.txt", &key())),
        ]);
        let s = LeakScanner::new(LeakScanConfig::default());
        let run = |a: &[&str]| fake.run(a);
        s.commits(&run, Some(&oids(&["c3", "c2", "c1"])), &mut Vec::new());
        assert!(s.commits(&run, Some(&[]), &mut Vec::new()).is_empty());
        assert_eq!(
            fake.calls_to("rev-list"),
            vec!["rev-list --no-walk c1 c3 --not --remotes"],
            "one check for every flagged commit"
        );
    }

    #[test]
    fn a_batch_too_large_is_read_one_by_one_and_a_huge_commit_once() {
        let mut fake = Fake::with(&[
            ("c1", patch("c1", "deploy.txt", &key())),
            ("c2", patch("c2", "a.txt", "fine")),
        ]);
        fake.max_patches = Some(1);
        let s = LeakScanner::new(LeakScanConfig::default());
        let run = |a: &[&str]| fake.run(a);
        let mut errors = Vec::new();
        let found = s.commits(&run, Some(&oids(&["c2", "c1"])), &mut errors);
        assert_eq!(found.len(), 1);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(fake.calls_to("log").len(), 3, "the batch, then each");

        fake.max_patches = Some(0);
        let s = LeakScanner::new(LeakScanConfig::default());
        let run = |a: &[&str]| fake.run(a);
        let mut errors = Vec::new();
        s.commits(&run, Some(&oids(&["c1"])), &mut errors);
        assert_eq!(errors, vec!["1 commit too large to read"]);
        let mut errors = Vec::new();
        s.commits(&run, Some(&oids(&["c1"])), &mut errors);
        assert_eq!(errors, vec!["1 commit too large to read"], "still said");
        assert_eq!(fake.calls_to("log").len(), 4, "not read again");
        let mut errors = Vec::new();
        s.commits(&run, Some(&[]), &mut errors);
        assert!(errors.is_empty(), "gone, so no longer a gap");
    }

    #[test]
    fn a_failed_diff_is_reported_and_keeps_its_last_findings() {
        let staged = format!(
            "diff --git a/k b/k\n--- a/k\n+++ b/k\n@@ -1,0 +1 @@\n+{}\n",
            key()
        );
        let fail = Cell::new(false);
        let run = |a: &[&str]| -> Result<Vec<u8>, GitError> {
            if fail.get() {
                return Err(GitError::new("git diff timed out"));
            }
            Ok(if a.contains(&"--cached") {
                staged.clone().into_bytes()
            } else {
                Vec::new()
            })
        };
        let s = LeakScanner::new(LeakScanConfig::default());
        let mut errors = Vec::new();
        let first = s.worktree(&run, &[], true, &mut errors);
        assert_eq!(first.len(), 1);
        assert!(errors.is_empty());
        fail.set(true);
        let again = s.worktree(&run, &[], true, &mut errors);
        assert_eq!(again, first);
        assert_eq!(errors, vec!["git diff timed out"; 2]);
    }

    #[test]
    fn a_recent_push_is_read_and_reported_as_pushed() {
        let mut fake = Fake::with(&[("c1", patch("c1", "deploy.txt", &key()))]);
        fake.remote = oids(&["c1"]);
        let now = 1_800_000_000;
        // `log -g` over the remote refs, newest first: the push of c1 over
        // c0 at `time`.
        let reflog_raw = |time: i64| {
            format!(
                "c1\x1frefs/remotes/origin/main@{{{time}}}\x1f{PUSH}\x1e\nc0\x1frefs/remotes/origin/main@{{1}}\x1fupdate by fetch\x1e"
            )
        };
        let reflog = RefCell::new(reflog_raw(now - 2 * RECENT_PUSHES));
        let run = |a: &[&str]| -> Result<Vec<u8>, GitError> {
            if a[0] == "log" && a[1] == "-g" {
                fake.calls.borrow_mut().push(a.join(" "));
                return Ok(reflog.borrow().clone().into_bytes());
            }
            if a[0] == "rev-list" && a.contains(&"--exclude=origin/main") {
                fake.calls.borrow_mut().push(a.join(" "));
                return Ok(b"c1\n".to_vec());
            }
            fake.run(a)
        };
        let s = LeakScanner::new(LeakScanConfig::default());
        let mut errors = Vec::new();
        // Too old to count.
        s.recent_pushes(&run, now, &mut errors);
        assert!(fake.calls_to("rev-list -n50").is_empty());
        *reflog.borrow_mut() = reflog_raw(now - 60);
        s.recent_pushes(&run, now, &mut errors);
        assert_eq!(
            fake.calls_to("rev-list -n50"),
            vec!["rev-list -n50 c1 --not c0 --exclude=origin/main --remotes"]
        );
        let found = s.commits(&run, Some(&[]), &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].source, LeakSource::Pushed("c1".into()));
        // The same push is read once.
        s.recent_pushes(&run, now, &mut errors);
        assert_eq!(fake.calls_to("rev-list -n50").len(), 1);
    }

    #[test]
    fn a_second_push_in_the_same_second_is_read() {
        let mut fake = Fake::with(&[
            ("c1", patch("c1", "a.txt", "x")),
            ("c2", patch("c2", "deploy.txt", &key())),
        ]);
        fake.remote = oids(&["c1", "c2"]);
        let now = 1_800_000_000;
        let t = now - 60;
        // Reflog times are whole seconds: two pushes can share one.
        let reflog = RefCell::new(format!(
            "c1\x1frefs/remotes/origin/main@{{{t}}}\x1f{PUSH}\x1e\nc0\x1frefs/remotes/origin/main@{{1}}\x1fupdate by fetch\x1e"
        ));
        let run = |a: &[&str]| -> Result<Vec<u8>, GitError> {
            if a[0] == "log" && a[1] == "-g" {
                return Ok(reflog.borrow().clone().into_bytes());
            }
            if a[0] == "rev-list" && a.contains(&"--exclude=origin/main") {
                fake.calls.borrow_mut().push(a.join(" "));
                return Ok(a[2].as_bytes().to_vec());
            }
            fake.run(a)
        };
        let s = LeakScanner::new(LeakScanConfig::default());
        let mut errors = Vec::new();
        s.recent_pushes(&run, now, &mut errors);
        *reflog.borrow_mut() = format!(
            "c2\x1frefs/remotes/origin/main@{{{t}}}\x1f{PUSH}\x1e\nc1\x1frefs/remotes/origin/main@{{{t}}}\x1f{PUSH}\x1e\nc0\x1frefs/remotes/origin/main@{{1}}\x1fupdate by fetch\x1e"
        );
        s.recent_pushes(&run, now, &mut errors);
        assert_eq!(
            fake.calls_to("rev-list -n50"),
            vec![
                "rev-list -n50 c1 --not c0 --exclude=origin/main --remotes",
                "rev-list -n50 c2 --not c1 --exclude=origin/main --remotes",
            ]
        );
        let found = s.commits(&run, Some(&[]), &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].source, LeakSource::Pushed("c2".into()));
        // Read once.
        s.recent_pushes(&run, now, &mut errors);
        assert_eq!(fake.calls_to("rev-list -n50").len(), 2);
    }

    #[test]
    fn a_pushed_commit_too_large_to_read_is_said_to_be() {
        let mut fake = Fake::with(&[("c1", patch("c1", "deploy.txt", &key()))]);
        fake.remote = oids(&["c1"]);
        fake.max_patches = Some(0);
        let now = 1_800_000_000;
        let reflog = format!(
            "c1\x1frefs/remotes/origin/main@{{{t}}}\x1f{PUSH}\x1e\nc0\x1frefs/remotes/origin/main@{{1}}\x1fupdate by fetch\x1e",
            t = now - 60
        );
        let run = |a: &[&str]| -> Result<Vec<u8>, GitError> {
            if a[0] == "log" && a[1] == "-g" {
                return Ok(reflog.clone().into_bytes());
            }
            if a[0] == "rev-list" && a.contains(&"--exclude=origin/main") {
                return Ok(b"c1\n".to_vec());
            }
            fake.run(a)
        };
        let s = LeakScanner::new(LeakScanConfig::default());
        for _ in 0..2 {
            let mut errors = Vec::new();
            s.recent_pushes(&run, now, &mut errors);
            assert!(s.commits(&run, Some(&[]), &mut errors).is_empty());
            assert_eq!(errors, vec!["1 pushed commit too large to read"]);
        }
    }

    #[test]
    fn worktree_flags_new_paths_and_added_lines() {
        let staged = format!(
            "diff --git a/cfg.rs b/cfg.rs\n--- a/cfg.rs\n+++ b/cfg.rs\n@@ -1,0 +2 @@\n+let k = \"{}\";\n",
            key()
        );
        let run = |a: &[&str]| -> Result<Vec<u8>, GitError> {
            Ok(if a.contains(&"--cached") {
                staged.clone().into_bytes()
            } else {
                Vec::new()
            })
        };
        let changes = [
            change(".env", '?', '?'),
            change("id_rsa", 'A', ' '),
            // Already tracked: editing it is not new.
            change(".env.prod", ' ', 'M'),
            change("cfg.rs", 'M', ' '),
        ];
        let s = LeakScanner::new(LeakScanConfig::default());
        let found = s.worktree(&run, &changes, true, &mut Vec::new());
        let got: Vec<(&str, &str, Option<u32>)> = found
            .iter()
            .map(|l| (l.path.as_str(), l.rule, l.line))
            .collect();
        assert_eq!(
            got,
            vec![
                (".env", "env-file", None),
                ("id_rsa", "ssh-private-key", None),
                ("cfg.rs", "aws-access-key", Some(2)),
            ]
        );
        assert_eq!(found[2].source, LeakSource::Staged);
        assert!(
            s.worktree(&run, &changes, false, &mut Vec::new())
                .iter()
                .all(|l| l.line.is_none()),
            "no content scan when skipped"
        );
    }

    #[test]
    fn allow_list_covers_every_source() {
        let cfg = LeakScanConfig {
            enabled: true,
            allow: leaks::allow_matcher(std::path::Path::new("/r"), &["fixtures/".into()]),
        };
        let s = LeakScanner::new(cfg);
        let fake = Fake::with(&[("c1", patch("c1", "fixtures/.env", &key()))]);
        let run = |a: &[&str]| fake.run(a);
        assert!(
            s.commits(&run, Some(&oids(&["c1"])), &mut Vec::new())
                .is_empty()
        );
        assert!(s.untracked_file("fixtures/k.txt", &key()).is_empty());
        assert!(
            s.worktree(
                &run,
                &[change("fixtures/.env", '?', '?')],
                false,
                &mut Vec::new()
            )
            .is_empty()
        );
        assert_eq!(s.untracked_file("k.txt", &key()).len(), 1);
    }
}
