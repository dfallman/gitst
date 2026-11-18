//! Write ops, run as plain git: the user's own config applies, and the
//! index may be locked (spec §7.2, §7.3).

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::cli::{CliBackend, Run, plain_command, run_bounded};
use super::hunk;
use crate::write::{
    CommitPlan, PushPlan, REFUSE_DETACHED, REFUSE_NO_REMOTE, REFUSE_UNBORN, ShownHunk, WriteError,
    WriteOp, WriteOutcome, amend_message, classify_commit, classify_pull, classify_push, lock_held,
};

/// The tree of an empty index.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Most stdout kept from one write op.
const MAX_WRITE_OUTPUT: usize = 64 << 20;
/// Times a held `index.lock` is waited out, after the first attempt.
const LOCK_RETRIES: usize = 3;
const LOCK_WAIT: Duration = Duration::from_millis(500);
const BUSY: &str = "repository is busy (index.lock held), try again";
const STAGED_MOVED: &str = "staged changes moved, review again";
const HEAD_MOVED: &str = "HEAD moved, review again";
/// The time limit of the editor path's checks, as for other write ops.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// `run_write`'s summary when the time limit stopped git.
const TIMED_OUT: &str = "timed out";
/// The hunk shown is no longer in the file's diff, or no longer applies.
const STALE_HUNK: &str = "file changed, refreshed";

fn failure(summary: impl Into<String>, output: impl Into<String>) -> WriteError {
    WriteError {
        summary: summary.into(),
        output: output.into(),
    }
}

fn done(summary: String) -> Result<WriteOutcome, WriteError> {
    Ok(WriteOutcome::Done {
        summary,
        warning: None,
    })
}

impl CliBackend {
    /// A git command for a write op: the user's config applies, and the
    /// index lock is taken. It still never prompts and reads no stdin.
    pub(crate) fn write_command(&self) -> Command {
        plain_command(&self.repo.root)
    }

    /// Returns stdout. On failure the summary is the first stderr line and
    /// the output is stdout and stderr. A held `index.lock` is waited out
    /// up to three times, 500 ms apart, and never removed.
    pub(crate) fn run_write(
        &self,
        args: &[&str],
        input: Option<&[u8]>,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>, WriteError> {
        let mut retries = 0;
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(failure("cancelled", ""));
            }
            let mut cmd = self.write_command();
            cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
            let run = run_bounded(
                &mut cmd,
                timeout,
                Some(cancel),
                MAX_WRITE_OUTPUT,
                true,
                input,
            )
            .map_err(|e| failure(format!("git: {e}"), ""))?;
            let (stdout, stderr) = match run {
                Run::Exited {
                    status,
                    stdout,
                    stderr,
                } => {
                    if status.success() {
                        return Ok(stdout);
                    }
                    (stdout, String::from_utf8_lossy(&stderr).into_owned())
                }
                Run::TimedOut => return Err(failure(TIMED_OUT, "")),
                Run::Cancelled => return Err(failure("cancelled", "")),
                Run::TooLarge => return Err(failure("too much output", "")),
            };
            if lock_held(&stderr) {
                if retries < LOCK_RETRIES {
                    retries += 1;
                    std::thread::sleep(LOCK_WAIT);
                    continue;
                }
                return Err(failure(BUSY, stderr));
            }
            let summary = stderr
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("git failed")
                .trim_start_matches("fatal: ")
                .to_string();
            let output = format!("{}{stderr}", String::from_utf8_lossy(&stdout));
            return Err(failure(summary, output));
        }
    }

    pub(crate) fn run_op(
        &self,
        op: &WriteOp,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<WriteOutcome, WriteError> {
        let git = |args: &[&str]| self.run_write(args, None, timeout, cancel);
        match op {
            WriteOp::Stage { path } => {
                git(&["add", "--", path])?;
                done(format!("staged {path}"))
            }
            WriteOp::Unstage { path, unborn } => {
                if *unborn {
                    git(&["rm", "--cached", "--quiet", "--", path])?;
                } else {
                    git(&["restore", "--staged", "--", path])?;
                }
                done(format!("unstaged {path}"))
            }
            WriteOp::StageAll => {
                git(&["add", "-A"])?;
                done("staged all".into())
            }
            WriteOp::UnstageAll { unborn } => {
                if *unborn {
                    git(&["rm", "-r", "--cached", "--quiet", "--", "."])?;
                } else {
                    git(&["reset", "--quiet"])?;
                }
                done("unstaged all".into())
            }
            WriteOp::ApplyHunk {
                path,
                hunk,
                reverse,
            } => self.apply_hunk(path, hunk, *reverse, timeout, cancel),
            WriteOp::PrepareCommit { stage_all, amend } => {
                self.prepare_commit(*stage_all, *amend, timeout, cancel)
            }
            WriteOp::Commit {
                message,
                amend,
                tree,
                head,
            } => self.commit(message, *amend, tree, head.as_deref(), timeout, cancel),
            WriteOp::PreparePush => self.prepare_push(timeout, cancel),
            WriteOp::Push(plan) => self.push(plan, timeout, cancel),
            WriteOp::Pull => self.pull(timeout, cancel),
            WriteOp::Stash { message } => self.stash(message.as_deref(), timeout, cancel),
            WriteOp::StashPop { index, oid } => {
                self.stash_restore("pop", *index, oid, timeout, cancel)
            }
            WriteOp::StashApply { index, oid } => {
                self.stash_restore("apply", *index, oid, timeout, cancel)
            }
            WriteOp::StashDrop { index, oid } => {
                self.check_stash(*index, oid, timeout, cancel)?;
                git(&["stash", "drop", &format!("stash@{{{index}}}")])?;
                done(format!("dropped stash@{{{index}}}"))
            }
        }
    }

    fn text(
        &self,
        args: &[&str],
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<String, WriteError> {
        let out = self.run_write(args, None, timeout, cancel)?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    /// HEAD's tree, or `None` on an unborn branch.
    fn head_tree(&self, timeout: Duration, cancel: &AtomicBool) -> Option<String> {
        self.text(
            &["rev-parse", "--verify", "-q", "HEAD^{tree}"],
            timeout,
            cancel,
        )
        .ok()
        .filter(|t| !t.is_empty())
    }

    /// HEAD's full oid, or `None` on an unborn branch.
    fn head_oid(
        &self,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<Option<String>, WriteError> {
        match self.text(&["rev-parse", "--verify", "-q", "HEAD"], timeout, cancel) {
            Ok(oid) => Ok(Some(oid).filter(|o| !o.is_empty())),
            // `-q` makes an unborn HEAD a silent exit 1; anything else
            // (busy, timeout, cancel, spawn) is passed on as it is.
            Err(e) if e.summary == "git failed" && e.output.is_empty() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Stops unless the index and HEAD are still what the confirm pinned
    /// (spec §7.5, step 5).
    fn check_pin(
        &self,
        tree: &str,
        head: Option<&str>,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<(), WriteError> {
        if self.text(&["write-tree"], timeout, cancel)? != tree {
            return Err(failure(STAGED_MOVED, ""));
        }
        if self.head_oid(timeout, cancel)?.as_deref() != head {
            return Err(failure(HEAD_MOVED, ""));
        }
        Ok(())
    }

    /// The summary of the commit just made, warning when its tree isn't
    /// the pinned one (spec §7.5, step 7).
    fn committed(
        &self,
        amend: bool,
        tree: &str,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<WriteOutcome, WriteError> {
        let short = self.text(&["rev-parse", "--short", "HEAD"], timeout, cancel)?;
        let warning = (self.text(&["rev-parse", "HEAD^{tree}"], timeout, cancel)? != tree)
            .then(|| "commit includes changes staged after review".to_string());
        Ok(WriteOutcome::Done {
            summary: format!("{} {short}", if amend { "amended" } else { "committed" }),
            warning,
        })
    }

    /// For a commit the editor path is about to make: whether the index and
    /// HEAD are still what the confirm pinned.
    pub fn commit_pin_holds(&self, tree: &str, head: Option<&str>) -> Result<(), WriteError> {
        self.check_pin(tree, head, CHECK_TIMEOUT, &AtomicBool::new(false))
    }

    /// For a commit the editor path made: its summary, warning when its
    /// tree isn't the pinned one.
    pub fn commit_outcome(&self, amend: bool, tree: &str) -> Result<WriteOutcome, WriteError> {
        self.committed(amend, tree, CHECK_TIMEOUT, &AtomicBool::new(false))
    }

    /// Stages the hunk `shown`, or unstages it (`reverse`), cut from a
    /// fresh read of the file's diff: the shown one can't be applied
    /// (spec §6.2). The read is plain git, like `git add`: with the
    /// repository's own filters, which the guarded view leaves out, a
    /// hunk that differs from the one shown is never staged.
    fn apply_hunk(
        &self,
        path: &str,
        shown: &ShownHunk,
        reverse: bool,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<WriteOutcome, WriteError> {
        let mut diff = vec!["diff"];
        if reverse {
            diff.push("--cached");
        }
        // `a/` and `b/` whatever `diff.noprefix` or `diff.mnemonicPrefix`
        // say, as `git apply` needs.
        diff.extend([
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--",
            path,
        ]);
        let raw = self.run_write(&diff, None, timeout, cancel).map_err(|e| {
            // Larger than any diff the view shows: not the one shown.
            if e.summary == "too much output" {
                failure(STALE_HUNK, "")
            } else {
                e
            }
        })?;
        let patch = hunk::split(&raw)
            .and_then(|diff| hunk::patch_for(&diff, shown))
            .ok_or_else(|| failure(STALE_HUNK, ""))?;
        let mut args = vec!["apply", "--cached", "--whitespace=nowarn"];
        if reverse {
            args.push("--reverse");
        }
        args.push("-");
        self.run_write(&args, Some(&patch), timeout, cancel)
            .map_err(|e| {
                // Lock, timeout, cancel, and spawn failures keep their
                // summary; git refusing the patch means the index moved.
                if e.summary == BUSY || e.output.is_empty() {
                    e
                } else {
                    failure(STALE_HUNK, e.output)
                }
            })?;
        done(
            if reverse {
                "unstaged hunk"
            } else {
                "staged hunk"
            }
            .into(),
        )
    }

    /// Pins the index tree and scans it fresh (spec §7.5, steps 1 to 3).
    fn prepare_commit(
        &self,
        stage_all: bool,
        amend: bool,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<WriteOutcome, WriteError> {
        if amend && stage_all {
            return Err(failure("amend never stages all", ""));
        }
        let head_tree = self.head_tree(timeout, cancel);
        if amend && head_tree.is_none() {
            return Err(failure("no commit to amend", ""));
        }
        if stage_all {
            self.run_write(&["add", "-A"], None, timeout, cancel)?;
        }
        let head = self.head_oid(timeout, cancel)?;
        let tree = self.text(&["write-tree"], timeout, cancel)?;
        let unchanged = match &head_tree {
            Some(h) => *h == tree,
            None => tree == EMPTY_TREE,
        };
        if unchanged && !amend {
            return Err(failure("nothing to commit", ""));
        }
        let (findings, coverage) = self.scan_staged();
        Ok(WriteOutcome::CommitReady(CommitPlan {
            tree,
            head,
            findings,
            coverage,
            amend,
        }))
    }

    /// Re-checks the pin, commits with the message on stdin, and checks the
    /// result (spec §7.5, steps 5 to 7). `message` is the subject when amending.
    fn commit(
        &self,
        message: &str,
        amend: bool,
        tree: &str,
        head: Option<&str>,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<WriteOutcome, WriteError> {
        self.check_pin(tree, head, timeout, cancel)?;
        let full = if amend {
            // The raw commit object, not `log`: user config such as
            // `log.showSignature` can't add text to it.
            let raw = self.text_raw(&["cat-file", "commit", "HEAD"], timeout, cancel)?;
            let previous = raw.split_once("\n\n").map_or("", |(_, body)| body);
            amend_message(previous, message)
        } else {
            message.to_string()
        };
        let mut args = vec!["commit", "--cleanup=whitespace", "-F", "-"];
        if amend {
            args.push("--amend");
        }
        self.run_write(&args, Some(full.as_bytes()), timeout, cancel)
            .map_err(|e| {
                if e.summary == TIMED_OUT {
                    return failure("commit failed: timeout", e.output);
                }
                // Lock, cancel, and spawn failures keep the summary
                // `run_write` chose; only git's own non-zero exit is classified.
                if e.summary == BUSY || e.output.is_empty() {
                    return e;
                }
                WriteError {
                    summary: classify_commit(&self.with_hook_hint(&e.output)),
                    output: e.output,
                }
            })?;
        self.committed(amend, tree, timeout, cancel)
    }

    /// Records HEAD and the push target, then counts and scans exactly the
    /// commits that would go out (spec §6.4, §7.5).
    fn prepare_push(
        &self,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<WriteOutcome, WriteError> {
        // Not `--short`: with a tag of the same name it says `heads/<name>`.
        let full = self
            .text(&["symbolic-ref", "-q", "HEAD"], timeout, cancel)
            .map_err(|e| {
                // `-q` makes a detached HEAD a silent exit 1.
                if e.summary == "git failed" && e.output.is_empty() {
                    failure(REFUSE_DETACHED, "")
                } else {
                    e
                }
            })?;
        let local = full
            .strip_prefix("refs/heads/")
            .ok_or_else(|| failure(REFUSE_DETACHED, ""))?
            .to_string();
        let oid = self
            .text(&["rev-parse", "--verify", "-q", "HEAD"], timeout, cancel)
            .map_err(|e| {
                if e.output.is_empty() && e.summary == "git failed" {
                    failure(REFUSE_UNBORN, "")
                } else {
                    e
                }
            })?;
        let (remote, branch, tracking) = self.push_target(&local, timeout, cancel)?;
        let upstream = self.text(
            &[
                "for-each-ref",
                "--format=%(upstream)",
                &format!("refs/heads/{local}"),
            ],
            timeout,
            cancel,
        )?;
        let have_tracking = self
            .text(&["rev-parse", "--verify", "-q", &tracking], timeout, cancel)
            .is_ok_and(|t| !t.is_empty());
        let listed = if have_tracking {
            self.text(&["rev-list", &oid, "--not", &tracking], timeout, cancel)?
        } else {
            self.text(&["rev-list", &oid, "--not", "--remotes"], timeout, cancel)?
        };
        let oids: Vec<String> = listed.lines().map(str::to_string).collect();
        if oids.is_empty() {
            return Err(failure("nothing to push", ""));
        }
        let (findings, coverage) = self.scan_range(&oids);
        Ok(WriteOutcome::PushReady(PushPlan {
            local,
            remote,
            branch,
            oid,
            count: oids.len(),
            set_upstream: upstream.is_empty(),
            findings,
            coverage,
        }))
    }

    /// The remote, the remote branch, and the remote-tracking ref a push of
    /// `local` goes to, as git's own push config says.
    fn push_target(
        &self,
        local: &str,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<(String, String, String), WriteError> {
        let refs = self.text_raw(
            &[
                "for-each-ref",
                "--format=%(push:remotename)%00%(push)%00%(push:remoteref)",
                &format!("refs/heads/{local}"),
            ],
            timeout,
            cancel,
        )?;
        let mut parts = refs.trim_end_matches('\n').splitn(3, '\0');
        let remote = parts.next().unwrap_or("");
        let tracking = parts.next().unwrap_or("");
        let remote_ref = parts.next().unwrap_or("");
        if !remote_ref.is_empty() && !remote_ref.starts_with("refs/heads/") {
            return Err(failure(
                "push target is not a branch, push from a shell",
                "",
            ));
        }
        // The remote ref, when set, is authoritative for the branch name.
        let ref_branch = remote_ref
            .strip_prefix("refs/heads/")
            .filter(|b| !b.is_empty());
        let counted = |remote: &str, branch: &str| {
            if tracking.is_empty() {
                format!("refs/remotes/{remote}/{branch}")
            } else {
                tracking.to_string()
            }
        };
        if !remote.is_empty() {
            if let Some(b) = ref_branch {
                return Ok((remote.to_string(), b.to_string(), counted(remote, b)));
            }
            if let Some(b) = tracking
                .strip_prefix(&format!("refs/remotes/{remote}/"))
                .filter(|b| !b.is_empty())
            {
                return Ok((remote.to_string(), b.to_string(), tracking.to_string()));
            }
            // `%(push)` is empty whenever `simple` or `upstream` can't resolve
            // without a matching upstream (a new branch, say), so the config
            // decides instead.
            let config = |key: &str| {
                self.text(&["config", "--get", key], timeout, cancel)
                    .unwrap_or_default()
            };
            let mode = config("push.default");
            let merge = config(&format!("branch.{local}.merge"));
            let differs = !merge.is_empty() && merge != format!("refs/heads/{local}");
            if mode == "nothing" || mode == "upstream" || mode == "tracking" || differs {
                return Err(failure("no push target configured, push from a shell", ""));
            }
            let tracking = format!("refs/remotes/{remote}/{local}");
            return Ok((remote.to_string(), local.to_string(), tracking));
        }
        let remotes = self.text(&["remote"], timeout, cancel)?;
        let names: Vec<&str> = remotes.lines().collect();
        let remote = match names.as_slice() {
            [] => return Err(failure(REFUSE_NO_REMOTE, "")),
            [only] => (*only).to_string(),
            many if many.contains(&"origin") => "origin".to_string(),
            _ => return Err(failure("several remotes, push from a shell", "")),
        };
        let branch = ref_branch.unwrap_or(local);
        let tracking = counted(&remote, branch);
        Ok((remote, branch.to_string(), tracking))
    }

    /// Pushes the confirmed oid, never HEAD, with an explicit refspec.
    fn push(
        &self,
        plan: &PushPlan,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<WriteOutcome, WriteError> {
        // No `-u`: with an oid as the source git has no local branch to track.
        let refspec = format!("{}:refs/heads/{}", plan.oid, plan.branch);
        let args = ["push", plan.remote.as_str(), refspec.as_str()];
        self.run_write(&args, None, timeout, cancel).map_err(|e| {
            if e.summary == TIMED_OUT {
                return failure("push failed: timeout", e.output);
            }
            if e.summary == BUSY || e.output.is_empty() {
                return e;
            }
            let tracking = format!("refs/remotes/{}/{}", plan.remote, plan.branch);
            let behind = self
                .text(
                    &["rev-list", "--count", &format!("{}..{tracking}", plan.oid)],
                    timeout,
                    cancel,
                )
                .ok()
                .and_then(|n| n.parse::<u32>().ok())
                .filter(|n| *n > 0);
            WriteError {
                summary: classify_push(&e.output, behind),
                output: e.output,
            }
        })?;
        let mut warnings = Vec::new();
        if plan.set_upstream {
            let key = |k: &str| format!("branch.{}.{k}", plan.local);
            let merge = format!("refs/heads/{}", plan.branch);
            let tracked = self
                .run_write(
                    &["config", &key("remote"), &plan.remote],
                    None,
                    timeout,
                    cancel,
                )
                .and_then(|_| {
                    self.run_write(&["config", &key("merge"), &merge], None, timeout, cancel)
                });
            if tracked.is_err() {
                // Best effort: don't leave `remote` set without `merge`.
                let _ = self.run_write(
                    &["config", "--unset", &key("remote")],
                    None,
                    timeout,
                    cancel,
                );
                warnings.push("pushed, but tracking not set".to_string());
            }
        }
        let short = self
            .text(&["rev-parse", "--short", &plan.oid], timeout, cancel)
            .unwrap_or_else(|_| plan.oid.chars().take(7).collect());
        let newer = self
            .text(
                &[
                    "rev-list",
                    "--count",
                    &format!("{}..refs/heads/{}", plan.oid, plan.local),
                ],
                timeout,
                cancel,
            )
            .ok()
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|n| *n > 0);
        if let Some(n) = newer {
            warnings.push(format!(
                "{n} newer commit{} not pushed",
                if n == 1 { "" } else { "s" }
            ));
        }
        Ok(WriteOutcome::Done {
            summary: format!("pushed {short} to {}/{}", plan.remote, plan.branch),
            warning: (!warnings.is_empty()).then(|| warnings.join(" · ")),
        })
    }

    fn pull(&self, timeout: Duration, cancel: &AtomicBool) -> Result<WriteOutcome, WriteError> {
        // `--no-rebase`: an older git with `pull.rebase` set would rebase.
        self.run_write(&["pull", "--ff-only", "--no-rebase"], None, timeout, cancel)
            .map_err(|e| {
                if e.summary == TIMED_OUT {
                    return failure("pull failed: timeout", e.output);
                }
                if e.summary == BUSY || e.output.is_empty() {
                    return e;
                }
                WriteError {
                    summary: classify_pull(&e.output),
                    output: e.output,
                }
            })?;
        done("pulled".into())
    }

    fn stash(
        &self,
        message: Option<&str>,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<WriteOutcome, WriteError> {
        // No paths are passed, and with literal pathspecs git leaves the
        // untracked files in the work tree after saving them.
        let mut args = vec![
            "--no-literal-pathspecs",
            "stash",
            "push",
            "--include-untracked",
        ];
        if let Some(m) = message.filter(|m| !m.is_empty()) {
            args.extend(["-m", m]);
        }
        let out = self.run_write(&args, None, timeout, cancel)?;
        // Git may exit 0 and only say so.
        if String::from_utf8_lossy(&out).contains("No local changes to save") {
            return Err(failure("nothing to stash", ""));
        }
        done("stashed".into())
    }

    /// Fails with `stashes changed, refreshed` unless `stash@{index}` is
    /// still the stash the user saw.
    fn check_stash(
        &self,
        index: usize,
        oid: &str,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<(), WriteError> {
        let current = match self.text(
            &["rev-parse", "--verify", "-q", &format!("stash@{{{index}}}")],
            timeout,
            cancel,
        ) {
            Ok(c) => c,
            // `-q` makes a missing stash a silent exit 1; anything else
            // (busy, timeout, cancel, spawn) is passed on as it is.
            Err(e) if e.summary == "git failed" && e.output.is_empty() => String::new(),
            Err(e) => return Err(e),
        };
        if !oid.is_empty() && current == oid {
            Ok(())
        } else {
            Err(failure("stashes changed, refreshed", ""))
        }
    }

    /// `pop` or `apply`, restoring the index; without it, once, if git
    /// can't restore it.
    fn stash_restore(
        &self,
        verb: &str,
        index: usize,
        oid: &str,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<WriteOutcome, WriteError> {
        self.check_stash(index, oid, timeout, cancel)?;
        let reference = format!("stash@{{{index}}}");
        let past = if verb == "pop" { "popped" } else { "applied" };
        let first = self.run_write(
            &["stash", verb, "--index", &reference],
            None,
            timeout,
            cancel,
        );
        let warning = match first {
            Ok(_) => None,
            Err(e)
                if {
                    let o = e.output.to_lowercase();
                    o.contains("could not restore index")
                        || o.contains("conflicts in index")
                        || o.contains("try without --index")
                } =>
            {
                self.run_write(&["stash", verb, &reference], None, timeout, cancel)?;
                Some("index not restored".to_string())
            }
            Err(e) => return Err(e),
        };
        Ok(WriteOutcome::Done {
            summary: format!("{past} {reference}"),
            warning,
        })
    }

    /// Git prints nothing of its own when a hook rejects a commit, only the
    /// hook's output. So when git itself said nothing fatal, name the hook
    /// that exists, so `classify_commit` can.
    fn with_hook_hint(&self, output: &str) -> String {
        if output.contains("fatal:") {
            return output.to_string();
        }
        for hook in ["pre-commit", "commit-msg"] {
            let path = self
                .write_command()
                .args(["rev-parse", "--git-path", &format!("hooks/{hook}")])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .map(|p| self.repo.root.join(p));
            if path.is_some_and(|p| p.is_file()) {
                return format!("{hook} hook failed\n{output}");
            }
        }
        output.to_string()
    }

    fn text_raw(
        &self,
        args: &[&str],
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<String, WriteError> {
        let out = self.run_write(args, None, timeout, cancel)?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}
