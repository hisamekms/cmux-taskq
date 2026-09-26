//! The work of a run's own session (worker, resume, revise) broken down by
//! what it spent its time on (task 514): the model thinking and writing,
//! the commands it ran by kind, its subagents, its waits, and idle time.
//! Derived from the session's transcript over the span, by fixed rules;
//! reading the transcript is the infrastructure's, this module is pure.
//!
//! Every millisecond of the span gets one category, by priority:
//! a foreground tool > a background command > a subagent > the model >
//! idle. A shell command is labelled by the heaviest thing it runs
//! ([`classify`]); a background command and an async subagent end at their
//! `<task-notification>`.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::transcript::{RecordRole, TranscriptRecord, millis_text};

pub const MODEL: &str = "model";
pub const E2E: &str = "e2e";
pub const LLVM_COV: &str = "llvm_cov";
pub const TEST: &str = "test";
pub const BUILD: &str = "build";
pub const FMT: &str = "fmt";
pub const WAIT: &str = "wait";
pub const DAGQ: &str = "dagq";
pub const GIT: &str = "git";
pub const OTHER_COMMAND: &str = "other_command";
/// Commands joined with `&&`, `||`, `;` or a newline that run two or more
/// heavy kinds (say fmt, clippy and llvm-cov in one line).
pub const CHAIN: &str = "chain";
/// Other tools (reading, editing, searching files).
pub const TOOL: &str = "tool";
pub const SUBAGENT: &str = "subagent";
pub const IDLE: &str = "idle";

/// Every category, in the order they are listed, which also breaks ties
/// between overlapping commands of the same priority.
pub const CATEGORIES: [&str; 14] = [
    MODEL,
    CHAIN,
    E2E,
    LLVM_COV,
    TEST,
    BUILD,
    FMT,
    WAIT,
    DAGQ,
    GIT,
    OTHER_COMMAND,
    TOOL,
    SUBAGENT,
    IDLE,
];

/// The heavy commands: counted and failed per kind, and listed in
/// `timeline`.
pub const HEAVY: [&str; 5] = [CHAIN, E2E, LLVM_COV, TEST, BUILD];

/// Tools that only wait (for a wakeup, a monitor, a background output).
const WAIT_TOOLS: [&str; 4] = ["ScheduleWakeup", "Monitor", "TaskOutput", "BashOutput"];

/// The category of a shell `command`: the heaviest of its parts, or
/// [`CHAIN`] when two or more parts are heavy of different kinds.
pub fn classify(command: &str) -> &'static str {
    let mut kinds: Vec<&'static str> = parts(command).map(rank).collect();
    kinds.sort_unstable_by_key(|kind| RANKS.iter().position(|k| k == kind));
    kinds.dedup();
    if kinds.iter().filter(|kind| HEAVY.contains(kind)).count() >= 2 {
        return CHAIN;
    }
    kinds.first().copied().unwrap_or(OTHER_COMMAND)
}

/// The kinds of a part, heaviest first.
const RANKS: [&str; 9] = [
    E2E,
    LLVM_COV,
    TEST,
    BUILD,
    FMT,
    WAIT,
    DAGQ,
    GIT,
    OTHER_COMMAND,
];

/// The parts of a command joined with `&&`, `||`, `;` or a newline.
fn parts(command: &str) -> impl Iterator<Item = &str> {
    command
        .split(['\n', ';'])
        .flat_map(|line| line.split("&&"))
        .flat_map(|part| part.split("||"))
        .map(str::trim)
        .filter(|part| !part.is_empty())
}

/// The first rule `text` matches: e2e > llvm-cov > test > build/clippy >
/// fmt > wait > dagq > git > other.
fn rank(text: &str) -> &'static str {
    let words: Vec<&str> = text.split_whitespace().collect();
    let has = |word: &str| words.contains(&word);
    let after_cargo = cargo_subcommand(&words);
    if words
        .windows(2)
        .any(|pair| pair[0] == "--test" && pair[1] == "e2e")
        || has("--test=e2e")
    {
        E2E
    } else if after_cargo == Some("llvm-cov") {
        LLVM_COV
    } else if matches!(after_cargo, Some("test" | "nextest")) {
        TEST
    } else if matches!(after_cargo, Some("build" | "clippy" | "check" | "run")) {
        BUILD
    } else if after_cargo == Some("fmt") {
        FMT
    } else if has("sleep")
        || has("uptime")
        || has("until")
        || (text.contains("sysctl") && text.contains("loadavg"))
    {
        WAIT
    } else if words.iter().any(|word| command_word(word) == Some("dagq")) {
        DAGQ
    } else if words.iter().any(|word| command_word(word) == Some("git")) {
        GIT
    } else {
        OTHER_COMMAND
    }
}

/// The name a word runs as a command: its file name, without the shell's
/// punctuation around it.
fn command_word(word: &str) -> Option<&str> {
    let word = word.trim_start_matches(['(', '|', '&', '`', '$']);
    word.rsplit('/').next().filter(|name| !name.is_empty())
}

/// The subcommand after the first `cargo` (a `+toolchain` skipped).
fn cargo_subcommand<'a>(words: &[&'a str]) -> Option<&'a str> {
    let at = words
        .iter()
        .position(|word| command_word(word) == Some("cargo"))?;
    words[at + 1..]
        .iter()
        .find(|word| !word.starts_with('+'))
        .copied()
}

/// Whether a part runs the whole test suite: `cargo test` (or `cargo
/// nextest run`) with only flags before `--`, none of them choosing a
/// target or filtering.
pub fn full_test(part: &str) -> bool {
    let words: Vec<&str> = part.split_whitespace().collect();
    let Some(at) = words
        .iter()
        .position(|word| command_word(word) == Some("cargo"))
    else {
        return false;
    };
    let mut rest = words[at + 1..]
        .iter()
        .copied()
        .skip_while(|word| word.starts_with('+'));
    match rest.next() {
        Some("test") => {}
        Some("nextest") if rest.next() == Some("run") => {}
        _ => return false,
    }
    const TARGETED: [&str; 9] = [
        "--test",
        "--lib",
        "--bin",
        "--bins",
        "--doc",
        "--example",
        "--package",
        "-p",
        "-E",
    ];
    rest.take_while(|word| !matches!(*word, "--" | "|" | "&" | ">") && !word.starts_with('>'))
        .filter(|word| !word.starts_with("2>"))
        .all(|word| {
            word.starts_with('-')
                && !TARGETED
                    .iter()
                    .any(|t| word == *t || word.starts_with(&format!("{t}=")))
        })
}

/// What a command runs that `integrate` also runs: the llvm-cov gate, the
/// whole test suite, the e2e test.
fn verification_classes(command: &str) -> Vec<&'static str> {
    let mut classes: Vec<&'static str> = parts(command)
        .filter_map(|part| match rank(part) {
            LLVM_COV => Some(LLVM_COV),
            E2E => Some(E2E),
            TEST if full_test(part) => Some("full_test"),
            _ => None,
        })
        .collect();
    classes.sort_unstable();
    classes.dedup();
    classes
}

/// One tool call of the span, with its times (unix milliseconds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub tool_use_id: String,
    pub tool: String,
    pub category: &'static str,
    pub start: i64,
    pub end: i64,
    /// Its end was seen (a result, or the notification of a background
    /// one); otherwise it is cut at the span's end.
    pub finished: bool,
    pub background: bool,
    pub exit_code: Option<i64>,
    /// `None` when its outcome is unknown.
    pub failed: Option<bool>,
    pub status: Option<String>,
    /// The shell command, for the run directory's `worktime.jsonl` only.
    pub command: Option<String>,
}

impl Command {
    fn priority(&self) -> u8 {
        if self.category == SUBAGENT {
            3
        } else if self.background {
            2
        } else {
            1
        }
    }

    /// The line of `worktime.jsonl` of this command.
    pub fn line(&self, span: &Value) -> Value {
        const MAX: usize = 300;
        let command = self.command.as_deref().map(|command| {
            let mut end = command.len().min(MAX);
            while !command.is_char_boundary(end) {
                end -= 1;
            }
            &command[..end]
        });
        json!({
            "opened_event_id": span["opened_event_id"],
            "kind": span["kind"],
            "attempt": span["attempt"],
            "tool": self.tool,
            "category": self.category,
            "start": millis_text(self.start),
            "end": millis_text(self.end),
            "secs": secs(self.end - self.start),
            "finished": self.finished,
            "background": self.background,
            "exit_code": self.exit_code,
            "failed": self.failed,
            "status": self.status,
            "command": command,
        })
    }
}

/// Whole seconds of `millis`, rounded.
fn secs(millis: i64) -> i64 {
    (millis + 500).div_euclid(1000)
}

/// The tool calls that start in the span `from`..`to` (unix
/// milliseconds), in the order they started. A call whose end was not
/// seen is cut at `to`.
pub fn commands(records: &[TranscriptRecord], from: i64, to: i64) -> Vec<Command> {
    let mut results = BTreeMap::new();
    let mut notices = BTreeMap::new();
    for record in records {
        for result in &record.tool_results {
            results
                .entry(result.tool_use_id.clone())
                .or_insert((record.at, result));
        }
        if let Some(notice) = &record.notification {
            notices
                .entry(notice.tool_use_id.clone())
                .or_insert((record.at, notice));
        }
    }
    let mut commands = Vec::new();
    for record in records.iter().filter(|r| !r.sidechain) {
        if record.at < from || record.at >= to {
            continue;
        }
        for tool_use in &record.tool_uses {
            let result = results.get(&tool_use.id).copied();
            let notice = notices.get(&tool_use.id).copied();
            let shell = tool_use.command.is_some();
            let (category, background, end) = if tool_use.name == "Agent" {
                (
                    SUBAGENT,
                    notice.is_some(),
                    notice.map(|(at, _)| at).or(result.map(|(at, _)| at)),
                )
            } else if shell && (tool_use.background || notice.is_some()) {
                // Run in the background from the start, or moved there
                // while it ran: it ends at its notice.
                (
                    classify(tool_use.command.as_deref().unwrap_or_default()),
                    true,
                    notice.map(|(at, _)| at),
                )
            } else if shell {
                (
                    classify(tool_use.command.as_deref().unwrap_or_default()),
                    false,
                    result.map(|(at, _)| at),
                )
            } else if WAIT_TOOLS.contains(&tool_use.name.as_str()) {
                (WAIT, false, result.map(|(at, _)| at))
            } else {
                (TOOL, false, result.map(|(at, _)| at))
            };
            let (exit_code, failed, status) = match (background, notice, result) {
                (true, Some((_, notice)), _) => (
                    notice.exit_code,
                    match notice.status.as_deref() {
                        Some("failed") => Some(true),
                        Some("completed") => Some(notice.exit_code.is_some_and(|code| code != 0)),
                        _ => None,
                    },
                    notice.status.clone(),
                ),
                (false, _, Some((_, result))) => (
                    result.exit_code,
                    Some(result.is_error || result.exit_code.is_some_and(|code| code != 0)),
                    None,
                ),
                _ => (None, None, None),
            };
            commands.push(Command {
                tool_use_id: tool_use.id.clone(),
                tool: tool_use.name.clone(),
                category,
                start: record.at,
                end: end.map_or(to, |end| end.clamp(record.at, to)),
                finished: end.is_some(),
                background,
                exit_code,
                failed,
                status,
                command: tool_use.command.clone(),
            });
        }
    }
    commands
}

/// The model's time: from a record of the main session to the `assistant`
/// record that follows it.
fn model_intervals(records: &[TranscriptRecord]) -> Vec<(i64, i64)> {
    let mut main: Vec<&TranscriptRecord> = records
        .iter()
        .filter(|r| !r.sidechain && r.role != RecordRole::Other)
        .collect();
    main.sort_by_key(|r| r.at);
    main.windows(2)
        .filter(|pair| pair[1].assistant)
        .map(|pair| (pair[0].at, pair[1].at))
        .collect()
}

/// The work of one span: seconds per category, the heavy commands'
/// runs and failures, and what it ran that `integrate` runs again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breakdown {
    pub total_millis: i64,
    pub millis: BTreeMap<&'static str, i64>,
    pub commands: Vec<Command>,
    /// Commands that ran a check `integrate` runs too (one of the task's
    /// verification commands: the llvm-cov gate, the whole `cargo test`,
    /// the e2e test).
    pub verification_repeats: usize,
    /// Commands that ran the whole `cargo test`.
    pub full_tests: usize,
    /// Commands that ran llvm-cov.
    pub llvm_cov_runs: usize,
}

/// The breakdown of the span `from`..`to` (unix milliseconds) of
/// `records`, whose task verifies with `verification`.
pub fn breakdown(
    records: &[TranscriptRecord],
    from: i64,
    to: i64,
    verification: &[String],
) -> Breakdown {
    let to = to.max(from);
    let commands = commands(records, from, to);
    let order = |category: &str| {
        CATEGORIES
            .iter()
            .position(|c| *c == category)
            .unwrap_or(CATEGORIES.len())
    };
    // (start, end, priority, category), cut to the span.
    let mut intervals: Vec<(i64, i64, u8, &'static str)> = commands
        .iter()
        .map(|c| (c.start, c.end, c.priority(), c.category))
        .chain(
            model_intervals(records)
                .into_iter()
                .map(|(start, end)| (start, end, 4, MODEL)),
        )
        .map(|(start, end, priority, category)| (start.max(from), end.min(to), priority, category))
        .filter(|(start, end, ..)| start < end)
        .collect();
    intervals
        .sort_by_key(|&(start, end, priority, category)| (priority, order(category), start, end));
    let mut bounds: Vec<i64> = intervals
        .iter()
        .flat_map(|&(start, end, ..)| [start, end])
        .chain([from, to])
        .collect();
    bounds.sort_unstable();
    bounds.dedup();
    let mut millis: BTreeMap<&'static str, i64> = BTreeMap::new();
    for pair in bounds.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        let category = intervals
            .iter()
            .find(|&&(s, e, ..)| s <= start && end <= e)
            .map_or(IDLE, |&(.., category)| category);
        *millis.entry(category).or_default() += end - start;
    }
    let wanted: Vec<&str> = verification
        .iter()
        .flat_map(|command| verification_classes(command))
        .collect();
    let shell = || commands.iter().filter_map(|c| c.command.as_deref());
    Breakdown {
        total_millis: to - from,
        millis,
        verification_repeats: shell()
            .filter(|command| {
                verification_classes(command)
                    .iter()
                    .any(|class| wanted.contains(class))
            })
            .count(),
        full_tests: shell()
            .filter(|command| parts(command).any(full_test))
            .count(),
        llvm_cov_runs: shell()
            .filter(|command| parts(command).any(|p| rank(p) == LLVM_COV))
            .count(),
        commands,
    }
}

impl Breakdown {
    /// The `work` of the span's `session_closed`: values only, no command
    /// and no path (the event rule of ADR A). `heavy` lists the heavy
    /// commands for `timeline`.
    pub fn payload(&self) -> Value {
        let secs_by: BTreeMap<&str, i64> = CATEGORIES
            .iter()
            .filter_map(|c| Some((*c, secs(*self.millis.get(c)?))))
            .filter(|(_, secs)| *secs > 0)
            .collect();
        let heavy: Vec<&Command> = self
            .commands
            .iter()
            .filter(|c| HEAVY.contains(&c.category))
            .collect();
        let mut counts: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for command in &heavy {
            let count = counts.entry(command.category).or_default();
            count.0 += 1;
            count.1 += usize::from(command.failed == Some(true));
        }
        json!({
            "total_secs": secs(self.total_millis),
            "secs": secs_by,
            "commands": counts
                .into_iter()
                .map(|(category, (runs, failed))| (category, json!({"runs": runs, "failed": failed})))
                .collect::<BTreeMap<_, _>>(),
            "verification_repeats": self.verification_repeats,
            "full_tests": self.full_tests,
            "llvm_cov_runs": self.llvm_cov_runs,
            "heavy": heavy
                .iter()
                .map(|c| json!({
                    "category": c.category,
                    "start": millis_text(c.start),
                    "end": millis_text(c.end),
                    "secs": secs(c.end - c.start),
                    "background": c.background,
                    "finished": c.finished,
                    "failed": c.failed,
                }))
                .collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::transcript::Transcript;

    const SESSION: &str = "11111111-1111-4111-8111-111111111111";
    const BASE: i64 = 1_790_000_000_000;

    fn at(secs: i64) -> String {
        millis_text(BASE + secs * 1000)
    }

    fn ms(secs: i64) -> i64 {
        BASE + secs * 1000
    }

    fn line(kind: &str, secs: i64, extra: Value) -> String {
        let mut value = json!({
            "type": kind,
            "timestamp": at(secs),
            "sessionId": SESSION,
            "isSidechain": false,
            "version": "2.1.283",
        });
        for (key, field) in extra.as_object().unwrap() {
            value[key] = field.clone();
        }
        value.to_string()
    }

    fn input(secs: i64, text: &str) -> String {
        line("user", secs, json!({"message": {"content": text}}))
    }

    fn call(secs: i64, id: &str, name: &str, input: Value) -> String {
        line(
            "assistant",
            secs,
            json!({"message": {"content": [
                {"type": "text", "text": "..."},
                {"type": "tool_use", "id": id, "name": name, "input": input},
            ]}}),
        )
    }

    fn bash(secs: i64, id: &str, command: &str, background: bool) -> String {
        call(
            secs,
            id,
            "Bash",
            json!({"command": command, "run_in_background": background}),
        )
    }

    fn result(secs: i64, id: &str, text: &str, error: bool) -> String {
        line(
            "user",
            secs,
            json!({"message": {"content": [
                {"type": "tool_result", "tool_use_id": id, "content": text, "is_error": error},
            ]}}),
        )
    }

    fn notice(secs: i64, id: &str, status: &str, summary: &str) -> String {
        let content = format!(
            "<task-notification>\n<task-id>x</task-id>\n<tool-use-id>{id}</tool-use-id>\n<status>{status}</status>\n<summary>{summary}</summary>\n</task-notification>"
        );
        line(
            "queue-operation",
            secs,
            json!({"operation": "enqueue", "content": content}),
        )
    }

    fn records(lines: &[String]) -> Vec<TranscriptRecord> {
        Transcript::parse(&lines.join("\n"), SESSION)
            .unwrap()
            .records
    }

    #[test]
    fn commands_are_classified_by_the_heaviest_thing_they_run() {
        assert_eq!(
            classify("cargo test --locked --test e2e -- --ignored 2>&1 | tail"),
            E2E
        );
        assert_eq!(
            classify("cargo llvm-cov nextest --locked --fail-under-lines 80"),
            LLVM_COV
        );
        assert_eq!(classify("cargo +nightly test --lib stats"), TEST);
        assert_eq!(classify("cargo nextest run"), TEST);
        assert_eq!(
            classify("cargo clippy --locked --all-targets -- -D warnings"),
            BUILD
        );
        assert_eq!(classify("cargo fmt --all --check"), FMT);
        assert_eq!(classify("sleep 30"), WAIT);
        assert_eq!(classify("~/.local/bin/dagq ask --run r"), DAGQ);
        assert_eq!(classify("git status && git diff"), GIT);
        assert_eq!(classify("ls src"), OTHER_COMMAND);
        // Naming llvm-cov is not running it.
        assert_eq!(classify("grep -n llvm-cov AGENTS.md"), OTHER_COMMAND);
        assert_eq!(classify("git commit -m 'cargo llvm-cov'"), GIT);
        // Two heavy kinds joined make a chain; fmt is not heavy.
        assert_eq!(
            classify("cargo fmt --all && cargo clippy --all-targets && cargo test --locked"),
            CHAIN
        );
        assert_eq!(classify("cargo fmt --all\ncargo clippy"), BUILD);
        assert_eq!(classify("cargo build; cargo build --release"), BUILD);
    }

    #[test]
    fn the_whole_suite_is_cargo_test_with_flags_only() {
        assert!(full_test("cargo test --locked"));
        assert!(full_test("cargo test --locked 2>&1 | tail -5"));
        assert!(full_test("cargo nextest run --workspace"));
        assert!(full_test("cargo test -- --ignored"));
        assert!(!full_test("cargo test --locked --test cli_stats"));
        assert!(!full_test("cargo test --lib domain::worktime"));
        assert!(!full_test("cargo test stats"));
        assert!(!full_test("cargo test -p=dagq"));
        assert!(!full_test("cargo build"));
        assert!(!full_test("ls"));
    }

    /// A session of several turns with foreground and background commands,
    /// an async subagent, a chain, and a command cut by the span's end.
    #[test]
    fn a_span_is_split_by_priority_and_background_work_ends_at_its_notice() {
        let lines = [
            input(0, "do the task"),
            // 0..10 the model; 10..70 a foreground clippy.
            bash(10, "t1", "cargo clippy --all-targets", false),
            result(70, "t1", "ok", false),
            // 70..80 the model; a background llvm-cov 80..380.
            bash(
                80,
                "t2",
                "cargo llvm-cov --locked --fail-under-lines 80",
                true,
            ),
            result(81, "t2", "Command running in background", false),
            // 81..90 the model; an async subagent 90..200.
            call(90, "t3", "Agent", json!({"description": "review"})),
            result(91, "t3", "Async agent launched", false),
            // 91..95 the model; a failing foreground test 95..125.
            bash(95, "t4", "cargo test --locked --test cli_stats", false),
            result(125, "t4", "Exit code 101\nfailures", true),
            call(130, "t5", "Read", json!({"file_path": "x"})),
            result(131, "t5", "text", false),
            line(
                "assistant",
                135,
                json!({"message": {"content": [{"type": "text", "text": "waiting"}]}}),
            ),
            notice(200, "t3", "completed", "Agent \"review\" finished"),
            input(200, "<task-notification>...</task-notification>"),
            notice(
                380,
                "t2",
                "failed",
                "Background command \"cargo llvm-cov\" failed with exit code 1",
            ),
            input(380, "<task-notification>...</task-notification>"),
            // 380..390 the model; a chain 390..500; a full test cut at 600.
            bash(
                390,
                "t6",
                "cargo fmt --all --check && cargo clippy && cargo test --locked",
                false,
            ),
            result(500, "t6", "ok", false),
            bash(550, "t7", "cargo test --locked 2>&1 | tail", false),
        ];
        // A foreground build moved to the background ends at its notice.
        let moved = [
            input(0, "go"),
            bash(1, "m", "cargo build", false),
            result(2, "m", "Command was moved to the background", false),
            notice(
                40,
                "m",
                "completed",
                "Background command \"cargo build\" completed (exit code 0)",
            ),
        ];
        let moved = breakdown(&records(&moved), ms(0), ms(50), &[]);
        assert!(moved.commands[0].background);
        assert_eq!(moved.commands[0].end, ms(40));
        assert_eq!(moved.commands[0].failed, Some(false));
        let records = records(&lines);
        let verification = vec![
            "cargo fmt --all --check".to_owned(),
            "cargo llvm-cov --locked --fail-under-lines 80".to_owned(),
        ];
        let work = breakdown(&records, ms(0), ms(600), &verification);
        let secs_of = |category: &str| work.millis.get(category).copied().unwrap_or(0) / 1000;
        assert_eq!(work.total_millis, 600_000);
        assert_eq!(work.millis.values().sum::<i64>(), 600_000);
        assert_eq!(secs_of(BUILD), 60);
        assert_eq!(secs_of(TEST), 30 + 50);
        assert_eq!(secs_of(CHAIN), 110);
        assert_eq!(secs_of(TOOL), 1);
        // The background llvm-cov takes 80..380 but for the foreground
        // test and Read; the model and the subagent are below it.
        assert_eq!(secs_of(LLVM_COV), (380 - 80) - 30 - 1);
        // The subagent is under the background command: none of its own.
        assert_eq!(secs_of(SUBAGENT), 0);
        assert_eq!(secs_of(MODEL), 10 + 10 + 10 + 50);
        assert_eq!(secs_of(IDLE), 0);

        let command = |id: &str| work.commands.iter().find(|c| c.tool_use_id == id).unwrap();
        assert!(command("t2").background);
        assert_eq!(command("t2").exit_code, Some(1));
        assert_eq!(command("t2").failed, Some(true));
        assert_eq!(command("t2").end, ms(380));
        assert_eq!(command("t3").category, SUBAGENT);
        assert!(command("t3").background);
        assert_eq!(command("t3").end, ms(200));
        assert_eq!(command("t4").exit_code, Some(101));
        assert_eq!(command("t4").failed, Some(true));
        assert_eq!(command("t1").failed, Some(false));
        assert!(!command("t7").finished);
        assert_eq!(command("t7").end, ms(600));
        assert_eq!(command("t7").failed, None);
        // llvm-cov repeats integrate's gate; the full test suite is not in
        // this task's verification.
        assert_eq!(work.verification_repeats, 1);
        assert_eq!(work.full_tests, 2);
        assert_eq!(work.llvm_cov_runs, 1);

        let payload = work.payload();
        assert_eq!(payload["total_secs"], 600);
        assert_eq!(payload["secs"]["build"], 60);
        assert!(payload["secs"].get("idle").is_none());
        assert_eq!(payload["commands"]["test"], json!({"runs": 2, "failed": 1}));
        assert_eq!(
            payload["commands"]["llvm_cov"],
            json!({"runs": 1, "failed": 1})
        );
        assert_eq!(
            payload["commands"]["chain"],
            json!({"runs": 1, "failed": 0})
        );
        assert!(payload["commands"].get("fmt").is_none());
        let heavy = payload["heavy"].as_array().unwrap();
        assert_eq!(heavy.len(), 5);
        assert_eq!(heavy[1]["category"], "llvm_cov");
        assert_eq!(heavy[1]["background"], true);
        assert_eq!(heavy[1]["secs"], 300);
        // No command text or path in the event's payload.
        assert!(!payload.to_string().contains("cargo"));

        let span = json!({"opened_event_id": 3, "kind": "worker", "attempt": 1});
        let line = command("t2").line(&span);
        assert_eq!(line["kind"], "worker");
        assert_eq!(line["category"], "llvm_cov");
        assert_eq!(line["status"], "failed");
        assert_eq!(line["secs"], 300);
        assert!(
            line["command"]
                .as_str()
                .unwrap()
                .starts_with("cargo llvm-cov")
        );
    }

    /// A resume goes on in the same transcript: only the calls that start
    /// in its span are its own, and the time before its first record and
    /// after an unanswered input is idle.
    #[test]
    fn a_span_takes_only_its_own_part_of_the_transcript() {
        let lines = [
            input(0, "first"),
            bash(5, "a", "cargo test --locked", false),
            result(50, "a", "ok", false),
            // The resume, in the same session.
            input(1000, "resume"),
            bash(1010, "b", "git status", false),
            result(1011, "b", "clean", false),
            line(
                "assistant",
                1020,
                json!({"message": {"content": [{"type": "text", "text": "done"}]}}),
            ),
        ];
        let records = records(&lines);
        let resume = breakdown(&records, ms(990), ms(1100), &[]);
        assert_eq!(resume.commands.len(), 1);
        assert_eq!(resume.commands[0].category, GIT);
        assert_eq!(resume.full_tests, 0);
        assert_eq!(resume.millis[&GIT], 1000);
        assert_eq!(resume.millis[&MODEL], 10_000 + 9000);
        assert_eq!(resume.millis[&IDLE], 10_000 + 80_000);
        let worker = breakdown(&records, ms(0), ms(100), &["cargo test --locked".into()]);
        assert_eq!(worker.verification_repeats, 1);
        assert_eq!(worker.millis[&TEST], 45_000);
        // An empty span.
        let empty = breakdown(&records, ms(5000), ms(4000), &[]);
        assert_eq!(empty.total_millis, 0);
        assert!(empty.millis.is_empty());
    }

    #[test]
    fn a_long_command_is_cut_on_a_char_boundary_in_its_line() {
        let command = Command {
            tool_use_id: "x".into(),
            tool: "Bash".into(),
            category: OTHER_COMMAND,
            start: 0,
            end: 1500,
            finished: true,
            background: false,
            exit_code: None,
            failed: Some(false),
            status: None,
            command: Some("あ".repeat(200)),
        };
        let line = command.line(&json!({}));
        assert_eq!(line["secs"], 2);
        assert_eq!(line["command"].as_str().unwrap().chars().count(), 100);
    }
}
