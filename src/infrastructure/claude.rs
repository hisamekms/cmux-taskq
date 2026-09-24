//! What Claude Code shows and writes that the supervisor reads of a live
//! session ([`AgentSignals`]): the dialogs of its TUI on the screen
//! (ADR-0019 decision 6) and the input its `Stop` hook writes to the idle
//! marker (ADR-0016, task 147). The formats are Claude Code's own; the
//! supervisor only learns the kind of a dialog, an excerpt of the screen
//! and whether background work was left running.

use serde_json::Value;

use super::adapters::ClaudeCode;
use crate::application::{AgentSignals, IdleHook};

/// `prompt_waiting` carries this many last non-empty lines of the screen.
const PROMPT_EXCERPT_LINES: usize = 15;

/// Only this many last non-empty lines are searched for a dialog: a dialog
/// sits at the bottom, and text higher up is usually the work itself.
const PROMPT_SCAN_LINES: usize = 30;

/// Another numbered option counts within this many lines of the `❯` one.
const OPTION_REACH: usize = 3;

/// Which dialog of the agent's TUI holds the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// The folder trust question.
    Trust,
    /// A `❯`-marked choice among numbered options (a plugin
    /// recommendation, the auto mode notice, ...).
    Choice,
    /// A footer such as `Enter to confirm · Esc to cancel` alone.
    Confirm,
}

impl PromptKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trust => "trust",
            Self::Choice => "choice",
            Self::Confirm => "confirm",
        }
    }
}

/// Whether the bottom of a screen shows a dialog: a line starting with `Do
/// you trust` or an option offering to trust the folder, a line starting
/// with `❯` and a numbered option within three lines of another numbered
/// option, or a
/// line starting with `Enter to confirm` or `Esc to cancel`. Box borders are
/// ignored, and a phrase inside other text (a quote, code) does not count.
pub fn detect_prompt(screen: &str) -> Option<PromptKind> {
    let lines: Vec<&str> = screen
        .lines()
        .map(strip_frame)
        .filter(|line| !line.is_empty())
        .collect();
    let tail = &lines[lines.len().saturating_sub(PROMPT_SCAN_LINES)..];
    if tail.iter().any(|line| {
        line.starts_with("Do you trust")
            || option_text(line).is_some_and(|text| text.contains("trust this folder"))
    }) {
        return Some(PromptKind::Trust);
    }
    // An option's text can wrap, so another option may be a few lines away.
    let is_option = |i: usize| tail.get(i).is_some_and(|line| option_text(line).is_some());
    let near = |i: usize| {
        (i.saturating_sub(OPTION_REACH)..=i + OPTION_REACH).any(|j| j != i && is_option(j))
    };
    if (0..tail.len()).any(|i| tail[i].starts_with('❯') && is_option(i) && near(i)) {
        return Some(PromptKind::Choice);
    }
    tail.iter()
        .any(|line| line.starts_with("Enter to confirm") || line.starts_with("Esc to cancel"))
        .then_some(PromptKind::Confirm)
}

fn strip_frame(line: &str) -> &str {
    line.trim_matches(|c: char| c.is_whitespace() || matches!(c, '│' | '┃' | '║' | '|'))
}

/// The text of a numbered option line (`1. Yes`, `❯ 2. No`), if it is one.
fn option_text(line: &str) -> Option<&str> {
    let line = line.strip_prefix('❯').unwrap_or(line).trim_start();
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    let rest = line[digits..].strip_prefix(". ")?;
    (digits > 0).then_some(rest)
}

/// The last `count` non-empty lines of a screen, right-trimmed.
fn screen_tail(screen: &str, count: usize) -> String {
    let lines: Vec<&str> = screen
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect();
    lines[lines.len().saturating_sub(count)..].join("\n")
}

/// The idle marker is the input of Claude Code's `Stop` hook, as JSON.
/// Claude Code lists the background tasks of the turn in `background_tasks`,
/// and a `/exit` sent while one is `running` stops at its "Background work
/// is running" dialog, which stays until someone answers it. When the work
/// ends, Claude Code takes the turn up again and the hook writes a new
/// marker. A hook input without `background_tasks` (an older Claude Code),
/// or one that is not JSON, counts as idle.
pub fn idle_hook(content: &[u8]) -> IdleHook {
    let hook: Value = serde_json::from_slice(content).unwrap_or(Value::Null);
    let background_running = hook
        .get("background_tasks")
        .and_then(Value::as_array)
        .is_some_and(|tasks| tasks.iter().any(|task| task["status"] == "running"));
    let field = |name: &'static str| (name, hook.get(name).cloned().unwrap_or(Value::Null));
    IdleHook {
        background_running,
        evidence: vec![
            field("hook_event_name"),
            field("session_id"),
            field("stop_hook_active"),
        ],
    }
}

impl AgentSignals for ClaudeCode {
    fn detect_prompt(&self, screen: &str) -> Option<&'static str> {
        detect_prompt(screen).map(PromptKind::as_str)
    }

    fn screen_excerpt(&self, screen: &str) -> String {
        screen_tail(screen, PROMPT_EXCERPT_LINES)
    }

    fn idle_hook(&self, content: &[u8]) -> IdleHook {
        idle_hook(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TRUST: &str = "\
╭──────────────────────────────────────────────────────────────────────╮
│ Do you trust the files in this folder?                               │
│                                                                      │
│ /Users/me/.local/share/dagq/0123/runs/abcd/worktree                  │
│                                                                      │
│ Claude Code may read, write, or execute files contained in this      │
│ directory. This can pose security risks, so only use files from      │
│ trusted sources.                                                     │
│                                                                      │
│ ❯ 1. Yes, proceed                                                    │
│   2. No, exit                                                        │
│                                                                      │
╰──────────────────────────────────────────────────────────────────────╯
   Enter to confirm · Esc to exit
";

    const LSP_PLUGIN: &str = "\
 ✻ Welcome to Claude Code!

 Plugin recommendation

 This project uses Rust. The rust-analyzer LSP plugin gives Claude
 go-to-definition and diagnostics.

   1. Install rust-analyzer-lsp
 ❯ 2. Not now
   3. Don't suggest this again

 Enter to confirm · Esc to cancel


";

    const AUTO_MODE: &str = "\
> Implement the task

⏺ Reading the repository instructions.

────────────────────────────────────────────────────────────────────
 Auto mode is available

 Claude can run commands and edit files without asking each time,
 with a classifier that stops risky actions.

 ❯ 1. Yes, turn on auto mode
   2. No, keep asking

 Esc to cancel
";

    const WORK: &str = "\
⏺ Bash(cargo test --locked)
  ⎿  test result: ok. 42 passed; 0 failed
     grep -n \"Esc to cancel\" src/runtime.rs
     let text = \"Do you trust the files in this folder?\";

⏺ Update(src/runtime.rs)
  ⎿  Updated src/runtime.rs with 3 additions
     1. Added the check
     2. Added the test

✽ Compiling… (esc to interrupt)

╭──────────────────────────────────────────────────────────────────────╮
│ ❯ run the tests again                                                │
╰──────────────────────────────────────────────────────────────────────╯
  ? for shortcuts
";

    #[test]
    fn detect_prompt_finds_the_three_dialogs() {
        assert_eq!(detect_prompt(TRUST), Some(PromptKind::Trust));
        assert_eq!(detect_prompt(LSP_PLUGIN), Some(PromptKind::Choice));
        assert_eq!(detect_prompt(AUTO_MODE), Some(PromptKind::Choice));
        let newer_trust = "│ Quick safety check: Is this a project you created or one you trust?\n│ ❯ 1. Yes, I trust this folder\n│   2. No, exit\n";
        assert_eq!(detect_prompt(newer_trust), Some(PromptKind::Trust));
        let wrapped = "Allow this edit?\n❯ 1. Yes, and don't ask again for edits in\n     /Users/me/worktree\n  2. No\n";
        assert_eq!(detect_prompt(wrapped), Some(PromptKind::Choice));
        assert_eq!(
            detect_prompt("Save changes?\n  Enter to confirm · Esc to cancel\n"),
            Some(PromptKind::Confirm)
        );
    }

    #[test]
    fn detect_prompt_ignores_a_working_session() {
        assert_eq!(detect_prompt(WORK), None);
        assert_eq!(detect_prompt(""), None);
        // A single marked option is not a choice among options.
        assert_eq!(detect_prompt("❯ 1. only line\n"), None);
        // A dialog scrolled far above the bottom no longer counts.
        let scrolled = format!("{AUTO_MODE}{}", "output line\n".repeat(PROMPT_SCAN_LINES));
        assert_eq!(detect_prompt(&scrolled), None);
    }

    #[test]
    fn screen_tail_keeps_the_last_non_empty_lines() {
        assert_eq!(screen_tail("a  \n\nb\nc\n\n\n", 2), "b\nc");
        assert_eq!(screen_tail("a\n", 15), "a");
        assert_eq!(option_text("❯ 12. Twelve"), Some("Twelve"));
        assert_eq!(option_text(". none"), None);
        assert_eq!(PromptKind::Confirm.as_str(), "confirm");
    }

    #[test]
    fn idle_hook_reads_background_tasks_of_the_stop_hook_input() {
        let claude = ClaudeCode {
            executable: "claude".into(),
        };
        for (hook, running) in [
            // An older Claude Code writes no `background_tasks`.
            (json!({"hook_event_name": "Stop"}), false),
            (json!({"background_tasks": []}), false),
            (
                json!({"background_tasks": [{"id": "b1", "status": "completed"}]}),
                false,
            ),
            (
                json!({"background_tasks": [
                    {"id": "b1", "status": "completed"},
                    {"id": "b2", "type": "shell", "status": "running"}
                ]}),
                true,
            ),
        ] {
            let idle = claude.idle_hook(hook.to_string().as_bytes());
            assert_eq!(idle.background_running, running, "{hook}");
        }
        let idle = claude.idle_hook(
            json!({"hook_event_name": "Stop", "session_id": "s1", "stop_hook_active": false})
                .to_string()
                .as_bytes(),
        );
        assert_eq!(
            idle.evidence,
            vec![
                ("hook_event_name", json!("Stop")),
                ("session_id", json!("s1")),
                ("stop_hook_active", json!(false)),
            ]
        );
        // A marker that is not JSON still tells the agent stopped.
        let idle = claude.idle_hook(b"not json");
        assert!(!idle.background_running);
        assert_eq!(idle.evidence[0], ("hook_event_name", Value::Null));
        assert_eq!(
            claude.detect_prompt("Save changes?\n  Esc to cancel\n"),
            Some("confirm")
        );
        assert_eq!(claude.screen_excerpt("a\n\nb\n"), "a\nb");
    }
}
