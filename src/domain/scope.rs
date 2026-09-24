//! The paths a task may change (`add --paths`, ADR-0029). A task that
//! declares globs lets validation and `integrate` refuse a run whose diff
//! touches anything else, so a task registered with light verification
//! cannot land a change it was not verified for. No globs: no limit.
//!
//! A glob is matched against the whole repository-relative path, with `/`
//! separating segments: `*` matches any run of characters inside one segment,
//! `?` one character inside one segment, and a segment that is exactly `**`
//! zero or more whole segments. Everything else is literal. `*.md` therefore
//! matches `README.md` but not `docs/a.md`; `docs/**` matches every path
//! under `docs/`; `**/*.md` matches Markdown at any depth.

use super::DomainError;

/// Check the globs a task declares: none blank, none absolute, none with a
/// `.` or `..` segment (Git paths never have one, so it would match nothing).
pub fn validate_path_globs(globs: &[String]) -> Result<(), DomainError> {
    for glob in globs {
        let reason = if glob.trim().is_empty() {
            Some("it is blank")
        } else if glob.starts_with('/') {
            Some("it must be relative to the repository root")
        } else if glob.split('/').any(|s| s == "." || s == "..") {
            Some("it must not have a . or .. segment")
        } else if glob.split('/').any(str::is_empty) {
            Some("it must not have an empty segment")
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(DomainError::InvalidPathGlob {
                glob: glob.clone(),
                reason,
            });
        }
    }
    Ok(())
}

/// The globs in the order given, each once.
pub fn dedup_globs(globs: &[String]) -> Vec<String> {
    let mut unique: Vec<String> = Vec::new();
    for glob in globs {
        if !unique.contains(glob) {
            unique.push(glob.clone());
        }
    }
    unique
}

/// Whether `path` matches `glob` (see the module doc).
pub fn glob_matches(glob: &str, path: &str) -> bool {
    let glob: Vec<&str> = glob.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    segments_match(&glob, &path)
}

fn segments_match(glob: &[&str], path: &[&str]) -> bool {
    match glob.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| segments_match(rest, &path[skip..])),
        Some((first, rest)) => match path.split_first() {
            Some((segment, tail)) => {
                segment_matches(first.as_bytes(), segment.as_bytes()) && segments_match(rest, tail)
            }
            None => false,
        },
    }
}

fn segment_matches(glob: &[u8], text: &[u8]) -> bool {
    match glob.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|skip| segment_matches(rest, &text[skip..])),
        Some((b'?', rest)) => {
            // One character, not one byte: skip a whole UTF-8 sequence.
            let width = text
                .first()
                .map(|b| match b.leading_ones() {
                    0 => 1,
                    n => n as usize,
                })
                .unwrap_or(0);
            width > 0 && width <= text.len() && segment_matches(rest, &text[width..])
        }
        Some((c, rest)) => text.first() == Some(c) && segment_matches(rest, &text[1..]),
    }
}

/// The paths of `changed` that no glob of `globs` matches, in the order
/// given; empty when `globs` is empty (the task declares no limit).
pub fn out_of_scope(globs: &[String], changed: &[String]) -> Vec<String> {
    if globs.is_empty() {
        return Vec::new();
    }
    changed
        .iter()
        .filter(|path| !globs.iter().any(|glob| glob_matches(glob, path)))
        .cloned()
        .collect()
}

/// The `last_error` of a run whose diff leaves the task's paths, such as
/// `changed paths outside the task's --paths: src/lib.rs`.
pub fn scope_violation_reason(paths: &[String]) -> String {
    format!(
        "changed paths outside the task's --paths: {}",
        paths.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn globs(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    #[test]
    fn a_star_stays_inside_one_segment() {
        assert!(glob_matches("*.md", "README.md"));
        assert!(!glob_matches("*.md", "docs/a.md"));
        assert!(glob_matches("docs/*.md", "docs/a.md"));
        assert!(!glob_matches("docs/*.md", "docs/adr/a.md"));
        assert!(glob_matches("src/*", "src/lib.rs"));
        assert!(!glob_matches("src/*", "src"));
        assert!(glob_matches("a*b*c", "abc"));
        assert!(glob_matches("a*b*c", "axxbyyc"));
        assert!(!glob_matches("a*b*c", "axxbyy"));
    }

    #[test]
    fn a_double_star_segment_spans_any_depth() {
        assert!(glob_matches("docs/**", "docs/a.md"));
        assert!(glob_matches("docs/**", "docs/adr/0001.md"));
        assert!(!glob_matches("docs/**", "src/docs/a.md"));
        assert!(!glob_matches("docs/**", "docsx/a.md"));
        assert!(glob_matches("**/*.md", "README.md"));
        assert!(glob_matches("**/*.md", "plugins/x/SKILL.md"));
        assert!(!glob_matches("**/*.md", "src/lib.rs"));
        assert!(glob_matches("plugins/**/SKILL.md", "plugins/SKILL.md"));
        assert!(glob_matches("plugins/**/SKILL.md", "plugins/a/b/SKILL.md"));
        assert!(glob_matches("**", "any/path"));
    }

    #[test]
    fn a_question_mark_is_one_character_and_the_rest_is_literal() {
        assert!(glob_matches("a?c", "abc"));
        assert!(glob_matches("a?c", "aéc"));
        assert!(!glob_matches("a?c", "ac"));
        assert!(!glob_matches("a?c", "a/c"));
        assert!(glob_matches("Cargo.toml", "Cargo.toml"));
        assert!(!glob_matches("Cargo.toml", "Cargo.lock"));
        assert!(!glob_matches("Cargo.toml", "x/Cargo.toml"));
    }

    #[test]
    fn out_of_scope_lists_what_no_glob_matches_and_nothing_without_globs() {
        let changed = globs(&["docs/a.md", "README.md", "src/lib.rs", "tests/cli.rs"]);
        assert_eq!(
            out_of_scope(&globs(&["docs/**", "*.md"]), &changed),
            globs(&["src/lib.rs", "tests/cli.rs"])
        );
        assert!(out_of_scope(&[], &changed).is_empty());
        assert!(out_of_scope(&globs(&["**"]), &changed).is_empty());
        assert_eq!(
            scope_violation_reason(&globs(&["src/lib.rs", "tests/cli.rs"])),
            "changed paths outside the task's --paths: src/lib.rs, tests/cli.rs"
        );
    }

    #[test]
    fn globs_must_be_relative_and_plain() {
        assert!(validate_path_globs(&globs(&["docs/**", "*.md", "a/b?c"])).is_ok());
        for (glob, reason) in [
            (" ", "it is blank"),
            ("/docs/**", "it must be relative to the repository root"),
            ("docs/../src", "it must not have a . or .. segment"),
            ("./docs", "it must not have a . or .. segment"),
            ("docs//a", "it must not have an empty segment"),
            ("docs/", "it must not have an empty segment"),
        ] {
            assert_eq!(
                validate_path_globs(&globs(&["ok", glob])),
                Err(DomainError::InvalidPathGlob {
                    glob: glob.to_owned(),
                    reason,
                })
            );
        }
        assert_eq!(
            DomainError::InvalidPathGlob {
                glob: "/x".into(),
                reason: "it must be relative to the repository root",
            }
            .to_string(),
            "invalid --paths glob \"/x\": it must be relative to the repository root"
        );
        assert_eq!(dedup_globs(&globs(&["a", "b", "a"])), globs(&["a", "b"]));
    }
}
