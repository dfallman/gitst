//! Finds possible secrets in the working tree and in unpushed commits,
//! running git through a function the caller supplies. A commit never
//! changes, so each is scanned once and its findings kept by oid.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, PoisonError};

use ignore::gitignore::Gitignore;

use super::GitError;
use super::parse::{self, AddedFile};
use crate::leaks::{self, Hit};
use crate::model::{Change, Leak, LeakSource};

/// Runs git with the given arguments and returns its stdout.
pub type Runner<'a> = &'a dyn Fn(&[&str]) -> Result<Vec<u8>, GitError>;

/// Most unpushed commits scanned.
pub const MAX_SCANNED_COMMITS: usize = 50;

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
    /// Findings of flagged commits seen reaching a remote this session.
    pushed: Vec<Leak>,
    /// The last unpushed list git gave, newest first.
    last: Vec<String>,
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
    /// content findings in the staged and unstaged diffs.
    pub fn worktree(&self, git: Runner<'_>, changes: &[Change], content: bool) -> Vec<Leak> {
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
            for (cached, source) in [(true, LeakSource::Staged), (false, LeakSource::Unstaged)] {
                let mut args = vec!["diff"];
                if cached {
                    args.push("--cached");
                }
                args.extend(PATCH_ARGS);
                if let Ok(raw) = git(&args) {
                    out.extend(self.scan_files(parse::parse_added_lines(&raw), &source, false));
                }
            }
        }
        out
    }

    /// Findings in the unpushed commits (newest first), then in commits
    /// seen pushed this session. `unpushed` is `None` when git could not
    /// list them, and the last list stands in. A flagged commit that left
    /// the list was either pushed, and is kept as `Pushed`, or rewritten
    /// away, and is dropped; while git cannot tell which, it stays.
    pub fn commits(&self, git: Runner<'_>, unpushed: Option<&[String]>) -> Vec<Leak> {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(u) = unpushed {
            cache.last = u.to_vec();
        }
        let unpushed = cache.last.clone();
        let new: Vec<&str> = unpushed
            .iter()
            .map(String::as_str)
            .filter(|o| !cache.commits.contains_key(*o))
            .collect();
        if !new.is_empty() {
            let mut args = vec!["log", "--no-walk=unsorted", "-p", "--format=%x00%H"];
            args.extend(PATCH_ARGS);
            args.extend(&new);
            args.push("--");
            if let Ok(raw) = git(&args) {
                for (oid, patch) in parse::split_commit_patches(&raw) {
                    let source = LeakSource::Commit(oid.clone());
                    let found = self.scan_files(parse::parse_added_lines(patch), &source, true);
                    cache.commits.insert(oid, found);
                }
                // Commits without a patch, such as merges, have nothing to scan.
                for oid in new {
                    cache.commits.entry(oid.to_string()).or_default();
                }
            }
        }
        let current: HashSet<&str> = unpushed.iter().map(String::as_str).collect();
        let mut gone: Vec<String> = cache
            .commits
            .keys()
            .filter(|o| !current.contains(o.as_str()))
            .cloned()
            .collect();
        gone.sort();
        for oid in &gone {
            if cache.commits.get(oid).is_some_and(Vec::is_empty) {
                cache.commits.remove(oid);
                continue;
            }
            // Unknown for now: keep it, and ask again next refresh.
            let Ok(remotes) = git(&["branch", "-r", "--contains", oid]) else {
                continue;
            };
            let found = cache.commits.remove(oid).unwrap_or_default();
            if !remotes.trim_ascii().is_empty() {
                cache.pushed.extend(found.into_iter().map(|l| Leak {
                    source: LeakSource::Pushed(oid.clone()),
                    ..l
                }));
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

    /// Answers `log` with the patches of the oids asked for, and `branch -r
    /// --contains` with a branch for oids in `remote` (or an error while
    /// `branch_fails`); records every call.
    #[derive(Default)]
    struct Fake {
        patches: HashMap<String, String>,
        remote: Vec<String>,
        branch_fails: Cell<bool>,
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
            self.calls.borrow_mut().push(args.join(" "));
            if args[0] == "branch" && self.branch_fails.get() {
                return Err(GitError("git branch timed out".into()));
            }
            let out: String = match args[0] {
                "log" => args
                    .iter()
                    .filter_map(|a| self.patches.get(*a))
                    .cloned()
                    .collect(),
                "branch" if self.remote.iter().any(|o| args.contains(&o.as_str())) => {
                    "  origin/main\n".into()
                }
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
        let first = s.commits(&run, Some(&oids(&["c2", "c1"])));
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
        assert_eq!(s.commits(&run, Some(&oids(&["c2", "c1"]))), first);
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
        s.commits(&run, Some(&oids(&["c1"])));
        let after = s.commits(&run, Some(&[]));
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].source, LeakSource::Pushed("c1".into()));
        assert_eq!(s.commits(&run, Some(&[])), after, "kept for the session");
    }

    #[test]
    fn a_failed_push_check_keeps_the_finding_and_retries() {
        let mut fake = Fake::with(&[("c1", patch("c1", "deploy.txt", &key()))]);
        fake.remote = oids(&["c1"]);
        let s = LeakScanner::new(LeakScanConfig::default());
        let run = |a: &[&str]| fake.run(a);
        s.commits(&run, Some(&oids(&["c1"])));
        fake.branch_fails.set(true);
        let kept = s.commits(&run, Some(&[]));
        assert_eq!(kept.len(), 1, "a failed check must not drop the finding");
        assert_eq!(kept[0].source, LeakSource::Commit("c1".into()));
        fake.branch_fails.set(false);
        let after = s.commits(&run, Some(&[]));
        assert_eq!(after[0].source, LeakSource::Pushed("c1".into()));
        assert_eq!(fake.calls_to("branch").len(), 2, "checked again");
    }

    #[test]
    fn an_unknown_range_keeps_the_last_findings() {
        let fake = Fake::with(&[("c1", patch("c1", "deploy.txt", &key()))]);
        let s = LeakScanner::new(LeakScanConfig::default());
        let run = |a: &[&str]| fake.run(a);
        let first = s.commits(&run, Some(&oids(&["c1"])));
        assert_eq!(s.commits(&run, None), first);
        assert!(fake.calls_to("branch").is_empty(), "nothing left the list");
    }

    #[test]
    fn a_rewritten_commit_is_dropped_and_clean_ones_need_no_check() {
        let fake = Fake::with(&[
            ("c1", patch("c1", "deploy.txt", &key())),
            ("c2", patch("c2", "a.txt", "fine")),
        ]);
        let s = LeakScanner::new(LeakScanConfig::default());
        let run = |a: &[&str]| fake.run(a);
        s.commits(&run, Some(&oids(&["c2", "c1"])));
        assert!(s.commits(&run, Some(&[])).is_empty());
        assert_eq!(fake.calls_to("branch"), vec!["branch -r --contains c1"]);
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
        let found = s.worktree(&run, &changes, true);
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
            s.worktree(&run, &changes, false)
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
        assert!(s.commits(&run, Some(&oids(&["c1"]))).is_empty());
        assert!(s.untracked_file("fixtures/k.txt", &key()).is_empty());
        assert!(
            s.worktree(&run, &[change("fixtures/.env", '?', '?')], false)
                .is_empty()
        );
        assert_eq!(s.untracked_file("k.txt", &key()).len(), 1);
    }
}
