//! The integration tests other than `tests/e2e.rs` and `tests/plugin.rs`, in
//! one test binary (ADR-0078): each file here is a module named after the
//! feature it tests, so `cargo test --locked --test it runtime_claim::` runs
//! the tests of one file. The helpers several files use are in
//! `tests/common` (shared with `tests/plugin.rs`), and those only the
//! runtime tests use are in `tests/it/runtime_support`.
#[path = "../common/mod.rs"]
mod common;
#[macro_use]
mod runtime_support;

mod cli_goals;
mod cli_kpi;
mod cli_proposals;
mod cli_read;
mod cli_roles;
mod cli_stats;
mod cli_tasks;
mod cli_version;
mod lifecycle_cmux;
mod lifecycle_down;
mod lifecycle_in_cmux;
mod lifecycle_install;
mod lifecycle_plan;
mod lifecycle_replace;
mod lifecycle_up;
mod location;
mod plan_review;
mod queue_dependencies;
mod queue_goals;
mod queue_migration;
mod queue_proposals;
mod queue_runs;
mod queue_schema;
mod queue_search;
mod queue_tasks;
mod related;
mod runtime_adopt;
mod runtime_ask;
mod runtime_claim;
mod runtime_claim_defer;
mod runtime_claim_hold;
mod runtime_evidence;
mod runtime_handoff;
mod runtime_integrate;
mod runtime_observer;
mod runtime_recheck;
mod runtime_repair;
mod runtime_resume;
mod runtime_review;
mod runtime_run_env;
mod runtime_session;
mod runtime_stale_receipt;
mod runtime_stall;
mod runtime_sweep;
mod runtime_triage;
mod runtime_waiting;
