//! Pure parsers for git's machine-readable output.

use crate::model::{
    Branch, Change, Commit, Counts, DiffKind, DiffLine, ReflogEntry, Stash, TagInfo,
};

pub const LOG_FORMAT: &str = "%H%x1f%h%x1f%P%x1f%at%x1f%an%x1f%D%x1f%s%x1e";
pub const BRANCH_FORMAT: &str = "%(refname:short)%1f%(upstream:short)%1f%(upstream:track,nobracket)%1f%(committerdate:unix)%1f%(HEAD)%1e";
pub const STASH_FORMAT: &str = "%gd%x1f%ct%x1f%gs%x1e";
pub const REFLOG_FORMAT: &str = "%h%x1f%gd%x1f%gs%x1e";

/// `git log -g` output format for several refs at once: the full hash,
/// and the selector with the full refname, as in
/// `refs/remotes/origin/main@{1790484333}`.
pub const REFLOGS_FORMAT: &str = "%H%x1f%gD%x1f%gs%x1e";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusHeader {
    pub oid: Option<String>,
    /// `None` when HEAD is detached.
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub stash: usize,
}

/// Parses `git status --porcelain=v2 -z --branch --show-stash`.
pub fn parse_status(raw: &[u8]) -> (StatusHeader, Vec<Change>) {
    let mut header = StatusHeader::default();
    let mut changes = Vec::new();
    let mut fields = raw
        .split(|b| *b == 0)
        .map(|f| String::from_utf8_lossy(f).into_owned());

    while let Some(field) = fields.next() {
        if let Some(h) = field.strip_prefix("# ") {
            parse_status_header(h, &mut header);
        } else if let Some(rest) = field.strip_prefix("1 ") {
            if let Some(c) = ordinary_entry(rest, 6, None) {
                changes.push(c);
            }
        } else if let Some(rest) = field.strip_prefix("2 ") {
            let orig = fields.next();
            if let Some(c) = ordinary_entry(rest, 7, orig) {
                changes.push(c);
            }
        } else if let Some(rest) = field.strip_prefix("u ") {
            if let Some(c) = ordinary_entry(rest, 8, None) {
                changes.push(c);
            }
        } else if let Some(path) = field.strip_prefix("? ") {
            changes.push(Change {
                path: path.to_string(),
                orig_path: None,
                x: '?',
                y: '?',
                counts: Counts::Unknown,
                modified: None,
            });
        }
    }
    (header, changes)
}

fn parse_status_header(h: &str, header: &mut StatusHeader) {
    let (key, value) = h.split_once(' ').unwrap_or((h, ""));
    match key {
        "branch.oid" if value != "(initial)" => header.oid = Some(value.to_string()),
        "branch.head" if value != "(detached)" => header.head = Some(value.to_string()),
        "branch.upstream" => header.upstream = Some(value.to_string()),
        "branch.ab" => {
            for part in value.split(' ') {
                if let Some(n) = part.strip_prefix('+') {
                    header.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    header.behind = n.parse().unwrap_or(0);
                }
            }
        }
        "stash" => header.stash = value.parse().unwrap_or(0),
        _ => {}
    }
}

/// Parses the part of a `1`/`2`/`u` record after its type letter: `XY`,
/// then `skip` space-separated fields, then the path (which may contain spaces).
fn ordinary_entry(rest: &str, skip: usize, orig_path: Option<String>) -> Option<Change> {
    let mut parts = rest.splitn(skip + 2, ' ');
    let xy: Vec<char> = parts.next()?.chars().collect();
    let path = parts.nth(skip)?;
    let norm = |c: char| if c == '.' { ' ' } else { c };
    Some(Change {
        path: path.to_string(),
        orig_path,
        x: norm(*xy.first()?),
        y: norm(*xy.get(1)?),
        counts: Counts::Unknown,
        modified: None,
    })
}

pub type NumstatEntry = (String, Counts);

/// Lists every config entry with the file it came from. Git prints keys
/// with the section and name lowercased, and the subsection as written.
pub const CONFIG_LIST: [&str; 5] = ["config", "--list", "--show-scope", "--includes", "-z"];

/// One entry of `git config --list --show-scope -z`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigEntry {
    /// `system`, `global`, `local`, `worktree`, or `command`.
    pub scope: String,
    pub key: String,
    /// `true` for a key written without `=`, which git reads as true.
    pub value: String,
}

/// Parses `git config --list --show-scope -z`, in the order git applies
/// it: the last entry for a key is the one in force.
pub fn parse_config(raw: &[u8]) -> Vec<ConfigEntry> {
    let mut fields = raw.split(|b| *b == 0);
    let mut out = Vec::new();
    while let (Some(scope), Some(entry)) = (fields.next(), fields.next()) {
        let entry = String::from_utf8_lossy(entry);
        let (key, value) = entry.split_once('\n').unwrap_or((&entry, "true"));
        out.push(ConfigEntry {
            scope: String::from_utf8_lossy(scope).into_owned(),
            key: key.to_string(),
            value: value.to_string(),
        });
    }
    out
}

/// What a snapshot needs from the config.
#[derive(Debug, PartialEq, Eq)]
pub struct RepoConfig {
    /// `--untracked-files=…` for `git status`.
    pub untracked_mode: &'static str,
    pub has_remote: bool,
}

/// Untracked files follow `status.showUntrackedFiles`, but are listed one
/// by one (`all`) when it is not set.
pub fn parse_repo_config(config: &[ConfigEntry]) -> RepoConfig {
    let mut mode = "--untracked-files=all";
    let mut has_remote = false;
    for ConfigEntry { key, value, .. } in config {
        if key == "status.showuntrackedfiles" {
            mode = match value.trim().to_ascii_lowercase().as_str() {
                "no" | "false" | "off" | "0" => "--untracked-files=no",
                "normal" | "true" | "on" | "1" => "--untracked-files=normal",
                _ => "--untracked-files=all",
            };
        } else if key.starts_with("remote.") && key.ends_with(".url") {
            has_remote = true;
        }
    }
    RepoConfig {
        untracked_mode: mode,
        has_remote,
    }
}

/// Splits `\x1e`-terminated records into `\x1f`-separated fields.
fn records(raw: &[u8]) -> impl Iterator<Item = Vec<String>> + '_ {
    raw.split(|b| *b == 0x1e).filter_map(|rec| {
        let rec = String::from_utf8_lossy(rec);
        let rec = rec.trim_start_matches('\n');
        if rec.is_empty() {
            None
        } else {
            Some(rec.split('\x1f').map(str::to_string).collect())
        }
    })
}

/// Number inside the first `{…}`, as in `stash@{3}` or `HEAD@{1790484333}`.
fn braced_number(s: &str) -> Option<i64> {
    let open = s.find('{')?;
    let close = s[open..].find('}')? + open;
    s[open + 1..close].parse().ok()
}

/// Parses `git diff --numstat -z`. Renames report the new path.
pub fn parse_numstat(raw: &[u8]) -> Vec<NumstatEntry> {
    let mut out = Vec::new();
    let mut fields = raw
        .split(|b| *b == 0)
        .map(|f| String::from_utf8_lossy(f).into_owned());
    while let Some(field) = fields.next() {
        let mut parts = field.trim_start_matches('\n').splitn(3, '\t');
        let (Some(a), Some(r), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let path = if path.is_empty() {
            let _old = fields.next();
            match fields.next() {
                Some(new) => new,
                None => continue,
            }
        } else {
            path.to_string()
        };
        let counts = match (a.parse(), r.parse()) {
            (Ok(a), Ok(r)) => Counts::lines(a, r),
            _ => Counts::Binary,
        };
        out.push((path, counts));
    }
    out
}

/// Parses `git log --format=LOG_FORMAT`.
pub fn parse_log(raw: &[u8]) -> Vec<Commit> {
    records(raw)
        .filter(|f| f.len() >= 7)
        .map(|f| Commit {
            oid: f[0].clone(),
            short: f[1].clone(),
            parents: f[2].split_whitespace().count(),
            time: f[3].parse().unwrap_or(0),
            author: f[4].clone(),
            refs: f[5]
                .split(", ")
                .filter(|r| !r.is_empty() && *r != "HEAD")
                .map(|r| r.strip_prefix("HEAD -> ").unwrap_or(r).to_string())
                .collect(),
            subject: f[6..].join("\x1f"),
            unpushed: false,
        })
        .collect()
}

/// Parses `git for-each-ref --format=BRANCH_FORMAT refs/heads`.
pub fn parse_branches(raw: &[u8]) -> Vec<Branch> {
    records(raw)
        .filter(|f| f.len() >= 5)
        .map(|f| {
            let (mut ahead, mut behind) = (0, 0);
            for part in f[2].split(", ") {
                if let Some(n) = part.strip_prefix("ahead ") {
                    ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix("behind ") {
                    behind = n.parse().unwrap_or(0);
                }
            }
            Branch {
                name: f[0].clone(),
                upstream: (!f[1].is_empty()).then(|| f[1].clone()),
                ahead,
                behind,
                gone: f[2] == "gone",
                time: f[3].parse().unwrap_or(0),
                is_head: f[4] == "*",
            }
        })
        .collect()
}

/// Parses `git stash list --format=STASH_FORMAT`.
pub fn parse_stashes(raw: &[u8]) -> Vec<Stash> {
    records(raw)
        .filter(|f| f.len() >= 3)
        .map(|f| Stash {
            index: braced_number(&f[0]).unwrap_or(0) as usize,
            time: f[1].parse().unwrap_or(0),
            message: f[2..].join("\x1f"),
        })
        .collect()
}

/// Parses `git log -g --date=unix --format=REFLOGS_FORMAT --glob=…`, whose
/// entries name their ref. `short` holds the full hash.
pub fn parse_reflogs(raw: &[u8]) -> Vec<ReflogEntry> {
    records(raw)
        .filter(|f| f.len() >= 3)
        .filter_map(|f| {
            let (refname, _) = f[1].split_once("@{")?;
            Some(ReflogEntry {
                refname: refname.to_string(),
                short: f[0].clone(),
                time: braced_number(&f[1]).unwrap_or(0),
                message: f[2..].join("\x1f"),
            })
        })
        .collect()
}

/// Parses `git log -g --date=unix --format=REFLOG_FORMAT <refname>`.
pub fn parse_reflog(refname: &str, raw: &[u8]) -> Vec<ReflogEntry> {
    records(raw)
        .filter(|f| f.len() >= 3)
        .map(|f| ReflogEntry {
            refname: refname.to_string(),
            short: f[0].clone(),
            time: braced_number(&f[1]).unwrap_or(0),
            message: f[2..].join("\x1f"),
        })
        .collect()
}

/// Parses `git describe --tags --long` output such as `v0.1.9-4-gabc1234`.
pub fn parse_describe(s: &str) -> Option<TagInfo> {
    let mut parts = s.trim().rsplitn(3, '-');
    let hash = parts.next()?;
    let distance = parts.next()?.parse().ok()?;
    let name = parts.next()?;
    if !hash.starts_with('g') || name.is_empty() {
        return None;
    }
    Some(TagInfo {
        name: name.to_string(),
        distance,
    })
}

/// Parses unified diff output into display lines, dropping file headers.
pub fn parse_diff(raw: &[u8]) -> Vec<DiffLine> {
    let text = String::from_utf8_lossy(raw);
    let mut out = Vec::new();
    let mut in_header = false;
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with("diff ") {
            in_header = true;
            continue;
        }
        let (kind, body) = if line.starts_with("@@") {
            in_header = false;
            (DiffKind::Hunk, line)
        } else if in_header {
            if line.starts_with("Binary files") {
                (DiffKind::Meta, line)
            } else {
                continue;
            }
        } else if let Some(rest) = line.strip_prefix('+') {
            (DiffKind::Add, rest)
        } else if let Some(rest) = line.strip_prefix('-') {
            (DiffKind::Del, rest)
        } else if let Some(rest) = line.strip_prefix(' ') {
            (DiffKind::Context, rest)
        } else if line.starts_with('\\') {
            (DiffKind::Meta, line)
        } else {
            continue;
        };
        out.push(DiffLine {
            kind,
            text: body.replace('\t', "    "),
        });
    }
    out
}

/// A file in a `-U0` patch: the lines it adds, with their new line numbers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AddedFile {
    pub path: String,
    /// Created by the patch.
    pub new_file: bool,
    pub lines: Vec<(u32, String)>,
}

/// Parses `git diff` or `git log -p` output made with `-U0 --no-renames
/// --src-prefix=a/ --dst-prefix=b/`. Deleted files are left out.
pub fn parse_added_lines(raw: &[u8]) -> Vec<AddedFile> {
    let text = String::from_utf8_lossy(raw);
    let mut out = Vec::new();
    let mut cur: Option<AddedFile> = None;
    let mut in_header = false;
    let mut next = 0u32;
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with("diff --git ") {
            out.extend(cur.take().filter(|f| !f.path.is_empty()));
            cur = Some(AddedFile::default());
            in_header = true;
            continue;
        }
        let Some(file) = cur.as_mut() else {
            continue;
        };
        if line.starts_with("@@") {
            in_header = false;
            next = hunk_new_start(line);
        } else if in_header {
            if line.starts_with("new file mode") {
                file.new_file = true;
            } else if let Some(p) = line.strip_prefix("+++ ") {
                file.path = new_side_path(p).unwrap_or_default();
            } else if let Some((_, p)) = line
                .strip_prefix("Binary files ")
                .and_then(|r| r.strip_suffix(" differ"))
                .and_then(|r| r.rsplit_once(" and "))
            {
                file.path = new_side_path(p).unwrap_or_default();
            }
        } else if let Some(added) = line.strip_prefix('+') {
            file.lines.push((next, added.to_string()));
            next += 1;
        }
    }
    out.extend(cur.filter(|f| !f.path.is_empty()));
    out
}

/// The path in `b/<path>`; `None` for `/dev/null`. Git ends a path that
/// contains a space with a tab, and quotes one with special characters.
fn new_side_path(p: &str) -> Option<String> {
    let p = p.strip_suffix('\t').unwrap_or(p);
    let p = p
        .strip_prefix('"')
        .and_then(|q| q.strip_suffix('"'))
        .unwrap_or(p);
    p.strip_prefix("b/").map(str::to_string)
}

/// `c` in `@@ -a,b +c,d @@`.
fn hunk_new_start(line: &str) -> u32 {
    line.split(' ')
        .find_map(|part| part.strip_prefix('+'))
        .and_then(|n| n.split(',').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// Splits `git log --format=%x00%H -p` output into `(oid, patch)` pairs.
pub fn split_commit_patches(raw: &[u8]) -> Vec<(String, &[u8])> {
    raw.split(|b| *b == 0)
        .filter(|chunk| !chunk.is_empty())
        .filter_map(|chunk| {
            let end = chunk
                .iter()
                .position(|b| *b == b'\n')
                .unwrap_or(chunk.len());
            let oid = String::from_utf8_lossy(&chunk[..end]).trim().to_string();
            (!oid.is_empty()).then(|| (oid, &chunk[end..]))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &[u8] = b"# branch.oid ce2bc00e24bd\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0# stash 3\0\
1 .M N... 100644 100644 100644 aaa bbb src/app.rs\0\
1 A. N... 000000 100644 100644 000 ccc new file.rs\0\
2 R. N... 100644 100644 100644 d d R100 r.txt\0a.txt\0\
u UU N... 100644 100644 100644 100644 a b c conflict.rs\0\
? notes/\xe6\x97\xa5\xe6\x9c\xac.md\0";

    #[test]
    fn header_fields() {
        let (h, _) = parse_status(S);
        assert_eq!(h.oid.as_deref(), Some("ce2bc00e24bd"));
        assert_eq!(h.head.as_deref(), Some("main"));
        assert_eq!(h.upstream.as_deref(), Some("origin/main"));
        assert_eq!((h.ahead, h.behind, h.stash), (2, 1, 3));
    }

    #[test]
    fn entries() {
        let (_, c) = parse_status(S);
        assert_eq!(c.len(), 5);
        assert_eq!(
            (c[0].path.as_str(), c[0].x, c[0].y),
            ("src/app.rs", ' ', 'M')
        );
        assert_eq!(c[1].path, "new file.rs");
        assert!(c[1].staged());
        assert_eq!(c[2].path, "r.txt");
        assert_eq!(c[2].orig_path.as_deref(), Some("a.txt"));
        assert!(c[3].conflicted());
        assert!(c[4].untracked());
        assert_eq!(c[4].path, "notes/日本.md");
    }

    #[test]
    fn detached_and_initial() {
        let (h, _) = parse_status(b"# branch.oid (initial)\0# branch.head (detached)\0");
        assert_eq!(h.oid, None);
        assert_eq!(h.head, None);
    }

    #[test]
    fn repo_config_untracked_mode_and_remotes() {
        let repo_config = |raw: &[u8]| parse_repo_config(&parse_config(raw));
        let c = repo_config(b"");
        assert_eq!(
            (c.untracked_mode, c.has_remote),
            ("--untracked-files=all", false)
        );
        // The last value wins.
        let c = repo_config(
            b"local\0remote.origin.url\n/x.git\0global\0status.showuntrackedfiles\nno\0local\0status.showuntrackedfiles\nnormal\0",
        );
        assert_eq!(
            (c.untracked_mode, c.has_remote),
            ("--untracked-files=normal", true)
        );
        // A key without a value is boolean true.
        let c = repo_config(b"local\0status.showuntrackedfiles\0");
        assert_eq!(c.untracked_mode, "--untracked-files=normal");
        let c = repo_config(b"local\0status.showuntrackedfiles\nOff\0");
        assert_eq!(c.untracked_mode, "--untracked-files=no");
        assert!(!repo_config(b"local\0remote.origin.fetch\nx\0").has_remote);
    }

    #[test]
    fn config_entries_keep_scope_and_order() {
        let c = parse_config(b"global\0core.fsmonitor\ntrue\0local\0filter.a=b.clean\ncat\nx\0");
        assert_eq!(
            c,
            vec![
                ConfigEntry {
                    scope: "global".into(),
                    key: "core.fsmonitor".into(),
                    value: "true".into(),
                },
                ConfigEntry {
                    scope: "local".into(),
                    key: "filter.a=b.clean".into(),
                    value: "cat\nx".into(),
                },
            ]
        );
    }

    #[test]
    fn numstat_plain_binary_rename() {
        let v = parse_numstat(b"5\t0\tb.txt\0-\t-\timg.png\0");
        assert_eq!(v[0], ("b.txt".into(), Counts::lines(5, 0)));
        assert_eq!(v[1], ("img.png".into(), Counts::Binary));
        let r = parse_numstat(b"1\t2\t\0a.txt\0r.txt\0");
        assert_eq!(r, vec![("r.txt".into(), Counts::lines(1, 2))]);
    }

    #[test]
    fn log_records() {
        let raw = b"ce2b\x1fce2\x1fp1 p2\x1f1790484333\x1fAnn\x1fHEAD -> main, tag: v0.1, origin/main\x1fmerge it\x1e\nabcd\x1fabc\x1f\x1f1790484000\x1fBo\x1f\x1froot\x1e\n";
        let c = parse_log(raw);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].parents, 2);
        assert_eq!(c[0].refs, vec!["main", "tag: v0.1", "origin/main"]);
        assert_eq!(c[0].time, 1790484333);
        assert_eq!(c[1].parents, 0);
        assert!(c[1].refs.is_empty());
        assert_eq!(c[1].subject, "root");
    }

    #[test]
    fn branches_track() {
        let raw = b"feat\x1f\x1f\x1f1790484333\x1f \x1e\nmain\x1forigin/main\x1fahead 2, behind 3\x1f1790484333\x1f*\x1e\nold\x1forigin/old\x1fgone\x1f1\x1f \x1e\n";
        let b = parse_branches(raw);
        assert_eq!(b.len(), 3);
        assert_eq!((b[0].upstream.clone(), b[0].is_head), (None, false));
        assert_eq!((b[1].ahead, b[1].behind, b[1].is_head), (2, 3, true));
        assert!(b[2].gone);
    }

    #[test]
    fn stash_and_reflog() {
        let s = parse_stashes(b"stash@{1}\x1f1790484333\x1fWIP on main: ce2 x\x1e\n");
        assert_eq!((s[0].index, s[0].time), (1, 1790484333));
        assert_eq!(s[0].message, "WIP on main: ce2 x");
        let r = parse_reflog(
            "refs/remotes/origin/main",
            b"730d813\x1forigin/main@{1790484333}\x1fupdate by push\x1e\n",
        );
        assert_eq!(
            (r[0].time, r[0].message.as_str()),
            (1790484333, "update by push")
        );
        assert_eq!(r[0].refname, "refs/remotes/origin/main");
        assert_eq!(r[0].short, "730d813");
    }

    #[test]
    fn describe() {
        let t = parse_describe("v0.1.9-4-gabc1234\n").unwrap();
        assert_eq!((t.name.as_str(), t.distance), ("v0.1.9", 4));
        assert_eq!(parse_describe("rel-2-0-12-gdead").unwrap().name, "rel-2-0");
        assert!(parse_describe("garbage").is_none());
    }

    #[test]
    fn diff_lines() {
        let raw = b"diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@ fn\n ctx\n-old\n+new\n\\ No newline at end of file\n";
        let d = parse_diff(raw);
        let kinds: Vec<_> = d.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DiffKind::Hunk,
                DiffKind::Context,
                DiffKind::Del,
                DiffKind::Add,
                DiffKind::Meta
            ]
        );
        assert_eq!(d[0].text, "@@ -1,2 +1,2 @@ fn");
        assert_eq!(d[2].text, "old");
    }

    #[test]
    fn diff_expands_tabs_and_keeps_binary_note() {
        let d =
            parse_diff(b"diff --git a/i b/i\nBinary files a/i and b/i differ\n@@ -1 +1 @@\n+\tx\n");
        assert_eq!(d[0].kind, DiffKind::Meta);
        assert_eq!(d[2].text, "    x");
    }

    /// Real `git diff -U0` output: a new file with a space in its name
    /// (git ends that `+++` line with a tab), a new binary file, an edit
    /// whose added line starts with `++`, and a deleted file.
    const ADDED: &str = "diff --git a/a b.txt b/a b.txt\nnew file mode 100644\nindex 0000000..587be6b\n--- /dev/null\n+++ b/a b.txt\t\n@@ -0,0 +1 @@\n+x\ndiff --git a/k s.p12 b/k s.p12\nnew file mode 100644\nindex 0000000..bdc955b\nBinary files /dev/null and b/k s.p12 differ\ndiff --git a/plus.txt b/plus.txt\nindex 1111111..2222222 100644\n--- a/plus.txt\n+++ b/plus.txt\n@@ -3,0 +4,2 @@\n+++x\n+foo\n@@ -9 +11 @@ fn x() {\n-old\n+new\ndiff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\nindex 3333333..0000000\n--- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n";

    #[test]
    fn added_lines_with_numbers_paths_and_new_files() {
        let files = parse_added_lines(ADDED.as_bytes());
        let file = |path: &str, new_file, lines: &[(u32, &str)]| AddedFile {
            path: path.into(),
            new_file,
            lines: lines.iter().map(|(n, t)| (*n, t.to_string())).collect(),
        };
        assert_eq!(
            files,
            vec![
                file("a b.txt", true, &[(1, "x")]),
                file("k s.p12", true, &[]),
                file("plus.txt", false, &[(4, "++x"), (5, "foo"), (11, "new")]),
            ]
        );
        assert!(parse_added_lines(b"").is_empty());
    }

    #[test]
    fn commit_patches_split_on_nul() {
        let raw = b"\0aaa111\n\ndiff --git a/x b/x\n+++ b/x\n@@ -0,0 +1 @@\n+k\n\0bbb222\n";
        let parts = split_commit_patches(raw);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].0, "aaa111");
        assert_eq!(
            parse_added_lines(parts[0].1)[0].lines,
            vec![(1, "k".to_string())]
        );
        assert_eq!(parts[1], ("bbb222".to_string(), &b"\n"[..]));
    }
}
