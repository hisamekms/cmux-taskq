pub mod application;
pub mod domain;
pub mod infrastructure;
pub mod lifecycle;
pub mod runtime;
pub mod watch;

/// The version of this binary, recorded on every supervisor registration so
/// `up` can tell a supervisor of an older build from one of its own
/// (ADR-0014).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
