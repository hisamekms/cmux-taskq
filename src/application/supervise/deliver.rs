//! What the supervisor types into a live session, and whether it got there
//! (task 285): the Enter a long paste swallowed is sent again without the
//! text, a text still in the input box after that is raised to the inbox,
//! and a session that shows no sign of work after a request or an answer
//! ([`StartCheck`]) is sent it again or raised. Every `send_text` and
//! `send_exit` of the supervisor goes through [`submit`].

use super::*;

/// What the supervisor types into a session.
#[derive(Debug, Clone, Copy)]
pub(super) enum Input<'a> {
    /// A request or an answer, typed and submitted with Enter.
    Text(&'a str),
    /// `/exit`, never typed twice: a second one could pick a dialog's
    /// option.
    Exit,
}

impl Input<'_> {
    fn text(&self) -> &str {
        match self {
            Input::Text(text) => text,
            Input::Exit => "/exit",
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Input::Text(_) => "text",
            Input::Exit => "exit",
        }
    }
}

/// Enter is sent again at most this many times after a submit.
pub(super) const SUBMIT_RETRIES: usize = 3;

/// Where a submit ended, with the last screen read.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Submission {
    /// The input left the input box; `None` when the screen could not be
    /// read, which is not held against the send.
    Submitted(Option<String>),
    /// A dialog is on the screen: no Enter is sent over it.
    Dialog(String),
    /// The input is still in the box after [`SUBMIT_RETRIES`] Enters.
    Stuck(String),
}

impl Submission {
    /// The screen read after the submit, if any.
    pub(super) fn screen(&self) -> Option<&str> {
        match self {
            Submission::Submitted(screen) => screen.as_deref(),
            Submission::Dialog(screen) | Submission::Stuck(screen) => Some(screen),
        }
    }
}

/// Type `input` into the session in `workspace` and read the screen
/// every `submit_check_interval`: while the input box still holds it (and
/// no dialog is up) Enter alone is sent again, at most
/// [`SUBMIT_RETRIES`] times. Returns the outcome and the Enters sent
/// again. An error is a failed typing of the input; an Enter that fails
/// after it leaves the input stuck in the box.
pub(super) fn submit_input(
    cmux: &dyn WorkspaceBackend,
    signals: &dyn AgentSignals,
    workspace: &str,
    input: Input<'_>,
) -> Result<(Submission, usize)> {
    match input {
        Input::Text(text) => cmux.send_text(workspace, text)?,
        Input::Exit => cmux.send_exit(workspace)?,
    }
    let mut retries = 0;
    loop {
        thread::sleep(cmux.submit_check_interval());
        let screen = match cmux.capture(workspace) {
            Ok(screen) => screen,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "screen of workspace {workspace} could not be read after a submit: {error:#}");
                return Ok((Submission::Submitted(None), retries));
            }
        };
        if signals.detect_prompt(&screen).is_some() {
            return Ok((Submission::Dialog(screen), retries));
        }
        if !signals.input_pending(&screen, input.text()) {
            return Ok((Submission::Submitted(Some(screen)), retries));
        }
        if retries == SUBMIT_RETRIES {
            return Ok((Submission::Stuck(screen), retries));
        }
        if let Err(error) = cmux.send_enter(workspace) {
            warn!(error = %format_args!("{error:#}"), "Enter could not be sent again to workspace {workspace}: {error:#}");
            return Ok((Submission::Stuck(screen), retries));
        }
        retries += 1;
    }
}

/// [`submit_input`] into `run`'s session, `what` naming the input in the
/// records. Enters sent again are recorded as `submit_retried`; an input
/// still in the box as `submit_unconfirmed`, and a text also raised as an
/// `answer_prompt` ask to the inbox (a `/exit` becomes the `stuck_exit` ask
/// of its exit timeout). An error is only a failed typing: the input was
/// typed once it returns, so a record or an ask that fails after it is
/// only noted.
pub(super) fn submit(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    workspace: &str,
    input: Input<'_>,
    what: &str,
) -> Result<Submission> {
    let (submission, retries) = submit_input(sv.cmux, sv.signals, workspace, input)?;
    let note = |sv: &mut Supervisor<'_>, kind: &str, payload: Value| {
        if let Err(error) = sv.queue.record_runtime_event(run.id(), kind, payload) {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "{kind} of {} could not be recorded: {error:#}", run.id());
        }
    };
    if retries > 0 {
        note(
            sv,
            "submit_retried",
            json!({
                "workspace_id": workspace,
                "input": input.name(),
                "what": what,
                "retries": retries,
                "submitted": !matches!(submission, Submission::Stuck(_)),
            }),
        );
        info!(run_id = %run.id(), "{what} stayed in the input box of workspace {workspace}; Enter sent again {retries} times");
    }
    if let Submission::Stuck(screen) = &submission {
        let excerpt = sv.signals.screen_excerpt(screen);
        note(
            sv,
            "submit_unconfirmed",
            json!({
                "workspace_id": workspace,
                "input": input.name(),
                "what": what,
                "retries": retries,
                "excerpt": excerpt,
            }),
        );
        warn!(run_id = %run.id(), "{what} is still in the input box of workspace {workspace} after {retries} Enters");
        if let Input::Text(_) = input {
            let situation = format!(
                "the {what} the supervisor typed stays in the input box after {} Enters, not submitted",
                retries + 1
            );
            ask_unsubmitted(sv, run, workspace, &situation, &excerpt);
        }
    }
    Ok(submission)
}

/// Raise a session that did not take what the supervisor sent as an
/// `answer_prompt` ask to the inbox, the way a dialog is (the open ask of
/// the run is not registered twice). The runtime sends nothing more: the
/// person has the key or text sent, and the ask closes itself once the
/// session exits. A failed ask is only noted.
pub(super) fn ask_unsubmitted(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    workspace: &str,
    situation: &str,
    excerpt: &str,
) {
    let question = format!(
        "The session of run {run_id} (task {task_id}) in workspace {workspace}: {situation}. Answer with what to send to it (for example `enter` to press Enter, or the text to type), or what to do instead; it is done in that workspace, and this ask closes itself once the session exits.\n\nLast lines of the screen:\n{excerpt}",
        run_id = run.id(),
        task_id = run.task_id(),
    );
    let outcome = ask::ask(
        &mut *sv.queue,
        &sv.layout.repo_root,
        NewAsk {
            kind: AskKind::AnswerPrompt,
            task_id: Some(run.task_id()),
            run_id: Some(run.id().clone()),
            question,
            options: Vec::new(),
            asked_by: SessionRole::Supervisor.as_str().into(),
        },
        sv.cmux,
    );
    match outcome {
        Ok(outcome) => {
            info!(ask_id = %outcome["id"], run_id = %run.id(), "answer_prompt ask {} for {}: {situation} (notified: {})", outcome["id"], run.id(), outcome["notified"])
        }
        Err(error) => {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "answer_prompt ask for {} could not be opened: {error:#}", run.id())
        }
    }
}

/// What a session's screen says, [`StartCheck::wait`] after it was sent a
/// request or an answer, of whether it took it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StartSign {
    /// At work, or the screen moved on since the submit.
    Started,
    /// A dialog holds the session.
    Dialog(&'static str),
    /// The input box is drawn and no longer holds the text, but nothing
    /// happened: the text was lost.
    Lost,
    /// The text is still in the input box, or there is no input box.
    Held,
}

/// Judge `screen`, read a while after `text` was submitted, against
/// `submitted` (the screen read right after the submit, if any).
pub(super) fn start_sign(
    signals: &dyn AgentSignals,
    screen: &str,
    submitted: Option<&str>,
    text: &str,
) -> StartSign {
    if let Some(kind) = signals.detect_prompt(screen) {
        return StartSign::Dialog(kind);
    }
    if signals.working(screen) || submitted.is_some_and(|before| before != screen) {
        return StartSign::Started;
    }
    if signals.input_ready(screen) && !signals.input_pending(screen, text) {
        StartSign::Lost
    } else {
        StartSign::Held
    }
}

/// Watches a request or an answer sent to a live session until the session
/// shows a sign of work: its idle marker written after it, the agent at
/// work, or its screen changed. With none after `start_wait`, a text the
/// input box lost is sent once more (`submit_resent`); otherwise, or when
/// that is lost too, the run records `submit_not_started` and the inbox is
/// asked, instead of waiting out the resume timeout.
#[derive(Debug, Clone)]
pub(super) struct StartCheck {
    what: String,
    text: String,
    sent: Instant,
    /// The idle marker is compared with this, on the files' wall clock.
    sent_at: SystemTime,
    submitted: Option<String>,
    resent: bool,
    /// A sign was seen, or the inbox was asked: nothing more to check.
    done: bool,
}

impl StartCheck {
    /// Watch `text`, submitted at `sent_at` as `submission`: a text stuck
    /// in the box was raised by [`submit`] already.
    pub(super) fn new(
        what: &str,
        text: &str,
        sent_at: SystemTime,
        submission: &Submission,
    ) -> Self {
        Self {
            what: what.to_owned(),
            text: text.to_owned(),
            sent: Instant::now(),
            sent_at,
            submitted: submission.screen().map(str::to_owned),
            resent: false,
            done: matches!(submission, Submission::Stuck(_)),
        }
    }

    /// One observation of the session in `workspace`.
    pub(super) fn poll(
        &mut self,
        sv: &mut Supervisor<'_>,
        run: &TaskRun,
        workspace: &str,
        idle_marker: &Path,
    ) -> Result<()> {
        let wait = sv.cmux.start_wait();
        if self.done || self.sent.elapsed() < wait {
            return Ok(());
        }
        if sv
            .files
            .modified(idle_marker)
            .is_ok_and(|modified| modified > self.sent_at)
        {
            self.done = true;
            return Ok(());
        }
        let screen = match sv.cmux.capture(workspace) {
            Ok(screen) => screen,
            Err(error) => {
                warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "screen of {} could not be read for a sign of work: {error:#}", run.id());
                return Ok(());
            }
        };
        let sign = start_sign(sv.signals, &screen, self.submitted.as_deref(), &self.text);
        let excerpt = sv.signals.screen_excerpt(&screen);
        match sign {
            StartSign::Started => self.done = true,
            StartSign::Dialog(kind) => {
                self.done = true;
                let situation = format!(
                    "a {kind} dialog came up after the supervisor sent the {}",
                    self.what
                );
                ask_unsubmitted(sv, run, workspace, &situation, &excerpt);
            }
            StartSign::Lost if !self.resent => {
                sv.queue.record_runtime_event(
                    run.id(),
                    "submit_resent",
                    json!({
                        "workspace_id": workspace,
                        "what": self.what,
                        "waited_secs": wait.as_secs(),
                    }),
                )?;
                info!(run_id = %run.id(), "session of {} showed no sign of the {} within {}s and its input box is empty; sending it again", run.id(), self.what, wait.as_secs());
                let sent_at = sv.files.now();
                let text = self.text.clone();
                let what = self.what.clone();
                let submission = submit(sv, run, workspace, Input::Text(&text), &what)?;
                *self = Self::new(&what, &text, sent_at, &submission);
                self.resent = true;
            }
            StartSign::Lost | StartSign::Held => {
                self.done = true;
                sv.queue.record_runtime_event(
                    run.id(),
                    "submit_not_started",
                    json!({
                        "workspace_id": workspace,
                        "what": self.what,
                        "waited_secs": wait.as_secs(),
                        "resent": self.resent,
                        "excerpt": excerpt,
                    }),
                )?;
                warn!(run_id = %run.id(), "session of {} showed no sign of the {} within {}s; asking the inbox", run.id(), self.what, wait.as_secs());
                let situation = format!(
                    "the session showed no sign of work within {}s of the {} the supervisor sent{}",
                    wait.as_secs(),
                    self.what,
                    if self.resent { " twice" } else { "" }
                );
                ask_unsubmitted(sv, run, workspace, &situation, &excerpt);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::SupervisorEnvironment;
    use crate::domain::{Task, TaskRun};
    use std::sync::Mutex;

    /// A session whose screen is one of `screens` per capture (the last
    /// one repeats), recording what was sent.
    struct Backend {
        screens: Mutex<Vec<String>>,
        sent: Mutex<Vec<String>>,
    }

    impl Backend {
        fn new(screens: &[&str]) -> Self {
            Self {
                screens: Mutex::new(screens.iter().rev().map(|s| (*s).to_owned()).collect()),
                sent: Mutex::new(Vec::new()),
            }
        }

        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl WorkspaceBackend for Backend {
        fn preflight(&self) -> Result<()> {
            unimplemented!()
        }
        fn preflight_detached(&self, _: &SupervisorEnvironment) -> Result<()> {
            unimplemented!()
        }
        fn create(&self, _: &Task, _: &TaskRun, _: &str, _: &WorkspaceTags) -> Result<String> {
            unimplemented!()
        }
        fn create_resume(
            &self,
            _: &Task,
            _: &TaskRun,
            _: &str,
            _: &WorkspaceTags,
        ) -> Result<String> {
            unimplemented!()
        }
        fn send_text(&self, _: &str, text: &str) -> Result<()> {
            self.sent.lock().unwrap().push(text.to_owned());
            Ok(())
        }
        fn send_enter(&self, _: &str) -> Result<()> {
            self.sent.lock().unwrap().push("<enter>".to_owned());
            Ok(())
        }
        fn capture(&self, _: &str) -> Result<String> {
            let mut screens = self.screens.lock().unwrap();
            match screens.len() {
                0 => bail!("no screen"),
                1 => Ok(screens[0].clone()),
                _ => Ok(screens.pop().unwrap()),
            }
        }
        fn close(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn set_color(&self, _: &str, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn set_status(&self, _: &str, _: &str, _: &str, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn pin(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        fn send_exit(&self, _: &str) -> Result<()> {
            self.sent.lock().unwrap().push("/exit".to_owned());
            Ok(())
        }
        fn exists(&self, _: &str) -> Result<bool> {
            unimplemented!()
        }
        fn listed_workspace_ids(&self) -> Result<Vec<String>> {
            unimplemented!()
        }
        fn create_named(&self, _: &str, _: &Path, _: &str, _: &WorkspaceTags) -> Result<String> {
            unimplemented!()
        }
        fn ensure_group(&self, _: &str, _: &str) -> Result<String> {
            unimplemented!()
        }
        fn notify(&self, _: &str, _: &str, _: Option<&str>) -> Result<()> {
            unimplemented!()
        }
        fn submit_check_interval(&self) -> Duration {
            Duration::ZERO
        }
    }

    /// Screens as words: `ready`, `pending:<text>` (in the box), `dialog`,
    /// `working`, `boot`.
    struct Signals;

    impl AgentSignals for Signals {
        fn detect_prompt(&self, screen: &str) -> Option<&'static str> {
            (screen == "dialog").then_some("choice")
        }
        fn screen_excerpt(&self, screen: &str) -> String {
            screen.to_owned()
        }
        fn idle_hook(&self, _: &[u8]) -> IdleHook {
            IdleHook::default()
        }
        fn input_ready(&self, screen: &str) -> bool {
            screen == "ready" || screen.starts_with("pending:")
        }
        fn input_pending(&self, screen: &str, text: &str) -> bool {
            screen.strip_prefix("pending:") == Some(text)
        }
        fn working(&self, screen: &str) -> bool {
            screen == "working"
        }
    }

    const TEXT: &str = "please rebase";

    fn submitted(screens: &[&str], input: Input<'_>) -> (Submission, usize, Vec<String>) {
        let backend = Backend::new(screens);
        let (submission, retries) = submit_input(&backend, &Signals, "ws", input).unwrap();
        (submission, retries, backend.sent())
    }

    #[test]
    fn a_text_that_left_the_box_is_submitted_once() {
        let (submission, retries, sent) = submitted(&["ready"], Input::Text(TEXT));
        assert_eq!(submission, Submission::Submitted(Some("ready".into())));
        assert_eq!((retries, sent), (0, vec![TEXT.to_owned()]));
        // A screen that cannot be read does not count against the send.
        let (submission, retries, _) = submitted(&[], Input::Text(TEXT));
        assert_eq!((submission, retries), (Submission::Submitted(None), 0));
    }

    #[test]
    fn a_text_left_in_the_box_gets_enter_alone_again() {
        let pending = format!("pending:{TEXT}");
        let (submission, retries, sent) =
            submitted(&[&pending, &pending, "working"], Input::Text(TEXT));
        assert_eq!(submission, Submission::Submitted(Some("working".into())));
        assert_eq!(retries, 2);
        // The text is typed once; only Enter goes again.
        assert_eq!(sent, [TEXT, "<enter>", "<enter>"]);
        // Past the retries it is stuck.
        let (submission, retries, sent) = submitted(&[&pending], Input::Text(TEXT));
        assert_eq!(submission, Submission::Stuck(pending.clone()));
        assert_eq!(retries, SUBMIT_RETRIES);
        assert_eq!(sent.iter().filter(|s| *s == TEXT).count(), 1);
        assert_eq!(sent.len(), 1 + SUBMIT_RETRIES);
    }

    #[test]
    fn exit_left_in_the_box_gets_enter_but_is_never_typed_again() {
        let (submission, retries, sent) = submitted(&["pending:/exit", "ready"], Input::Exit);
        assert_eq!(submission, Submission::Submitted(Some("ready".into())));
        assert_eq!(retries, 1);
        assert_eq!(sent, ["/exit", "<enter>"]);
        let (submission, _, sent) = submitted(&["pending:/exit"], Input::Exit);
        assert!(matches!(submission, Submission::Stuck(_)));
        assert_eq!(sent.iter().filter(|s| *s == "/exit").count(), 1);
    }

    #[test]
    fn no_enter_goes_over_a_dialog() {
        let (submission, retries, sent) = submitted(&["dialog"], Input::Exit);
        assert_eq!(submission, Submission::Dialog("dialog".into()));
        assert_eq!((retries, sent), (0, vec!["/exit".to_owned()]));
        let (submission, _, sent) = submitted(&["dialog"], Input::Text(TEXT));
        assert_eq!(submission.screen(), Some("dialog"));
        assert_eq!(sent, [TEXT]);
    }

    #[test]
    fn start_sign_tells_work_from_a_lost_or_held_text() {
        let pending = format!("pending:{TEXT}");
        assert_eq!(
            start_sign(&Signals, "working", Some("working"), TEXT),
            StartSign::Started
        );
        // The screen moved on since the submit.
        assert_eq!(
            start_sign(&Signals, "ready", Some("boot"), TEXT),
            StartSign::Started
        );
        assert_eq!(
            start_sign(&Signals, "dialog", Some("ready"), TEXT),
            StartSign::Dialog("choice")
        );
        assert_eq!(
            start_sign(&Signals, "ready", Some("ready"), TEXT),
            StartSign::Lost
        );
        assert_eq!(start_sign(&Signals, "ready", None, TEXT), StartSign::Lost);
        assert_eq!(
            start_sign(&Signals, &pending, Some(&pending), TEXT),
            StartSign::Held
        );
        assert_eq!(start_sign(&Signals, "boot", None, TEXT), StartSign::Held);
    }

    #[test]
    fn a_start_check_starts_from_the_submitted_screen() {
        let at = SystemTime::UNIX_EPOCH;
        let check = StartCheck::new("request", TEXT, at, &Submission::Submitted(None));
        assert!(!check.done && check.submitted.is_none());
        let check = StartCheck::new("request", TEXT, at, &Submission::Stuck("s".into()));
        assert!(check.done);
        assert_eq!(check.submitted.as_deref(), Some("s"));
        assert_eq!(Input::Exit.name(), "exit");
    }
}
