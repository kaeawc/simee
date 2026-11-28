//! emu never links the Android or iOS device stacks (#325): another device's screen reaches its
//! camera as a `feed:` the simee server serves, so nothing emu builds may depend on
//! `simee-android` or `simee-coresim`, behind a feature or not. Cargo unifies features across the
//! packages one invocation builds (`cargo build -p simee -p simee-emu`, as the release does), so an
//! optional dependency that simee's side enables would reach emu too: this asks Cargo's own
//! resolver (`cargo tree`, offline) which packages depend on each stack with every workspace
//! feature on and the dependencies of every target emu builds for, following normal ones only (emu's own
//! dev-dependencies don't link into it). Cargo resolves the manifests, so a dependency table
//! (`[dependencies.x]`, `[target.'cfg(..)'.dependencies.x]`), a path dependency under another key
//! (`foo = { path = "..", package = "simee-android" }`) or a workspace one are all seen.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The crates emu must never reach.
const DEVICE_STACKS: [&str; 2] = ["simee-android", "simee-coresim"];

/// The targets emu builds for, whose target-specific dependencies count.
const TARGETS: [&str; 3] = ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"];

/// The packages of the workspace at `root` that depend on `stack` through normal dependencies
/// (itself included), with every feature of every member on and the dependencies of [`TARGETS`]:
/// empty when the workspace has no such package.
fn dependents(root: &Path, stack: &str) -> Vec<String> {
    let tree = |offline: bool| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let mut cmd = Command::new(cargo);
        cmd.args(["tree", "--workspace", "--all-features", "-e", "normal", "--prefix", "none", "--format", "{p}", "-i", stack]);
        for target in TARGETS {
            cmd.args(["--target", target]);
        }
        if offline {
            cmd.arg("--offline");
        }
        cmd.arg("--manifest-path").arg(root.join("Cargo.toml")).output().expect("cargo tree runs")
    };
    // Offline first; a host that hasn't fetched another target's crates (a Linux CI runner and
    // the macOS-only ones) fetches them.
    let mut out = tree(true);
    if !out.status.success() && String::from_utf8_lossy(&out.stderr).contains("--offline was specified") {
        out = tree(false);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        // A workspace without that package has nothing depending on it.
        assert!(stderr.contains("did not match any packages"), "cargo tree -i {stack}: {stderr}");
        return Vec::new();
    }
    let mut names: Vec<String> =
        String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.split_whitespace().next()).map(str::to_owned).collect();
    names.sort();
    names.dedup();
    names
}

#[test]
fn emu_reaches_neither_device_stack_even_through_optional_dependencies() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    if !root.join("apps/emu/Cargo.toml").is_file() {
        eprintln!("skipped: not in the simee workspace, so apps/emu isn't here");
        return;
    }
    for stack in DEVICE_STACKS {
        let on = dependents(&root, stack);
        assert!(!on.iter().any(|p| p == "simee-emu"), "emu reaches {stack}: {stack} is a dependency of {on:?}");
        // simee itself does link it: the walk would see emu if it did.
        assert!(on.iter().any(|p| p == "simee"), "the walk sees simee reach {stack}: {on:?}");
        assert!(on.iter().any(|p| p == stack), "{on:?}");
    }
    let camera = dependents(&root, "simee-camera");
    assert!(camera.iter().any(|p| p == "simee-emu"), "the walk follows emu's dependencies: {camera:?}");
}

/// A throwaway workspace whose emu reaches simee-android in the ways a manifest-text walk missed:
/// a dependency table (`[target.'cfg(unix)'.dependencies.mid]`), and a path dependency under
/// another key (`droid = { path = .., package = "simee-android", optional = true }`).
fn fixture(tag: &str) -> PathBuf {
    let root = PathBuf::from("/tmp").join(format!("emu-stacks-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let write = |path: &str, text: &str| {
        let at = root.join(path);
        std::fs::create_dir_all(at.parent().expect("dir")).expect("mkdir");
        std::fs::write(at, text).expect("write");
    };
    write(
        "Cargo.toml",
        "[workspace]\nresolver = \"2\"\nmembers = [\"apps/emu\", \"crates/mid\", \"crates/android\"]\n\n\
         [workspace.dependencies]\nmid = { path = \"crates/mid\" }\n",
    );
    write(
        "apps/emu/Cargo.toml",
        "[package]\nname = \"simee-emu\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[target.'cfg(unix)'.dependencies.mid]\nworkspace = true\n",
    );
    write(
        "crates/mid/Cargo.toml",
        "[package]\nname = \"mid\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\nscreens = [\"dep:droid\"]\n\n\
         [dependencies]\ndroid = { path = \"../android\", package = \"simee-android\", optional = true }\n",
    );
    write("crates/android/Cargo.toml", "[package]\nname = \"simee-android\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    for krate in ["apps/emu", "crates/mid", "crates/android"] {
        write(&format!("{krate}/src/lib.rs"), "");
    }
    root
}

#[test]
fn a_dependency_table_or_a_renamed_path_dependency_is_caught() {
    let root = fixture("forms");
    let on = dependents(&root, "simee-android");
    let none = dependents(&root, "simee-coresim");
    let _ = std::fs::remove_dir_all(&root);
    assert!(on.iter().any(|p| p == "simee-emu") && on.iter().any(|p| p == "mid"), "{on:?}");
    assert!(none.is_empty(), "no such package: nothing depends on it: {none:?}");
}
