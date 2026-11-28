//! The repository paths the README and `docs/*.md` name in code spans
//! (`scripts/bench/android-boot.sh`) that don't exist, for `content::link_code_paths`: such a
//! span stays code instead of becoming a link to a 404 on GitHub (a removed script, or a path
//! relative to a crate such as `tests/publish.rs`).
//!
//! It reruns when the README or a doc changes. A path that is removed later, under a doc that
//! doesn't change, stops linking from the next build that reruns it (CI builds from scratch).

use std::collections::BTreeSet;
use std::path::Path;
use std::{env, fs};

/// `content::REPO_DIRS`, and the same shape of path `content::link_code_paths` looks for, in
/// the same places: code spans on a line outside fenced code.
const REPO_DIRS: [&str; 7] = ["scripts/", "crates/", "apps/", "desktop/", "website/", "tests/", "docs/"];

fn is_path(code: &str) -> bool {
    REPO_DIRS.iter().any(|d| code.starts_with(d)) && code.len() > 4 && code.chars().all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c))
}

fn main() {
    let site = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo = site.parent().expect("website/ is in the repository");
    let mut sources = vec![repo.join("README.md")];
    println!("cargo::rerun-if-changed={}", repo.join("README.md").display());
    println!("cargo::rerun-if-changed={}", repo.join("docs").display());
    if let Ok(entries) = fs::read_dir(repo.join("docs")) {
        sources.extend(entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "md")));
    }
    let mut missing = BTreeSet::new();
    for source in sources {
        let Ok(md) = fs::read_to_string(&source) else { continue };
        let mut fence = false;
        for line in md.lines() {
            if line.trim_start().starts_with("```") {
                fence = !fence;
            }
            if fence {
                continue;
            }
            for (i, code) in line.split('`').enumerate() {
                if i % 2 == 1 && is_path(code) && !repo.join(code.trim_end_matches('/')).exists() {
                    missing.insert(code.to_owned());
                }
            }
        }
    }
    let list: String = missing.iter().map(|p| format!("    {p:?},\n")).collect();
    let out = Path::new(&env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("missing_paths.rs");
    let doc = "/// Repository paths the README and docs name in code spans that don't exist (build.rs).";
    fs::write(out, format!("{doc}\npub const MISSING_PATHS: &[&str] = &[\n{list}];\n")).expect("write missing_paths.rs");
}
