//! The names the runtime's use cases had before they moved to
//! `application` (ADR-0013), kept for the tests and the CLI: the entry
//! points are [`crate::compose`], the use cases and their types are in
//! `application`, and the log subscriber is in `infrastructure::telemetry`. The
//! run-directory helpers read the local file system.
pub use crate::application::{
    health::{DoctorReport, LeaseHealth, ProcessHealth, RunHealth, SupervisorHealth},
    integrate::{IntegrateTarget, integrate_verify_log, register_follow_ups},
    prompt::{
        GoalPredecessorSummary, PredecessorSummary, STOP_BACKGROUND, TRIAGE_TOOLS, WORKER_READING,
        inbox_prompt, planner_prompt, prompt, review_prompt, siblings_in_progress,
    },
    rebind::REBIND_LOG,
    recording::{BACKEND_ERROR_CHARS, RecordingBackend, backend_failure_payload},
    supervise::{RunError, SUPERVISOR_HANDED_OFF},
};
pub use crate::compose::{
    OneShot, SuperviseOptions, ask, doctor, ended_run_material, integrate, rebind, recover,
    resume_session_with_provider, review, session, session_with_provider, stats, status,
    status_for, supervise, supervise_with_reviewer,
};
pub use crate::infrastructure::claude::{PromptKind, detect_prompt};

use std::path::{Path, PathBuf};

use crate::{
    application::{integrate, review},
    infrastructure::run_files::LocalRunFiles,
};

/// [`integrate::integrate_logs`] on the local file system.
pub fn integrate_logs(run_dir: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    integrate::integrate_logs(&LocalRunFiles, run_dir)
}

/// [`integrate::next_integrate_attempt`] on the local file system.
pub fn next_integrate_attempt(run_dir: &Path) -> u32 {
    integrate::next_integrate_attempt(&LocalRunFiles, run_dir)
}

/// [`review::review_logs_hint`] on the local file system.
pub fn review_logs_hint(run_dir: Option<&str>) -> String {
    review::review_logs_hint(&LocalRunFiles, run_dir)
}
