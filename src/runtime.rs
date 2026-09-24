//! The names the runtime's use cases had before they moved to
//! `application` (ADR-0013), kept for the tests and the CLI: the entry
//! points are [`crate::compose`], the use cases and their types are in
//! `application`, and the supervisor log is in `infrastructure`.
pub use crate::application::{
    health::{DoctorReport, LeaseHealth, ProcessHealth, RunHealth, SupervisorHealth},
    integrate::{
        IntegrateTarget, integrate_logs, integrate_verify_log, next_integrate_attempt,
        register_follow_ups,
    },
    prompt::{
        PredecessorSummary, STOP_BACKGROUND, WORKER_READING, inbox_prompt, planner_prompt, prompt,
        siblings_in_progress,
    },
    rebind::REBIND_LOG,
    recording::{BACKEND_ERROR_CHARS, RecordingBackend, backend_failure_payload},
    review::review_logs_hint,
    supervise::{PromptKind, RunError, TRIAGE_TOOLS, detect_prompt, review_prompt},
};
pub use crate::compose::{
    SuperviseOptions, ask, doctor, integrate, rebind, recover, resume_session_with_provider,
    review, session, session_with_provider, stats, status, status_for, supervise,
    supervise_with_reviewer, triage_prompt,
};
pub use crate::infrastructure::run_files::SupervisorLog;
