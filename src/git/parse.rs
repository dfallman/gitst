//! Pure parsers for git's machine-readable output.

use crate::model::Change;

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
}
