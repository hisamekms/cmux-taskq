//! The build identifier a binary names itself by (ADR-0045 decision 2).
//!
//! `build.rs` includes this file to compute the identifier it embeds, and the
//! library compiles it as a module so the rule is tested like any other code.
//! A release (a version without a pre-release) is its version alone, `X.Y.Z`.
//! A development version names the commit it was built from as SemVer build
//! metadata, `X.Y.Z-dev+<commit>`, with `.dirty` when the worktree had
//! uncommitted changes, and `+unknown` when it was built outside a Git
//! repository (from the crates.io source, say, or without `git`).

/// The metadata of a build that could not name its commit.
pub const UNKNOWN_COMMIT: &str = "unknown";

/// The build identifier of `version` built from `commit` (`None` when it is
/// not known), `dirty` when the worktree had uncommitted changes.
pub fn build_identifier(version: &str, commit: Option<&str>, dirty: bool) -> String {
    if !is_prerelease(version) {
        return version.to_owned();
    }
    match commit {
        Some(commit) if dirty => format!("{version}+{commit}.dirty"),
        Some(commit) => format!("{version}+{commit}"),
        None => format!("{version}+{UNKNOWN_COMMIT}"),
    }
}

/// Whether `version` has a SemVer pre-release, such as `0.4.0-dev`. Build
/// metadata is not part of the version Cargo gives, so any `-` is one.
pub fn is_prerelease(version: &str) -> bool {
    version.split('+').next().unwrap_or(version).contains('-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_names_its_version_alone() {
        assert_eq!(build_identifier("0.4.0", Some("abc"), true), "0.4.0");
        assert_eq!(build_identifier("0.4.0", None, false), "0.4.0");
    }

    #[test]
    fn a_development_version_names_its_commit_and_dirtiness() {
        let commit = "89e8c54ed2a851920a44435bf3868683d33f6a45";
        assert_eq!(
            build_identifier("0.4.0-dev", Some(commit), false),
            format!("0.4.0-dev+{commit}")
        );
        assert_eq!(
            build_identifier("0.4.0-dev", Some(commit), true),
            format!("0.4.0-dev+{commit}.dirty")
        );
    }

    #[test]
    fn a_development_version_outside_git_is_unknown() {
        assert_eq!(
            build_identifier("0.4.0-dev", None, true),
            "0.4.0-dev+unknown"
        );
    }

    #[test]
    fn only_a_pre_release_is_one() {
        assert!(is_prerelease("0.4.0-dev"));
        assert!(is_prerelease("1.0.0-rc.1+abc"));
        assert!(!is_prerelease("0.4.0"));
        assert!(!is_prerelease("0.4.0+abc-def"));
    }
}
