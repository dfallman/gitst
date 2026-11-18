//! A raw single-file diff cut into hunks, and the patch for the hunk the
//! detail view showed (spec §6.2).

use super::parse::parse_diff;
use crate::model::DiffKind;
use crate::write::ShownHunk;

/// A single-file diff: its header (`diff --git` to `+++`, without mode
/// lines) and its hunks.
pub struct RawDiff {
    pub header: Vec<u8>,
    pub hunks: Vec<RawHunk>,
}

/// One hunk: as the detail view shows it, and as git wrote it.
pub struct RawHunk {
    pub shown: ShownHunk,
    pub bytes: Vec<u8>,
}

/// Splits a single-file `git diff` into its file header and hunks; `None` for binary or hunkless diffs.
pub fn split(raw: &[u8]) -> Option<RawDiff> {
    let mut header = Vec::new();
    let mut hunks: Vec<Vec<u8>> = Vec::new();
    for line in raw.split_inclusive(|b| *b == b'\n') {
        // Hunk lines start with ` `, `+`, `-`, or `\`, so a `diff ` line
        // starts another file, as a type change (file to symlink) does.
        if line.starts_with(b"diff ") && !header.is_empty() {
            return None;
        }
        if line.starts_with(b"@@") {
            hunks.push(line.to_vec());
        } else if let Some(hunk) = hunks.last_mut() {
            hunk.extend_from_slice(line);
        } else if !line.starts_with(b"old mode ") && !line.starts_with(b"new mode ") {
            // As in `git add -p`, a mode change is staged on its own,
            // with the whole file, not with a hunk.
            header.extend_from_slice(line);
        }
    }
    if !header.starts_with(b"diff --git ") {
        return None;
    }
    let hunks: Vec<RawHunk> = hunks
        .into_iter()
        .filter_map(|bytes| {
            // The view's own parse, so the match is on exactly what it shows.
            let mut lines = parse_diff(&bytes).into_iter();
            let first = lines.next().filter(|l| l.kind == DiffKind::Hunk)?;
            let shown = ShownHunk {
                header: first.text,
                lines: lines.collect(),
            };
            Some(RawHunk { shown, bytes })
        })
        .collect();
    (!hunks.is_empty()).then_some(RawDiff { header, hunks })
}

/// The patch for the hunk equal to `shown`, or `None` when none matches (a stale view).
pub fn patch_for(raw: &RawDiff, shown: &ShownHunk) -> Option<Vec<u8>> {
    let hunk = raw.hunks.iter().find(|h| h.shown == *shown)?;
    Some([raw.header.as_slice(), hunk.bytes.as_slice()].concat())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DiffKind, DiffLine};
    use crate::write::ShownHunk;

    const TWO: &[u8] = concat!(
        "diff --git a/x b/x\n",
        "index 1111111..2222222 100644\n",
        "--- a/x\n",
        "+++ b/x\n",
        "@@ -1,3 +1,3 @@\n",
        " a\n",
        "-b\n",
        "+B\n",
        " c\n",
        "@@ -10,3 +10,3 @@ fn main() {\n",
        " j\n",
        "-k\n",
        "+K\n",
        " l\n",
    )
    .as_bytes();

    fn line(kind: DiffKind, text: &str) -> DiffLine {
        DiffLine {
            kind,
            text: text.into(),
        }
    }

    fn shown(header: &str, lines: &[(DiffKind, &str)]) -> ShownHunk {
        ShownHunk {
            header: header.into(),
            lines: lines.iter().map(|(k, t)| line(*k, t)).collect(),
        }
    }

    #[test]
    fn split_two_hunks() {
        let d = split(TWO).unwrap();
        assert!(d.header.ends_with(b"+++ b/x\n"), "{:?}", d.header);
        assert!(d.header.starts_with(b"diff --git a/x b/x\n"));
        assert_eq!(d.hunks.len(), 2);
        assert!(d.hunks.iter().all(|h| h.bytes.starts_with(b"@@")));
        assert_eq!(d.hunks[1].shown.header, "@@ -10,3 +10,3 @@ fn main() {");
        assert_eq!(
            d.hunks[0].shown,
            shown(
                "@@ -1,3 +1,3 @@",
                &[
                    (DiffKind::Context, "a"),
                    (DiffKind::Del, "b"),
                    (DiffKind::Add, "B"),
                    (DiffKind::Context, "c"),
                ],
            )
        );
        let patch = patch_for(&d, &d.hunks[1].shown).unwrap();
        let mut want = d.header.clone();
        want.extend_from_slice(b"@@ -10,3 +10,3 @@ fn main() {\n j\n-k\n+K\n l\n");
        assert_eq!(patch, want);
    }

    #[test]
    fn mode_lines_are_left_out_of_the_header() {
        let raw = concat!(
            "diff --git a/x b/x\n",
            "old mode 100644\n",
            "new mode 100755\n",
            "index 1111111..2222222\n",
            "--- a/x\n",
            "+++ b/x\n",
            "@@ -1 +1 @@\n",
            "-a\n",
            "+b\n",
        )
        .as_bytes();
        let d = split(raw).unwrap();
        assert_eq!(
            d.header,
            b"diff --git a/x b/x\nindex 1111111..2222222\n--- a/x\n+++ b/x\n"
        );
    }

    #[test]
    fn hunkless_binary_and_two_file_diffs_do_not_split() {
        assert!(split(b"").is_none());
        let mode = b"diff --git a/x b/x\nold mode 100644\nnew mode 100755\n";
        assert!(split(mode).is_none());
        let binary = b"diff --git a/x b/x\nindex 1..2 100644\nBinary files a/x and b/x differ\n";
        assert!(split(binary).is_none());
        let mut two = TWO.to_vec();
        two.extend_from_slice(b"diff --git a/x b/x\nnew file mode 120000\n--- /dev/null\n+++ b/x\n@@ -0,0 +1 @@\n+t\n");
        assert!(split(&two).is_none());
    }

    #[test]
    fn patch_matches_after_tab_and_crlf_normalization() {
        let raw = concat!(
            "diff --git a/x b/x\n",
            "index 1111111..2222222 100644\n",
            "--- a/x\n",
            "+++ b/x\n",
            "@@ -1,2 +1,2 @@\tfn\r\n",
            " \tkeep\r\n",
            "-\told\r\n",
            "+\tx\r\n",
        )
        .as_bytes();
        let d = split(raw).unwrap();
        let want = shown(
            "@@ -1,2 +1,2 @@    fn",
            &[
                (DiffKind::Context, "    keep"),
                (DiffKind::Del, "    old"),
                (DiffKind::Add, "    x"),
            ],
        );
        let patch = patch_for(&d, &want).unwrap();
        assert!(
            patch.ends_with(b"+\tx\r\n"),
            "the raw bytes, not the shown text"
        );
    }

    #[test]
    fn lossy_text_matches_as_shown() {
        let raw = b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-\xff\n+\xfe ok\n";
        let d = split(raw).unwrap();
        let want = shown(
            "@@ -1 +1 @@",
            &[(DiffKind::Del, "\u{fffd}"), (DiffKind::Add, "\u{fffd} ok")],
        );
        assert!(patch_for(&d, &want).unwrap().ends_with(b"+\xfe ok\n"));
    }

    #[test]
    fn no_newline_marker_kept_in_hunk() {
        let raw = concat!(
            "diff --git a/x b/x\n",
            "index 1111111..2222222 100644\n",
            "--- a/x\n",
            "+++ b/x\n",
            "@@ -1 +1 @@\n",
            "-a\n",
            "+b\n",
            "\\ No newline at end of file\n",
        )
        .as_bytes();
        let d = split(raw).unwrap();
        assert!(
            d.hunks[0]
                .bytes
                .ends_with(b"+b\n\\ No newline at end of file\n")
        );
        let want = shown(
            "@@ -1 +1 @@",
            &[
                (DiffKind::Del, "a"),
                (DiffKind::Add, "b"),
                (DiffKind::Meta, "\\ No newline at end of file"),
            ],
        );
        assert!(
            patch_for(&d, &want)
                .unwrap()
                .ends_with(b"\\ No newline at end of file\n")
        );
    }

    #[test]
    fn stale_hunk_is_none() {
        let d = split(TWO).unwrap();
        let mut moved = d.hunks[0].shown.clone();
        moved.header = "@@ -2,3 +2,3 @@".into();
        assert!(patch_for(&d, &moved).is_none(), "a header that moved");
        let mut edited = d.hunks[0].shown.clone();
        edited.lines[2].text = "BB".into();
        assert!(patch_for(&d, &edited).is_none(), "a line that changed");
    }
}
