//! Time limits for the waits of the integration tests (task 324).
//!
//! A test that waits on a thread, a child process or a stub agent can wait
//! forever when the condition never holds: `JoinHandle::join`,
//! `Child::wait` and `Command::output` have no deadline of their own. Such a
//! wait is wrapped in [`within`]; a monitor thread checks every open wait,
//! and one past its limit ends the whole test process with a failure that
//! names the test and what it waited for, rather than leaving
//! `cargo test | tail` hanging. Every test binary that uses it exits then,
//! since the stuck thread cannot be stopped from outside.
#![allow(dead_code)]

pub mod cli;

use std::{
    collections::HashMap,
    io::Write,
    process::{self, Command, ExitStatus, Output},
    sync::{
        LazyLock, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// A whole test: the fixtures open one for the test that uses them. Well
/// above the slowest test's minute or so under `cargo llvm-cov`'s load.
pub const TEST_LIMIT: Duration = Duration::from_secs(600);

/// A single step inside a test: a thread, a session or a process that is
/// expected to be done by then.
pub const STEP_LIMIT: Duration = Duration::from_secs(300);

/// The exit code of a timed-out run, the one a failed test binary has.
pub const TIMED_OUT: i32 = 101;

struct Open {
    test: String,
    what: String,
    started: Instant,
    limit: Duration,
}

static OPEN: LazyLock<Mutex<HashMap<u64, Open>>> = LazyLock::new(|| {
    thread::Builder::new()
        .name("test deadline monitor".into())
        .spawn(monitor)
        .unwrap();
    Mutex::new(HashMap::new())
});
static NEXT: AtomicU64 = AtomicU64::new(0);

fn open() -> MutexGuard<'static, HashMap<u64, Open>> {
    OPEN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An open wait; dropping it, when the wait returned or panicked, closes it.
#[must_use = "the wait is timed only while this is held"]
pub struct Waiting(u64);
impl Drop for Waiting {
    fn drop(&mut self) {
        open().remove(&self.0);
    }
}

/// Time what the calling thread waits for from now, `what` naming the
/// condition ("the supervisor thread to return").
pub fn within(limit: Duration, what: impl Into<String>) -> Waiting {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let test = thread::current()
        .name()
        .unwrap_or("(on a thread the test started)")
        .to_owned();
    open().insert(
        id,
        Open {
            test,
            what: what.into(),
            started: Instant::now(),
            limit,
        },
    );
    Waiting(id)
}

/// The whole test the calling thread runs, for a fixture to hold.
pub fn test() -> Waiting {
    within(TEST_LIMIT, "the test to finish")
}

/// `Command::output` and `Command::status` timed with [`STEP_LIMIT`]: the
/// test's child processes (the `dagq` binary, git, the hooks' shells).
pub trait Bounded {
    fn bounded_output(&mut self) -> std::io::Result<Output>;
    fn bounded_status(&mut self) -> std::io::Result<ExitStatus>;
}
impl Bounded for Command {
    fn bounded_output(&mut self) -> std::io::Result<Output> {
        let _waiting = within(STEP_LIMIT, exits(self));
        self.output()
    }
    fn bounded_status(&mut self) -> std::io::Result<ExitStatus> {
        let _waiting = within(STEP_LIMIT, exits(self));
        self.status()
    }
}

fn exits(command: &Command) -> String {
    let mut line = command.get_program().to_string_lossy().into_owned();
    for arg in command.get_args() {
        line.push(' ');
        line += &arg.to_string_lossy();
    }
    format!("`{line}` to exit")
}

fn monitor() {
    loop {
        thread::sleep(Duration::from_millis(250));
        let open = open();
        let Some(late) = open
            .values()
            .find(|wait| wait.started.elapsed() >= wait.limit)
        else {
            continue;
        };
        // Straight to the process's stderr: the test harness captures only
        // `print!`/`eprint!`, and the process ends before it would print.
        let mut report = format!(
            "\ntest {} timed out: {} did not happen within {:?}\n",
            late.test, late.what, late.limit
        );
        let mut others: Vec<_> = open
            .values()
            .filter(|wait| !std::ptr::eq(*wait, late))
            .collect();
        others.sort_by_key(|wait| wait.started);
        for wait in others {
            report += &format!(
                "  also waiting: test {} for {} (for {:?})\n",
                wait.test,
                wait.what,
                wait.started.elapsed()
            );
        }
        let _ = std::io::stderr().lock().write_all(report.as_bytes());
        process::exit(TIMED_OUT);
    }
}
