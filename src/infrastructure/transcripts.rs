//! Claude Code's transcripts (ADR-0048 decision 9): where the transcript of
//! a session is and how its lines are read. The format is Claude Code's
//! own and changes with its versions; this is the only module that knows
//! it (with `domain::transcript`), so a new format is fixed here.

use std::{
    fs,
    path::{Path, PathBuf},
};

use tracing::debug;

use crate::{
    application::{TranscriptSource, Transcripts},
    domain::transcript::{
        SESSION_UNKNOWN, TRANSCRIPT_MISSING, TRANSCRIPT_UNPARSABLE, Transcript, Unreadable,
    },
};

/// The transcripts under Claude Code's configuration directory.
#[derive(Debug, Clone)]
pub struct ClaudeTranscripts {
    config_dir: Option<PathBuf>,
}

#[cfg(test)]
thread_local! {
    /// The configuration directory a unit test reads, instead of the
    /// environment's.
    static TEST_CONFIG_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

impl ClaudeTranscripts {
    pub fn new(config_dir: Option<PathBuf>) -> Self {
        Self { config_dir }
    }

    /// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
    pub fn from_env() -> Self {
        #[cfg(test)]
        if let Some(dir) = TEST_CONFIG_DIR.with(|dir| dir.borrow().clone()) {
            return Self::new(Some(dir));
        }
        let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".claude")));
        Self::new(dir)
    }

    /// Make [`ClaudeTranscripts::from_env`] read `dir` on this thread.
    #[cfg(test)]
    pub(crate) fn use_config_dir_in_test(dir: &Path) {
        TEST_CONFIG_DIR.with(|cell| *cell.borrow_mut() = Some(dir.to_owned()));
    }

    /// The file of the transcript: the recorded path, else
    /// `projects/<cwd encoded>/<session id>.jsonl`, else that file name in
    /// any project directory.
    fn path(&self, source: &TranscriptSource, session_id: &str) -> Option<PathBuf> {
        if let Some(path) = &source.transcript_path {
            return Some(PathBuf::from(path));
        }
        let projects = self.config_dir.as_ref()?.join("projects");
        let file = format!("{session_id}.jsonl");
        if let Some(cwd) = &source.cwd {
            let path = projects.join(encode_cwd(cwd)).join(&file);
            if path.is_file() {
                return Some(path);
            }
        }
        fs::read_dir(&projects)
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path().join(&file))
            .find(|path| path.is_file())
    }
}

/// The directory name Claude Code gives the project of `cwd`: every
/// character but an ASCII letter or digit is `-`.
pub fn encode_cwd(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

impl Transcripts for ClaudeTranscripts {
    fn read(&self, source: &TranscriptSource) -> Result<Transcript, Unreadable> {
        let Some(session_id) = source.session_id.as_deref() else {
            return Err(Unreadable {
                code: SESSION_UNKNOWN,
                version: None,
                detail: "the span has no session id".into(),
            });
        };
        let Some(path) = self.path(source, session_id) else {
            return Err(Unreadable {
                code: TRANSCRIPT_MISSING,
                version: None,
                detail: format!("no transcript of session {session_id} was found"),
            });
        };
        let bytes = fs::read(&path).map_err(|error| Unreadable {
            code: TRANSCRIPT_MISSING,
            version: None,
            detail: format!("{}: {error}", path.display()),
        })?;
        let Ok(text) = String::from_utf8(bytes) else {
            return Err(Unreadable {
                code: TRANSCRIPT_UNPARSABLE,
                version: None,
                detail: format!("{} is not UTF-8", path.display()),
            });
        };
        let transcript = Transcript::parse(&text, session_id).map_err(|mut error| {
            error.detail = format!("{}: {}", path.display(), error.detail);
            error
        })?;
        if transcript.skipped > 0 {
            debug!(
                "transcript {}: {} line(s) that are not JSON skipped",
                path.display(),
                transcript.skipped
            );
        }
        Ok(transcript)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::transcript::{SESSION_MISMATCH, TRANSCRIPT_UNSUPPORTED};

    const SESSION: &str = "33333333-3333-4333-8333-333333333333";

    fn record(session: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"2026-09-26T00:00:00.000Z","sessionId":"{session}","message":{{"content":"hi"}}}}"#
        )
    }

    #[test]
    fn transcripts_are_found_by_cwd_by_session_id_or_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let transcripts = ClaudeTranscripts::new(Some(dir.path().to_owned()));
        let source = TranscriptSource {
            session_id: Some(SESSION.into()),
            cwd: Some("/work/tree.x".into()),
            transcript_path: None,
        };
        assert_eq!(
            transcripts.read(&source).unwrap_err().code,
            TRANSCRIPT_MISSING
        );

        let project = dir.path().join("projects").join("-work-tree-x");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join(format!("{SESSION}.jsonl")), record(SESSION)).unwrap();
        assert_eq!(transcripts.read(&source).unwrap().records.len(), 1);
        // Another cwd: found in whatever project holds the session.
        let elsewhere = TranscriptSource {
            cwd: Some("/private/work/tree.x".into()),
            ..source.clone()
        };
        assert_eq!(transcripts.read(&elsewhere).unwrap().records.len(), 1);

        let path = dir.path().join("given.jsonl");
        fs::write(&path, record("44444444-4444-4444-8444-444444444444")).unwrap();
        let given = TranscriptSource {
            transcript_path: Some(path.display().to_string()),
            ..source.clone()
        };
        assert_eq!(transcripts.read(&given).unwrap_err().code, SESSION_MISMATCH);
        fs::write(&path, "{\"unknown\":1}\n").unwrap();
        assert_eq!(
            transcripts.read(&given).unwrap_err().code,
            TRANSCRIPT_UNSUPPORTED
        );
        fs::write(&path, [0xff, 0xfe]).unwrap();
        assert_eq!(
            transcripts.read(&given).unwrap_err().code,
            TRANSCRIPT_UNPARSABLE
        );
        fs::write(&path, format!("{}\nnot json\n", record(SESSION))).unwrap();
        assert_eq!(transcripts.read(&given).unwrap().skipped, 1);

        let unknown = TranscriptSource::default();
        assert_eq!(
            transcripts.read(&unknown).unwrap_err().code,
            SESSION_UNKNOWN
        );
        let nowhere = ClaudeTranscripts::new(None);
        assert_eq!(nowhere.read(&source).unwrap_err().code, TRANSCRIPT_MISSING);
    }

    #[test]
    fn from_env_reads_the_test_directory() {
        let dir = tempfile::tempdir().unwrap();
        ClaudeTranscripts::use_config_dir_in_test(dir.path());
        assert_eq!(
            ClaudeTranscripts::from_env().config_dir.as_deref(),
            Some(dir.path())
        );
        assert_eq!(encode_cwd("/Users/a/.local/x_y"), "-Users-a--local-x-y");
    }
}
