//! Embeds the build identifier (ADR-0045 decision 2) as `DAGQ_BUILD_ID`,
//! which `dagq::VERSION` and `dagq --version` report.

use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "src/build_id.rs"]
mod build_id;

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/build_id.rs");
    let state = if build_id::is_prerelease(&version) {
        // Registered before anything can fail, so a build that fell back to
        // `unknown` is looked at again once the sources change rather than
        // kept until build.rs itself does.
        for path in ["src", "migrations", "Cargo.toml", "Cargo.lock"] {
            println!("cargo:rerun-if-changed={path}");
        }
        let state = git_state(&manifest_dir);
        if state.is_none() {
            // `cargo package` verifies the unpacked copy in
            // target/package/ with the checkout's own target directory, and
            // cargo takes that build for the checkout's (the unit hash does
            // not depend on where the package sits). The copy has no `.git`,
            // and a watched path that is missing always reruns the script,
            // so the checkout's next build names its commit again.
            println!(
                "cargo:rerun-if-changed={}",
                manifest_dir.join(".git").display()
            );
            println!(
                "cargo:warning=dagq {version} is built outside a Git worktree or without git; \
                 its build identifier is {version}+{}",
                build_id::UNKNOWN_COMMIT
            );
        }
        state
    } else {
        None
    };
    let (commit, dirty) = match &state {
        Some((commit, dirty)) => (Some(commit.as_str()), *dirty),
        None => (None, false),
    };
    println!(
        "cargo:rustc-env=DAGQ_BUILD_ID={}",
        build_id::build_identifier(&version, commit, dirty)
    );
}

/// The commit `HEAD` names and whether the worktree has uncommitted changes,
/// or `None` when the package is not the root of a Git worktree (a crates.io
/// source unpacked somewhere, possibly inside another repository) or `git`
/// cannot answer.
fn git_state(manifest_dir: &Path) -> Option<(String, bool)> {
    let toplevel = git(manifest_dir, &["rev-parse", "--show-toplevel"])?;
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !same(Path::new(&toplevel), manifest_dir) {
        return None;
    }
    // Rerun when this worktree moves to another commit (HEAD, and the ref of
    // the branch it is on) or its index changes, so the identifier follows
    // commits and checkouts. Only this branch's ref is watched: the refs
    // directory is shared by every worktree, and a commit in another one
    // must not rebuild this one. An edit elsewhere in the worktree (docs,
    // say) marks the next build dirty only once something else reruns this
    // script.
    let mut watched = vec![
        "HEAD".to_owned(),
        "index".to_owned(),
        "packed-refs".to_owned(),
    ];
    watched.extend(git(manifest_dir, &["symbolic-ref", "-q", "HEAD"]));
    for name in watched {
        if let Some(path) = git(manifest_dir, &["rev-parse", "--git-path", &name]) {
            println!(
                "cargo:rerun-if-changed={}",
                manifest_dir.join(path).display()
            );
        }
    }
    let commit = git(manifest_dir, &["rev-parse", "--verify", "HEAD"])?;
    let status = git(manifest_dir, &["status", "--porcelain"])?;
    Some((commit, !status.is_empty()))
}

/// The trimmed stdout of a successful `git` in `dir`.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
