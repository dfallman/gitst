//! Pure parsers for git's machine-readable output.

use crate::model::{Branch, Change, Commit, ReflogEntry, Stash, TagInfo};

pub const LOG_FORMAT: &str = "%H%x1f%h%x1f%P%x1f%at%x1f%an%x1f%D%x1f%s%x1e";
pub const BRANCH_FORMAT: &str = "%(refname:short)%1f%(upstream:short)%1f%(upstream:track,nobracket)%1f%(committerdate:unix)%1f%(HEAD)%1e";
pub const STASH_FORMAT: &str = "%gd%x1f%ct%x1f%gs%x1e";
pub const REFLOG_FORMAT: &str = "%h%x1f%gd%x1f%gs%x1e";

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
    let mut fields = raw.split(|b| *b == 0).map(|f| String::from_utf8_lossy(f).into_owned());

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
                added: None,
                removed: None,
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
        added: None,
        removed: None,
    })
}

pub type NumstatEntry = (String, Option<u32>, Option<u32>);

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

fn count(s: &str) -> Option<u32> {
    s.parse().ok()
}

/// Parses `git diff --numstat -z`. Renames report the new path.
pub fn parse_numstat(raw: &[u8]) -> Vec<NumstatEntry> {
    let mut out = Vec::new();
    let mut fields = raw.split(|b| *b == 0).map(|f| String::from_utf8_lossy(f).into_owned());
    while let Some(field) = fields.next() {
        let mut parts = field.splitn(3, '\t');
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
        out.push((path, count(a), count(r)));
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
    Some(TagInfo { name: name.to_string(), distance })
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
        assert_eq!((c[0].path.as_str(), c[0].x, c[0].y), ("src/app.rs", ' ', 'M'));
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
    fn numstat_plain_binary_rename() {
        let v = parse_numstat(b"5\t0\tb.txt\0-\t-\timg.png\0");
        assert_eq!(v[0], ("b.txt".into(), Some(5), Some(0)));
        assert_eq!(v[1], ("img.png".into(), None, None));
        let r = parse_numstat(b"1\t2\t\0a.txt\0r.txt\0");
        assert_eq!(r, vec![("r.txt".into(), Some(1), Some(2))]);
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
        assert_eq!((r[0].time, r[0].message.as_str()), (1790484333, "update by push"));
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
}
