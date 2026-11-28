//! The pre-push checks as one program: `cargo xtask check` does what `scripts/check.sh` does,
//! with the same step names, in the same order, on every host. The bash script stays the macOS
//! and Linux path; this is the Windows one (no bash needed) and works anywhere cargo does.
//!
//! The steps are data ([`plan`]) and the decisions behind them are pure functions of the changed
//! files, the host and the installed tools, so tests cover them without running cargo.

use std::fmt::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

/// Why a task didn't succeed: bad arguments (exit 2), or a failed step (exit 1, already reported).
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    Usage(String),
    Check,
}

pub fn usage() -> String {
    "usage: cargo xtask check [--quick|--all]\n  \
     check            what the change needs (the pre-push hook runs this)\n  \
     check --quick    linters, fmt, host clippy and tests only\n  \
     check --all      every step, whatever changed"
        .into()
}

/// How much of the checks to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// What the change needs: docs-only changes run the linters and nothing else.
    Changed,
    Quick,
    All,
}

pub fn parse(args: &[String]) -> Result<Mode, Failure> {
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["check"] => Ok(Mode::Changed),
        ["check", "--quick"] => Ok(Mode::Quick),
        ["check", "--all"] => Ok(Mode::All),
        _ => Err(Failure::Usage(usage())),
    }
}

pub fn run(args: &[String]) -> Result<(), Failure> {
    let mode = parse(args)?;
    let root = repo_root();
    let host = Host::current();
    let have = Tools::probe(&root);
    let changed = changed_files(&root);
    let steps = plan(mode, &host, &have, &changed);
    execute(&root, &steps)
}

/// The operating system a check runs on, as `std::env::consts::OS` names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    pub os: String,
}

impl Host {
    pub fn current() -> Host {
        Host { os: std::env::consts::OS.to_owned() }
    }
}

/// What is installed here, by the names `scripts/check.sh` branches on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tools {
    /// Linters from `scripts/lint.sh` that run (each needs bash, so none without it).
    pub linters: Vec<&'static str>,
    pub python: bool,
    pub nextest: bool,
    /// Rust targets installed with rustup.
    pub targets: Vec<String>,
}

/// The linters `scripts/lint.sh` knows, in the order `scripts/check.sh` runs them.
pub const LINTERS: [&str; 6] = ["shellcheck", "shfmt", "lychee", "typos", "actionlint", "yamllint"];

impl Tools {
    pub fn probe(root: &Path) -> Tools {
        let works = |program: &str, args: &[&str]| {
            Command::new(program)
                .args(args)
                .current_dir(root)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        let bash = works("bash", &["--version"]);
        let linters = if bash {
            LINTERS
                .into_iter()
                .filter(|tool| {
                    // shellcheck needs shfmt to list the scripts.
                    let bin = if *tool == "shellcheck" && !works("shfmt", &["--version"]) { "shfmt" } else { tool };
                    works(bin, &["--version"])
                })
                .collect()
        } else {
            Vec::new()
        };
        let targets =
            output(root, "rustup", &["target", "list", "--installed"]).map(|o| o.lines().map(str::to_owned).collect()).unwrap_or_default();
        Tools {
            linters,
            python: works("python3", &["--version"]) || works("python", &["--version"]),
            nextest: works("cargo", &["nextest", "--version"]),
            targets,
        }
    }
}

/// One step: a label, and the program that is the step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub label: String,
    pub program: String,
    pub args: Vec<String>,
    /// Relative to the repository root.
    pub dir: Option<&'static str>,
}

/// A step that is not run, and why (printed as `skipped: ...`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Planned {
    Run(Step),
    Skip { label: String, why: String },
}

fn step(label: &str, program: &str, args: &[&str]) -> Planned {
    Planned::Run(Step { label: label.into(), program: program.into(), args: args.iter().map(|a| (*a).into()).collect(), dir: None })
}

fn skip(label: &str, why: &str) -> Planned {
    Planned::Skip { label: label.into(), why: why.into() }
}

/// Whether the change touches only docs (Markdown, `docs/`, PNGs), which need no build.
pub fn docs_only(changed: &[String]) -> bool {
    !changed.is_empty() && changed.iter().all(|f| f.ends_with(".md") || f.starts_with("docs/") || f.ends_with(".png"))
}

fn touches_desktop(changed: &[String]) -> bool {
    changed.iter().any(|f| f.starts_with("desktop/"))
}

/// The labels `scripts/check.sh` uses for the steps both programs have, which a test keeps equal.
pub const SHARED_LABELS: [&str; 7] = ["fmt", "fmt (desktop)", "clippy", "macOS test list", "tests", "desktop clippy", "desktop tests"];

/// The steps for `mode` on `host` with `have` installed and `changed` files since the merge base.
pub fn plan(mode: Mode, host: &Host, have: &Tools, changed: &[String]) -> Vec<Planned> {
    let mut steps = Vec::new();
    for tool in LINTERS {
        if have.linters.contains(&tool) {
            steps.push(step(tool, "bash", &["scripts/lint.sh", tool]));
        } else {
            let why = if have.linters.is_empty() { format!("bash and {tool} not both installed") } else { format!("{tool} not installed") };
            steps.push(skip(tool, &why));
        }
    }
    if mode == Mode::Changed && docs_only(changed) {
        steps.push(skip("build", "only docs changed; nothing to build"));
        return steps;
    }
    steps.push(step("fmt", "cargo", &["fmt", "--all", "--check"]));
    steps.push(step("fmt (desktop)", "rustfmt", &["--check", "--edition", "2024", "desktop/src/main.rs"]));
    steps.push(step("clippy", "cargo", &["clippy", "--all-targets", "--quiet", "--", "-D", "warnings"]));
    if have.python {
        steps.push(step("macOS test list", "python3", &["scripts/ci-scope.py", "check"]));
    } else {
        steps.push(skip("macOS test list", "python3 not installed"));
    }
    if mode != Mode::Quick {
        // The other platform's code, type-checked from here when its std is installed.
        let other = match host.os.as_str() {
            "linux" => Some("aarch64-apple-darwin"),
            "macos" => Some("x86_64-unknown-linux-gnu"),
            _ => None,
        };
        if let Some(target) = other {
            let label = format!("clippy ({target})");
            if have.targets.iter().any(|t| t == target) {
                steps.push(step(&label, "cargo", &["clippy", "--target", target, "--all-targets", "--quiet", "--", "-D", "warnings"]));
            } else {
                steps.push(skip(&label, &format!("rustup target add {target}")));
            }
        }
    }
    if have.nextest {
        steps.push(step(
            "tests",
            "cargo",
            &[
                "nextest",
                "run",
                "--workspace",
                "--no-fail-fast",
                "--status-level",
                "fail",
                "--final-status-level",
                "fail",
                "--hide-progress-bar",
            ],
        ));
    } else {
        steps.push(step("tests", "cargo", &["test", "--workspace", "--no-fail-fast", "--quiet"]));
    }
    if host.os == "macos" {
        steps.push(step(
            "release imports",
            "bash",
            &["-c", "cargo build --release --quiet -p simee-emu -p simee && scripts/check-imports.sh"],
        ));
    }
    if mode != Mode::Quick && (mode == Mode::All || touches_desktop(changed)) && host.os == "windows" {
        for label in ["desktop clippy", "desktop tests"] {
            steps.push(skip(label, "the desktop app doesn't build on Windows yet (#250)"));
        }
    } else if mode != Mode::Quick && (mode == Mode::All || touches_desktop(changed)) {
        for (label, args) in [
            ("desktop clippy", ["clippy", "--all-targets", "--quiet", "--", "-D", "warnings"].as_slice()),
            ("desktop tests", ["test", "--quiet"].as_slice()),
        ] {
            steps.push(Planned::Run(Step {
                label: label.into(),
                program: "cargo".into(),
                args: args.iter().map(|a| (*a).into()).collect(),
                dir: Some("desktop"),
            }));
        }
    }
    steps
}

/// The labels in `plan`'s output, in order (for tests and for `--list`-style output).
pub fn labels(steps: &[Planned]) -> Vec<&str> {
    steps
        .iter()
        .map(|s| match s {
            Planned::Run(s) => s.label.as_str(),
            Planned::Skip { label, .. } => label.as_str(),
        })
        .collect()
}

fn execute(root: &Path, steps: &[Planned]) -> Result<(), Failure> {
    let total = Instant::now();
    for planned in steps {
        match planned {
            Planned::Skip { label, why } => println!("check: {label:<34}skipped: {why}"),
            Planned::Run(step) => {
                print!("check: {:<34}", step.label);
                let start = Instant::now();
                let dir = step.dir.map_or_else(|| root.to_path_buf(), |d| root.join(d));
                let out = Command::new(&step.program).args(&step.args).current_dir(&dir).output();
                let secs = start.elapsed().as_secs();
                match out {
                    Ok(o) if o.status.success() => println!("ok  ({secs}s)"),
                    Ok(o) => {
                        println!("FAILED ({secs}s)\n");
                        println!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
                        println!("\n  rerun: {}", rerun(step));
                        return Err(Failure::Check);
                    }
                    Err(e) => {
                        println!("FAILED ({secs}s)\n\n{}: {e}\n\n  rerun: {}", step.program, rerun(step));
                        return Err(Failure::Check);
                    }
                }
            }
        }
    }
    println!("check: all passed in {}s", total.elapsed().as_secs());
    Ok(())
}

fn rerun(step: &Step) -> String {
    let mut line = String::new();
    if let Some(dir) = step.dir {
        let _ = write!(line, "cd {dir} && ");
    }
    line.push_str(&step.program);
    for a in &step.args {
        line.push(' ');
        line.push_str(a);
    }
    line
}

/// The repository root: the parent of this crate's manifest directory.
pub fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().map(Path::to_path_buf).unwrap_or_else(|| ".".into())
}

fn output(root: &Path, program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).current_dir(root).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Files changed since the merge base with `origin/main`, plus uncommitted and untracked ones
/// (as `scripts/check.sh` lists them). Empty when git can't say.
pub fn changed_files(root: &Path) -> Vec<String> {
    let base = output(root, "git", &["merge-base", "HEAD", "origin/main"])
        .or_else(|| output(root, "git", &["rev-parse", "HEAD~1"]))
        .map_or_else(|| "HEAD".to_owned(), |s| s.trim().to_owned());
    let range = format!("{base}...HEAD");
    let mut files: Vec<String> = [
        vec!["diff", "--name-only", range.as_str()],
        vec!["diff", "--name-only", "HEAD"],
        vec!["ls-files", "--others", "--exclude-standard"],
    ]
    .iter()
    .filter_map(|args| output(root, "git", args))
    .flat_map(|o| o.lines().map(str::to_owned).collect::<Vec<_>>())
    .collect();
    files.sort();
    files.dedup();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux() -> Host {
        Host { os: "linux".into() }
    }

    fn windows() -> Host {
        Host { os: "windows".into() }
    }

    fn all_tools() -> Tools {
        Tools { linters: LINTERS.to_vec(), python: true, nextest: true, targets: vec!["aarch64-apple-darwin".into()] }
    }

    fn files(list: &[&str]) -> Vec<String> {
        list.iter().map(|f| (*f).to_owned()).collect()
    }

    #[test]
    fn arguments_pick_the_mode_and_anything_else_is_usage() {
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(parse(&args(&["check"])), Ok(Mode::Changed));
        assert_eq!(parse(&args(&["check", "--quick"])), Ok(Mode::Quick));
        assert_eq!(parse(&args(&["check", "--all"])), Ok(Mode::All));
        for bad in [args(&[]), args(&["check", "--fast"]), args(&["build"]), args(&["check", "--quick", "--all"])] {
            assert_eq!(parse(&bad), Err(Failure::Usage(usage())), "{bad:?}");
        }
    }

    #[test]
    fn a_docs_only_change_runs_the_linters_and_stops() {
        let steps = plan(Mode::Changed, &linux(), &all_tools(), &files(&["README.md", "docs/windows.md", "assets/x.png"]));
        assert_eq!(labels(&steps), ["shellcheck", "shfmt", "lychee", "typos", "actionlint", "yamllint", "build"]);
        assert!(matches!(steps.last(), Some(Planned::Skip { why, .. }) if why.contains("only docs changed")));
        // --all and --quick don't take the shortcut; an empty change list (git can't say) doesn't either.
        assert!(labels(&plan(Mode::All, &linux(), &all_tools(), &files(&["README.md"]))).contains(&"tests"));
        assert!(labels(&plan(Mode::Changed, &linux(), &all_tools(), &[])).contains(&"tests"));
        assert!(!docs_only(&files(&["README.md", "src/lib.rs"])));
    }

    #[test]
    fn windows_runs_fmt_clippy_and_tests_without_bash() {
        let have = Tools { linters: vec![], python: true, nextest: false, targets: vec![] };
        let steps = plan(Mode::Changed, &windows(), &have, &files(&["crates/sim-core/src/lib.rs"]));
        let run: Vec<&str> = steps.iter().filter_map(|s| if let Planned::Run(s) = s { Some(s.label.as_str()) } else { None }).collect();
        assert_eq!(run, ["fmt", "fmt (desktop)", "clippy", "macOS test list", "tests"]);
        assert!(matches!(&steps[0], Planned::Skip { why, .. } if why == "bash and shellcheck not both installed"));
        assert!(
            steps.iter().any(
                |s| matches!(s, Planned::Run(s) if s.label == "tests" && s.args == ["test", "--workspace", "--no-fail-fast", "--quiet"])
            )
        );
        assert!(!labels(&steps).iter().any(|l| l.starts_with("clippy (")), "no other platform to check from Windows");
    }

    #[test]
    fn the_other_platforms_clippy_needs_its_std() {
        let linux_have = all_tools();
        assert!(labels(&plan(Mode::Changed, &linux(), &linux_have, &files(&["a.rs"]))).contains(&"clippy (aarch64-apple-darwin)"));
        let without = Tools { targets: vec![], ..all_tools() };
        let steps = plan(Mode::Changed, &linux(), &without, &files(&["a.rs"]));
        assert!(steps.iter().any(|s| matches!(s, Planned::Skip { label, why } if label == "clippy (aarch64-apple-darwin)" && why == "rustup target add aarch64-apple-darwin")));
        assert!(
            !labels(&plan(Mode::Quick, &linux(), &linux_have, &files(&["a.rs"]))).iter().any(|l| l.starts_with("clippy (")),
            "--quick skips it"
        );
        let mac = Host { os: "macos".into() };
        let mac_have = Tools { targets: vec!["x86_64-unknown-linux-gnu".into()], ..all_tools() };
        let steps = plan(Mode::Changed, &mac, &mac_have, &files(&["a.rs"]));
        assert!(labels(&steps).contains(&"clippy (x86_64-unknown-linux-gnu)"));
        assert!(labels(&steps).contains(&"release imports"), "macOS also checks the release's imports");
    }

    #[test]
    fn nextest_is_used_when_installed() {
        let with = plan(Mode::Quick, &linux(), &all_tools(), &files(&["a.rs"]));
        assert!(with.iter().any(|s| matches!(s, Planned::Run(s) if s.label == "tests" && s.args[0] == "nextest")));
    }

    #[test]
    fn the_desktop_runs_when_it_changed_or_with_all_never_with_quick() {
        let has = |mode, changed: &[&str]| labels(&plan(mode, &linux(), &all_tools(), &files(changed))).contains(&"desktop tests");
        assert!(has(Mode::Changed, &["desktop/src/main.rs"]));
        assert!(!has(Mode::Changed, &["crates/sim-core/src/lib.rs"]));
        assert!(has(Mode::All, &["crates/sim-core/src/lib.rs"]));
        assert!(!has(Mode::Quick, &["desktop/src/main.rs"]));
        let steps = plan(Mode::All, &linux(), &all_tools(), &[]);
        assert!(steps.iter().any(|s| matches!(s, Planned::Run(s) if s.label == "desktop clippy" && s.dir == Some("desktop"))));
        // Windows says why it doesn't run them, even with --all.
        let steps = plan(Mode::All, &windows(), &all_tools(), &[]);
        assert!(steps.iter().any(|s| matches!(s, Planned::Skip { label, why } if label == "desktop tests" && why.contains("#250"))));
        assert!(!steps.iter().any(|s| matches!(s, Planned::Run(s) if s.dir.is_some())));
        assert_eq!(
            rerun(&Step { label: "x".into(), program: "cargo".into(), args: vec!["test".into()], dir: Some("desktop") }),
            "cd desktop && cargo test"
        );
    }

    /// The bash script and this program are two spellings of one check: a step added to one
    /// without the other fails here, and says which.
    #[test]
    fn the_shared_steps_have_the_bash_scripts_names() {
        let script = std::fs::read_to_string(repo_root().join("scripts/check.sh")).expect("scripts/check.sh");
        let mut in_script: Vec<String> = Vec::new();
        for line in script.lines() {
            let Some(rest) = line.trim_start().strip_prefix("step ") else { continue };
            let label = match rest.strip_prefix('"') {
                Some(quoted) => quoted.split('"').next().unwrap_or_default().to_owned(),
                None => rest.split_whitespace().next().unwrap_or_default().to_owned(),
            };
            if !label.contains('$') && label != "()" {
                in_script.push(label);
            }
        }
        let mut here: Vec<&str> = SHARED_LABELS.to_vec();
        here.push("release imports");
        for label in &in_script {
            assert!(
                here.contains(&label.as_str()),
                "scripts/check.sh has a step `{label}` that xtask/src/lib.rs doesn't (add it to plan and SHARED_LABELS)"
            );
        }
        for label in SHARED_LABELS {
            assert!(in_script.iter().any(|l| l == label), "SHARED_LABELS names `{label}`, which scripts/check.sh no longer has");
        }
        let planned = plan(Mode::All, &Host { os: "macos".into() }, &all_tools(), &[]);
        for label in SHARED_LABELS.iter().chain(&["release imports"]) {
            assert!(labels(&planned).contains(label), "plan never produces `{label}`");
        }
    }

    #[test]
    fn this_repository_has_a_root_and_git_can_list_changes_without_failing() {
        let root = repo_root();
        assert!(root.join("Cargo.toml").is_file() && root.join("xtask").is_dir());
        let _ = changed_files(&root);
        let have = Tools::probe(&root);
        assert!(have.linters.iter().all(|l| LINTERS.contains(l)));
    }
}
