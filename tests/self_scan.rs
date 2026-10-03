//! gitst's own sources must not trip its secret rules, so running gitst on
//! gitst stays quiet. Test keys are assembled at run time.

use std::path::{Path, PathBuf};

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        if p.is_dir() {
            files(&p, out);
        } else {
            out.push(p);
        }
    }
}

#[test]
fn sources_hold_no_possible_secrets() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut paths = vec![root.join("README.md")];
    files(&root.join("src"), &mut paths);
    files(&root.join("tests"), &mut paths);
    for p in paths {
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let hits = gitst::leaks::scan_lines(text.lines().zip(1..).map(|(l, n)| (n, l)));
        assert!(hits.is_empty(), "{}: {hits:?}", p.display());
    }
}
