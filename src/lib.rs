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
pub mod build_id;
pub mod compose;
pub mod domain;
pub mod infrastructure;
pub mod lifecycle;
pub mod migration_numbers;
pub mod observer;
pub mod runtime;
pub mod view;
pub mod watch;

/// The build identifier of this binary (ADR-0045 decision 2): `X.Y.Z` for a
/// release, `X.Y.Z-dev+<commit>[.dirty]` for a development build. `dagq
/// --version` prints it, and every supervisor registration records it so
/// `up` replaces a supervisor whose identifier differs from its own in any
/// part, the commit included.
pub const VERSION: &str = env!("DAGQ_BUILD_ID");
