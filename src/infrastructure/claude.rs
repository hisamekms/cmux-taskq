//! What Claude Code shows and writes that the supervisor reads of a live
//! session ([`AgentSignals`]): the dialogs of its TUI on the screen
//! (ADR-0019 decision 6) and the input its `Stop` hook writes to the idle
//! marker (ADR-0016, task 147), its input box (task 285) and the dialogs
//! the supervisor answers by rule (ADR-0047 decision 29). The formats
//! are Claude Code's own; the supervisor only learns the kind of a dialog,
//! an excerpt of the screen, whether background work was left running,
//! whether the input box is drawn, what it still holds and whether the
//! agent works.

use serde_json::Value;

use super::adapters::ClaudeCode;
use crate::{
    application::{AgentSignals, DialogAnswer, IdleHook, KnownDialog},
    domain::stall::BackgroundTask,
};

/// `prompt_waiting` carries this many last non-empty lines of the screen.
const PROMPT_EXCERPT_LINES: usize = 15;

/// Only this many last non-empty lines are searched for a dialog: a dialog
/// sits at the bottom, and text higher up is usually the work itself.
const PROMPT_SCAN_LINES: usize = 30;

/// Another numbered option counts within this many lines of the `❯` one.
const OPTION_REACH: usize = 3;

/// The input box closes within this many non-empty lines of the bottom:
/// under it Claude Code draws only its hints, the status line and the menu
/// of slash commands a `/` opens.
const INPUT_FOOTER_LINES: usize = 14;

/// This many last letters and digits of a text typed into the input box
/// are looked for in it: wrapping and `cmux send`'s rewriting of the text
/// (one line, slashes for backslashes) change the rest.
const INPUT_TAIL_CHARS: usize = 24;

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

/// The title line of the confirmation Claude Code shows for `/exit` while
/// background work runs.
const BACKGROUND_WORK_TITLE: &str = "Background work is running";

/// Its option that stops the background work and exits.
const EXIT_AND_STOP: &str = "Exit and stop tasks";

/// The title of the Settings panel, followed by its tabs.
const SETTINGS_TITLE: &str = "Settings:";

/// The footer of the Settings panel, under its title.
const SETTINGS_FOOTER: &str = "Esc to";

/// The tabs of the Settings panel; two on the title line mark it.
const SETTINGS_TABS: &[&str] = &["Status", "Config", "Usage"];

/// The dialog of the fixed list (ADR-0047 decision 29) at the bottom of a
/// screen with no input box under it, and the keys that answer it:
///
/// - the "Background work is running" confirmation: a title line starting
///   with it and, below, a `❯`-marked choice among numbered options one of
///   which starts with "Exit and stop tasks". The keys move the mark to that
///   option (`down` or `up`, as many as it is away) and press `enter`.
/// - the Settings panel (`/status`, `/usage`, ...): a line starting with
///   `Settings:` that names two of its tabs, and below it a line starting
///   with `Esc to`. The key is `escape`.
///
/// Anything else, a dialog or not, is `None`: a changed screen gets no key.
pub fn known_dialog(screen: &str) -> Option<DialogAnswer> {
    if input_box(screen).is_some() {
        return None;
    }
    let lines: Vec<&str> = screen
        .lines()
        .map(strip_frame)
        .filter(|line| !line.is_empty())
        .collect();
    let tail = &lines[lines.len().saturating_sub(PROMPT_SCAN_LINES)..];
    if let Some(title) = tail
        .iter()
        .rposition(|line| line.starts_with(BACKGROUND_WORK_TITLE))
    {
        let options: Vec<(bool, &str)> = tail[title + 1..]
            .iter()
            .filter_map(|line| option_text(line).map(|text| (line.starts_with('❯'), text)))
            .collect();
        let marked = options.iter().position(|(marked, _)| *marked)?;
        let target = options
            .iter()
            .position(|(_, text)| text.starts_with(EXIT_AND_STOP))?;
        let step = if target > marked { "down" } else { "up" };
        let mut keys = vec![step; target.abs_diff(marked)];
        keys.push("enter");
        return Some(DialogAnswer {
            dialog: KnownDialog::BackgroundWork,
            keys,
        });
    }
    let title = tail.iter().rposition(|line| {
        line.strip_prefix(SETTINGS_TITLE).is_some_and(|rest| {
            SETTINGS_TABS
                .iter()
                .filter(|tab| rest.contains(*tab))
                .count()
                >= 2
        })
    })?;
    tail[title + 1..]
        .iter()
        .any(|line| line.starts_with(SETTINGS_FOOTER))
        .then(|| DialogAnswer {
            dialog: KnownDialog::SettingsPanel,
            keys: vec!["escape"],
        })
}

/// How Claude Code reports a login that ran out or was never there (task
/// 266): its error line, under the tool call or the prompt it failed, starts
/// with one of these. Text that merely contains them (a grep, a test's
/// output, this file on the screen) does not count.
const AUTH_ERRORS: &[&str] = &[
    "API Error: 401",
    "Invalid API key",
    "OAuth token has expired",
    "OAuth token revoked",
];

/// Whether the bottom of a screen shows the session stopped at a login
/// that ran out (ADR-0047 decision 42): one of the last lines, box borders
/// and the `⎿` of a result ignored, starts with one of [`AUTH_ERRORS`] and
/// asks for `/login`.
pub fn auth_required(screen: &str) -> bool {
    let lines: Vec<&str> = screen
        .lines()
        .map(strip_frame)
        .filter(|line| !line.is_empty())
        .collect();
    lines[lines.len().saturating_sub(PROMPT_SCAN_LINES)..]
        .iter()
        .map(|line| line.trim_start_matches(['⎿', '⏺', ' ']))
        .any(|line| {
            AUTH_ERRORS.iter().any(|error| line.starts_with(error)) && line.contains("/login")
        })
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

/// A horizontal rule or the top or bottom border of a box, which Claude
/// Code draws above and below its input box.
fn is_rule(line: &str) -> bool {
    let mut chars = line.chars();
    matches!(
        (chars.next(), chars.next()),
        (
            Some('─' | '━' | '═' | '╭' | '╰' | '┌' | '└'),
            Some('─' | '━' | '═')
        )
    )
}

/// The lines of the input box at the bottom of a screen, the prompt line
/// (`❯`, `>` or the shell mode's `!`, frame stripped) first: the lines
/// between the last rule within [`INPUT_FOOTER_LINES`] of the bottom and
/// the rule above it, the first of them a prompt line. `None` while the
/// TUI is not drawn yet (the shell's own `❯` line has no rule above it),
/// when a dialog replaced the box, or once Claude Code exited and left its
/// last frame above `Resume this session with:`.
fn input_box(screen: &str) -> Option<Vec<&str>> {
    let lines: Vec<&str> = screen
        .lines()
        .map(strip_frame)
        .filter(|line| !line.is_empty())
        .collect();
    let close = (lines.len().saturating_sub(INPUT_FOOTER_LINES + 1)..lines.len())
        .rev()
        .find(|&i| is_rule(lines[i]))?;
    if lines[close + 1..]
        .iter()
        .any(|line| line.contains("Resume this session with"))
    {
        return None;
    }
    let open = (0..close).rev().find(|&i| is_rule(lines[i]))? + 1;
    let prompt = lines.get(open).filter(|_| open < close)?;
    (prompt.starts_with(['❯', '>', '!']) && option_text(prompt).is_none())
        .then(|| lines[open..close].to_vec())
}

/// Whether Claude Code's input box is drawn and no dialog is on the
/// screen: the session takes what is typed now. A booting session (the
/// shell's line, the welcome banner alone) is not ready.
pub fn input_ready(screen: &str) -> bool {
    input_box(screen).is_some() && detect_prompt(screen).is_none()
}

/// Whether the input box still holds `text` typed into it (its last
/// letters and digits, however wrapped), or a paste Claude Code collapsed
/// to `[Pasted text #N ...]`: Enter did not submit it. A screen without the
/// box holds nothing.
pub fn input_pending(screen: &str, text: &str) -> bool {
    let Some(lines) = input_box(screen) else {
        return false;
    };
    let content = lines.join(" ");
    let content = content.trim_start_matches(['❯', '>', '!']).trim();
    if content.contains("[Pasted text") {
        return true;
    }
    let alnum =
        |text: &str| -> Vec<char> { text.chars().filter(|c| c.is_alphanumeric()).collect() };
    let typed = alnum(text);
    let tail: String = typed[typed.len().saturating_sub(INPUT_TAIL_CHARS)..]
        .iter()
        .collect();
    !tail.is_empty()
        && alnum(content)
            .into_iter()
            .collect::<String>()
            .contains(&tail)
}

/// Whether the agent is at work: Claude Code shows `esc to interrupt`
/// next to its spinner while it runs a turn.
pub fn agent_working(screen: &str) -> bool {
    let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(PROMPT_SCAN_LINES)..]
        .iter()
        .any(|line| line.to_lowercase().contains("esc to interrupt"))
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
    let text = |task: &Value, name: &str| task[name].as_str().unwrap_or_default().to_owned();
    let background_tasks: Vec<BackgroundTask> = hook
        .get("background_tasks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|task| task["status"] == "running")
        .map(|task| BackgroundTask {
            id: text(task, "id"),
            description: text(task, "description"),
            command: text(task, "command"),
        })
        .collect();
    let field = |name: &'static str| (name, hook.get(name).cloned().unwrap_or(Value::Null));
    IdleHook {
        background_running: !background_tasks.is_empty(),
        background_tasks,
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

    fn auth_required(&self, screen: &str) -> bool {
        auth_required(screen)
    }

    fn screen_excerpt(&self, screen: &str) -> String {
        screen_tail(screen, PROMPT_EXCERPT_LINES)
    }

    fn idle_hook(&self, content: &[u8]) -> IdleHook {
        idle_hook(content)
    }

    fn input_ready(&self, screen: &str) -> bool {
        input_ready(screen)
    }

    fn input_pending(&self, screen: &str, text: &str) -> bool {
        input_pending(screen, text)
    }

    fn working(&self, screen: &str) -> bool {
        agent_working(screen)
    }

    fn known_dialog(&self, screen: &str) -> Option<DialogAnswer> {
        known_dialog(screen)
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

    /// The runner's shell line before Claude Code draws anything: the
    /// shell's own `❯` has no rule above it.
    const BOOT: &str = "\
Last login: Thu Sep 24 22:27:53 on ttys008
worktree on  dagq/f8f7c65d is 📦 v0.3.0 via 🦀 v1.93.0
❯ '/runs/f8f7c65d/runner' '--db' '/queue.db' 'session' '--run' 'f8f7c65d' '--resume'
";

    /// The welcome banner of a session still loading its conversation.
    const BOOT_BANNER: &str = "\
❯ '/runs/f8f7c65d/runner' '--db' '/queue.db' 'session' '--run' 'f8f7c65d' '--resume'
╭───────────────────────────────────────────────────╮
│ ✻ Welcome to Claude Code!                         │
│                                                   │
│   cwd: /runs/f8f7c65d/worktree                    │
╰───────────────────────────────────────────────────╯
";

    const READY: &str = "\
⏺ The receipt is written; the run is ready for review.

──────────────────────────────────────────────────────────────────────
❯\u{a0}
──────────────────────────────────────────────────────────────────────
  ⏵⏵ auto mode on (shift+tab to cycle)
";

    /// Claude Code 1.x draws the input box as a box.
    const READY_BOXED: &str = "\
╭──────────────────────────────────────────────────────────────────────╮
│ >                                                                    │
╰──────────────────────────────────────────────────────────────────────╯
  ? for shortcuts
";

    /// The resolution request, pasted and wrapped, with Enter taken as part
    /// of the paste.
    const LONG_PENDING: &str = "\
──────────────────────────────────────────────────────────────────────
❯ dagq: integrate could not land run f8f7c65d (task 221) and returned n
  eeds_session. Reason: rebase conflicted in src/runtime.rs ... Do not m
  erge or push. When done, report briefly and stop; do not run /exit.
──────────────────────────────────────────────────────────────────────
  ⏵⏵ auto mode on (shift+tab to cycle)
";

    const PASTED: &str = "\
──────────────────────────────────────────────────────────────────────
❯ [Pasted text #1 +3 lines]
──────────────────────────────────────────────────────────────────────
";

    /// `/exit` typed after the paste, and the menu of slash commands.
    const EXIT_PENDING: &str = "\
──────────────────────────────────────────────────────────────────────
❯ /exit
──────────────────────────────────────────────────────────────────────
  /exit                 Exit the REPL
  /export               Export the current conversation
";

    /// Claude Code exited on `/exit`: its last frame stays above the
    /// shell's lines.
    const EXITED: &str = "\
──────────────────────────────────────────────────────────────────────
❯ /exit
──────────────────────────────────────────────────────────────────────

Resume this session with:
claude --resume 68a96a60-1826-461e-8002-40690588884d
worktree on  dagq/68a96a60 took 8h32m49s
❯
";

    const REQUEST: &str = "dagq: integrate could not land run f8f7c65d (task 221) and returned needs_session.\nReason: rebase conflicted in src/runtime.rs ...\nDo not merge or push. When done, report briefly and stop; do not run /exit.";

    #[test]
    fn input_ready_waits_for_the_input_box_without_a_dialog() {
        assert!(!input_ready(BOOT));
        assert!(!input_ready(BOOT_BANNER));
        assert!(!input_ready(""));
        assert!(input_ready(READY));
        assert!(input_ready(READY_BOXED));
        assert!(input_ready(LONG_PENDING));
        assert!(input_ready(EXIT_PENDING));
        // A working session takes input too.
        assert!(input_ready(WORK));
        for dialog in [TRUST, LSP_PLUGIN, AUTO_MODE] {
            assert!(!input_ready(dialog), "{dialog}");
        }
        assert!(!input_ready(EXITED));
        // A quote under a rule in the transcript is not the input box.
        let quoted = format!("{}\n> quoted\n{READY}", "─".repeat(70));
        assert_eq!(input_box(&quoted).unwrap(), ["❯"]);
        // A box scrolled far above the bottom is not the input box.
        let scrolled = format!("{READY}{}", "output line\n".repeat(INPUT_FOOTER_LINES));
        assert!(!input_ready(&scrolled));
    }

    #[test]
    fn input_pending_finds_the_typed_text_left_in_the_box() {
        assert!(input_pending(LONG_PENDING, REQUEST));
        assert!(input_pending(PASTED, REQUEST));
        assert!(input_pending(EXIT_PENDING, "/exit"));
        assert!(input_pending(WORK, "run the tests again"));
        for screen in [READY, READY_BOXED, BOOT, BOOT_BANNER, TRUST, AUTO_MODE] {
            assert!(!input_pending(screen, REQUEST), "{screen}");
            assert!(!input_pending(screen, "/exit"), "{screen}");
        }
        assert!(!input_pending(EXITED, "/exit"));
        // A typed `/exit` is not the request. (A request left in the box
        // counts for `/exit` too, which its own `/exit` ends: one Enter
        // submits both.)
        assert!(!input_pending(EXIT_PENDING, REQUEST));
        assert!(!input_pending(LONG_PENDING, "answer to ask 3: yes"));
        assert!(!input_pending(READY, ""));
    }

    #[test]
    fn agent_working_reads_the_spinner() {
        assert!(agent_working(WORK));
        for screen in [READY, BOOT, LONG_PENDING, TRUST] {
            assert!(!agent_working(screen), "{screen}");
        }
        let claude = ClaudeCode {
            executable: "claude".into(),
        };
        assert!(claude.working("✻ Thinking… (3s · Esc to interrupt)\n"));
        assert!(claude.input_ready(READY));
        assert!(claude.input_pending(EXIT_PENDING, "/exit"));
    }

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

    /// The confirmation of a `/exit` sent while a `run_in_background`
    /// shell runs (Claude Code 2.1, task 49).
    const BACKGROUND_WORK: &str = "\
⏺ The receipt is written; the run is ready for review.

╭──────────────────────────────────────────────────────────────────────╮
│ Background work is running                                           │
│                                                                      │
│ 1 background task is still running:                                  │
│   · cargo test --locked --test e2e -- --ignored (shell)              │
│                                                                      │
│ ❯ 1. Exit and stop tasks                                             │
│   2. Move to background and exit                                     │
│   3. Stay                                                            │
╰──────────────────────────────────────────────────────────────────────╯
   Enter to confirm · Esc to cancel
";

    /// The Settings panel `/status` opens, on its Usage tab.
    const SETTINGS_PANEL: &str = "\
⏺ The receipt is written; the run is ready for review.

────────────────────────────────────────────────────────────────────────
 Settings:  Status   Config   Usage   (←/→ or tab to cycle)

 Current session
 ███████████▌                                       23% used
 Resets 11pm (Asia/Tokyo)

 Current week (all models)
 ████                                               8% used
 Resets Oct 1, 9am (Asia/Tokyo)

 Esc to exit
";

    #[test]
    fn known_dialog_answers_background_work_with_exit_and_stop_tasks() {
        let answer = known_dialog(BACKGROUND_WORK).expect("known dialog");
        assert_eq!(answer.dialog, KnownDialog::BackgroundWork);
        assert_eq!(answer.keys, vec!["enter"]);
        // The mark on another option is moved to it first.
        let stay = BACKGROUND_WORK
            .replace("│ ❯ 1. Exit", "│   1. Exit")
            .replace("│   3. Stay  ", "│ ❯ 3. Stay  ");
        assert_eq!(known_dialog(&stay).unwrap().keys, vec!["up", "up", "enter"]);
        let reordered = BACKGROUND_WORK
            .replace(
                "❯ 1. Exit and stop tasks      ",
                "❯ 1. Move to background and exit",
            )
            .replace(
                "  2. Move to background and exit",
                "  2. Exit and stop tasks        ",
            );
        assert_eq!(
            known_dialog(&reordered).unwrap().keys,
            vec!["down", "enter"]
        );
        // It also counts as a dialog for prompt_waiting.
        assert_eq!(detect_prompt(BACKGROUND_WORK), Some(PromptKind::Choice));
    }

    #[test]
    fn known_dialog_closes_the_settings_panel_with_escape() {
        let answer = known_dialog(SETTINGS_PANEL).expect("known dialog");
        assert_eq!(answer.dialog, KnownDialog::SettingsPanel);
        assert_eq!(answer.keys, vec!["escape"]);
        assert_eq!(KnownDialog::SettingsPanel.as_str(), "settings_panel");
        assert_eq!(KnownDialog::BackgroundWork.as_str(), "background_work");
        // The Status tab of the same panel.
        let status = "Settings:  Status   Config   Usage\n Version: 2.1.281\n Esc to exit\n";
        assert_eq!(
            known_dialog(status).map(|a| a.dialog),
            Some(KnownDialog::SettingsPanel)
        );
    }

    #[test]
    fn known_dialog_leaves_every_other_screen_alone() {
        for screen in [
            TRUST,
            LSP_PLUGIN,
            AUTO_MODE,
            WORK,
            READY,
            EXIT_PENDING,
            BOOT,
            "",
        ] {
            assert_eq!(known_dialog(screen), None, "{screen}");
        }
        // The title quoted in the work, above a drawn input box.
        let quoted = format!(
            "⏺ grep \"Background work is running\" AGENTS.md\n{SETTINGS_TITLE} Status Usage\n Esc to exit\n{READY}"
        );
        assert_eq!(known_dialog(&quoted), None);
        // Without the option to pick, or without a marked option.
        let no_exit = BACKGROUND_WORK.replace("Exit and stop tasks", "Exit anyway       ");
        assert_eq!(known_dialog(&no_exit), None);
        let unmarked = BACKGROUND_WORK.replace("❯ 1.", "  1.");
        assert_eq!(known_dialog(&unmarked), None);
        // The panel's title without its footer.
        assert_eq!(
            known_dialog("Settings:  Status   Config   Usage\n Version: 2.1.281\n"),
            None
        );
        // A line that merely starts with Settings.
        assert_eq!(
            known_dialog("Settings saved to ~/.claude/settings.json\n"),
            None
        );
        // A dialog scrolled far above the bottom no longer counts.
        let scrolled = format!(
            "{SETTINGS_PANEL}{}",
            "output line\n".repeat(PROMPT_SCAN_LINES)
        );
        assert_eq!(known_dialog(&scrolled), None);
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
            assert_eq!(idle.background_tasks.len(), usize::from(running), "{hook}");
        }
        let idle = claude.idle_hook(
            json!({"background_tasks": [
                {"id": "b2", "status": "running", "description": "cargo test", "command": "cargo test --locked"}
            ]})
            .to_string()
            .as_bytes(),
        );
        assert_eq!(
            idle.background_tasks,
            [BackgroundTask {
                id: "b2".into(),
                description: "cargo test".into(),
                command: "cargo test --locked".into(),
            }]
        );
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

    #[test]
    fn auth_required_reads_a_login_that_ran_out() {
        let expired = "\
⏺ Bash(cargo test)
  ⎿  API Error: 401 {\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"OAuth token has expired.\"}} · Please run /login

│ ❯ 
  ? for shortcuts
";
        assert!(auth_required(expired));
        let claude = ClaudeCode {
            executable: "claude".into(),
        };
        assert!(claude.auth_required("│ Invalid API key · Please run /login │\n"));
        assert!(!auth_required(WORK));
        assert!(!auth_required(READY));
        // Text that only mentions the error is the work.
        assert!(!auth_required(
            "src/claude.rs:12: \"API Error: 401\" · Please run /login\n"
        ));
        assert!(!auth_required("  ⎿  API Error: 401 (retrying)\n"));
        // A phrase far above the bottom is the work, not the session's state.
        let mut old = String::from("Please run /login\n");
        old.push_str(&"line\n".repeat(PROMPT_SCAN_LINES));
        assert!(!auth_required(&old));
    }
}
