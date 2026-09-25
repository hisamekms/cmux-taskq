//! The idle marker a session's agent writes when it stops (ADR-0016): when
//! it was written, and what the agent's adapter read of it
//! ([`AgentSignals::idle_hook`]). The agent is idle when it finished a
//! response and left no background work running (task 147); a `/exit` sent
//! while background work runs stops at the agent's own dialog. The screen is
//! not read.

use super::*;

pub(super) struct IdleMarker {
    path: PathBuf,
    modified: SystemTime,
    hook: IdleHook,
}

impl IdleMarker {
    /// `None` when the hook never wrote one. Time and content come from one
    /// open file, so they belong to the same write (the hook replaces the
    /// marker by a rename).
    pub(super) fn read(
        files: &dyn RunFiles,
        signals: &dyn AgentSignals,
        path: &Path,
    ) -> Result<Option<Self>> {
        let Some((modified, bytes)) = files.read_stamped(path)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            path: path.to_owned(),
            modified,
            hook: signals.idle_hook(&bytes),
        }))
    }

    /// When the agent wrote it.
    pub(super) fn modified(&self) -> SystemTime {
        self.modified
    }

    /// The background tasks still `running` when the agent stopped.
    pub(super) fn background_tasks(&self) -> &[BackgroundTask] {
        &self.hook.background_tasks
    }

    /// Background work the agent left running when it stopped.
    pub(super) fn background_running(&self) -> bool {
        self.hook.background_running
    }

    /// Idle, by a marker written after `since`.
    pub(super) fn idle_since(&self, since: SystemTime) -> bool {
        !self.background_running() && self.modified > since
    }

    /// Evidence that the agent went idle after publishing the receipt: the
    /// marker is no older than the receipt. Markers from earlier turns (for
    /// example a question to the inbox) do not count.
    pub(super) fn idle_after_receipt(
        &self,
        files: &dyn RunFiles,
        receipt: &Path,
    ) -> Result<Option<Value>> {
        if self.background_running() {
            return Ok(None);
        }
        self.stopped_after_receipt(files, receipt)
    }

    /// Evidence that the agent stopped after publishing the receipt, idle
    /// or not (its background work still running).
    pub(super) fn stopped_after_receipt(
        &self,
        files: &dyn RunFiles,
        receipt: &Path,
    ) -> Result<Option<Value>> {
        let receipt_modified = files.modified(receipt)?;
        if self.modified < receipt_modified {
            return Ok(None);
        }
        let mut evidence = serde_json::Map::new();
        evidence.insert("marker_path".into(), json!(path_text(&self.path)?));
        evidence.insert("marker_modified".into(), json!(unix_seconds(self.modified)));
        evidence.insert(
            "receipt_modified".into(),
            json!(unix_seconds(receipt_modified)),
        );
        for (name, value) in &self.hook.evidence {
            evidence.insert((*name).into(), value.clone());
        }
        evidence.insert(
            "background_running".into(),
            json!(self.background_running()),
        );
        Ok(Some(Value::Object(evidence)))
    }
}

/// Whether the marker shows background work the agent left running.
pub(super) fn background_running(
    files: &dyn RunFiles,
    signals: &dyn AgentSignals,
    marker: &Path,
) -> Result<bool> {
    Ok(IdleMarker::read(files, signals, marker)?.is_some_and(|idle| idle.background_running()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::memory_files::MemoryFiles;

    /// An agent whose marker is `running` while background work runs and
    /// anything else once it is idle.
    struct Signals;

    impl AgentSignals for Signals {
        fn detect_prompt(&self, _: &str) -> Option<&'static str> {
            None
        }

        fn screen_excerpt(&self, screen: &str) -> String {
            screen.to_owned()
        }

        fn idle_hook(&self, content: &[u8]) -> IdleHook {
            IdleHook {
                background_running: content == b"running",
                background_tasks: Vec::new(),
                evidence: vec![("hook_event_name", json!("Stop"))],
            }
        }

        fn input_ready(&self, _: &str) -> bool {
            true
        }

        fn input_pending(&self, _: &str, _: &str) -> bool {
            false
        }

        fn working(&self, _: &str) -> bool {
            false
        }
    }

    fn idle_after_receipt(files: &MemoryFiles, receipt: &Path, marker: &Path) -> Option<Value> {
        IdleMarker::read(files, &Signals, marker)
            .unwrap()
            .and_then(|idle| idle.idle_after_receipt(files, receipt).unwrap())
    }

    #[test]
    fn idle_marker_is_idle_unless_background_work_runs() {
        let files = MemoryFiles::default();
        let marker = Path::new("/run/idle.json");
        let receipt = Path::new("/run/receipt.json");
        assert!(
            IdleMarker::read(&files, &Signals, marker)
                .unwrap()
                .is_none()
        );
        assert!(!background_running(&files, &Signals, marker).unwrap());
        assert!(idle_after_receipt(&files, receipt, marker).is_none());
        files.write(receipt, b"{}").unwrap();
        let before = files.now() - Duration::from_secs(60);
        for (content, running) in [("{}", false), ("running", true)] {
            files.write(marker, content.as_bytes()).unwrap();
            let idle = IdleMarker::read(&files, &Signals, marker).unwrap().unwrap();
            assert_eq!(idle.background_running(), running, "{content}");
            assert_eq!(
                background_running(&files, &Signals, marker).unwrap(),
                running
            );
            assert_eq!(idle.idle_since(before), !running, "{content}");
            assert!(!idle.idle_since(files.now() + Duration::from_secs(60)));
            assert_eq!(
                idle_after_receipt(&files, receipt, marker).is_some(),
                !running,
                "{content}"
            );
            // Past the wait, the stop counts whatever still runs.
            let stopped = idle
                .stopped_after_receipt(&files, receipt)
                .unwrap()
                .unwrap();
            assert_eq!(stopped["background_running"], running);
            assert_eq!(stopped["marker_path"], "/run/idle.json");
            assert_eq!(stopped["hook_event_name"], "Stop");
        }
        // Nor does a marker older than the receipt count.
        files.put(marker, files.now() - Duration::from_secs(3600), "{}");
        assert!(idle_after_receipt(&files, receipt, marker).is_none());
        // A receipt that cannot be read is an error, not idleness.
        let idle = IdleMarker::read(&files, &Signals, marker).unwrap().unwrap();
        assert!(
            idle.stopped_after_receipt(&files, Path::new("/run/none"))
                .is_err()
        );
    }
}
