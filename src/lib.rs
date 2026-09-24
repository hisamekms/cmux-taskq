//! The dagq runtime in layers (ADR-0013):
//!
//! - [`domain`]: the aggregates (`Task`, `Goal`, `TaskRun`), their value
//!   types and the business decisions, with no I/O.
//! - [`application`]: the use cases and the ports (traits) they reach the
//!   outside through.
//! - [`infrastructure`]: the adapters that implement the ports (SQLite,
//!   Git, cmux, Claude Code, launchd, processes, files, clock and IDs).
//! - [`compose`]: the composition root, which builds the adapters and
//!   injects them into the use cases; `main` resolves the queue location,
//!   parses the CLI and prints what it returns.
//!
//! Outside the layers: [`view`] and [`watch`] shape the CLI's compact
//! output and the inbox's event reads, [`observer`] is the periodic
//! observation job, and [`runtime`] and [`lifecycle`] only re-export the
//! names the tests use from before the move.
pub mod application;
pub mod compose;
pub mod domain;
pub mod infrastructure;
pub mod lifecycle;
pub mod observer;
pub mod runtime;
pub mod view;
pub mod watch;

/// The version of this binary, recorded on every supervisor registration so
/// `up` can tell a supervisor of an older build from one of its own
/// (ADR-0014).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
