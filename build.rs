//! Embeds the build identifier (ADR-0045 decision 2) as `DAGQ_BUILD_ID`,
//! which `dagq::VERSION` and `dagq --version` report, and lists the queue's
//! migrations for `src/infrastructure/schema.rs` (ADR-0067 decision 1).

use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "src/build_id.rs"]
mod build_id;
// Only the listing is used here; `integrate` uses the rest.
#[allow(dead_code)]
#[path = "src/migration_numbers.rs"]
mod migration_numbers;

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/build_id.rs");
    println!("cargo:rerun-if-changed=src/migration_numbers.rs");
    // A migration added, removed or renamed changes the list, whatever the
    // version (a directory is watched with everything under it).
    println!("cargo:rerun-if-changed={}", migration_numbers::DIRECTORY);
    list_migrations(&manifest_dir);
    let state = if build_id::is_prerelease(&version) {
        // Registered before anything can fail, so a build that fell back to
        // `unknown` is looked at again once the sources change rather than
        // kept until build.rs itself does.
        for path in ["src", "Cargo.toml", "Cargo.lock"] {
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

/// Write `$OUT_DIR/migrations.rs`, the array of the migrations in order of
/// their number, each through `include_str!`, which
/// `src/infrastructure/schema.rs` includes as `MIGRATIONS`. Numbers that do
/// not run from 0001 without a gap or a repeat fail the build, naming the
/// files.
fn list_migrations(manifest_dir: &Path) {
    let directory = manifest_dir.join(migration_numbers::DIRECTORY);
    let entries = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()));
    let names: Vec<String> = entries
        .map(|entry| {
            let entry = entry
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()));
            entry.file_name().to_string_lossy().into_owned()
        })
        .collect();
    let ordered = match migration_numbers::ordered(&names) {
        Ok(ordered) => ordered,
        Err(problems) => {
            for problem in problems.lines() {
                println!("cargo:warning={problem}");
            }
            panic!(
                "the migrations under {} are not numbered from 0001 without a gap or a repeat:\n{problems}",
                directory.display()
            );
        }
    };
    let mut source = String::from("&[\n");
    for name in ordered {
        let path = directory.join(name);
        source.push_str(&format!(
            "    include_str!({:?}),\n",
            path.display().to_string()
        ));
    }
    source.push_str("]\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    std::fs::write(out.join("migrations.rs"), source).expect("write migrations.rs");
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
