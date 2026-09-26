//! The dialogs of the fixed list the supervisor answers by rule (ADR-0047
//! decision 29): which one is on the screen and its keys come from
//! [`AgentSignals::known_dialog`]; whether its safety conditions hold, the
//! keys and the record are here. A dialog is answered at most once in a
//! stage of the run, and one whose conditions do not hold is recorded and
//! left to the path it took before (the `answer_prompt` or `stuck_exit`
//! ask).

use super::*;
use crate::{application::KnownDialog, domain::RunEvent};

/// The events that begin a stage of a run's session: a dialog is answered
/// at most once after the last of them.
const STAGE_EVENTS: &[&str] = &[
    "agent_started",
    "resume_started",
    "revise_requested",
    "exit_requested",
];

/// Answer the known dialog on `screen` of the session of `run` in
/// `workspace` when its conditions hold: "Background work is running" only
/// once the supervisor sent `/exit` (`exit_requested`), with the worktree
/// clean and the receipt's commit at its HEAD; the Settings panel at any
/// stage. The keys sent are recorded as `auto_repaired` (`repair:
/// dialog_answered`), with the conditions checked and the screen's excerpt;
/// conditions that do not hold, or keys that could not be sent, as
/// `known_dialog_unanswered`. Returns whether the keys were sent: the
/// caller then waits for the dialog to go, and a dialog already answered
/// or recorded in this stage gets nothing more (`false`).
pub(super) fn answer_known_dialog(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    workspace: &str,
    screen: &str,
    exit_requested: bool,
) -> Result<bool> {
    let Some(answer) = sv.signals.known_dialog(screen) else {
        return Ok(false);
    };
    let dialog = answer.dialog.as_str();
    if handled_in_stage(&sv.queue.run_events(run.id())?, dialog) {
        return Ok(false);
    }
    let excerpt = sv.signals.screen_excerpt(screen);
    let (ok, conditions) = match answer.dialog {
        KnownDialog::BackgroundWork => background_work_conditions(sv, run, exit_requested),
        KnownDialog::SettingsPanel => (true, json!({})),
    };
    let sent = if ok {
        answer
            .keys
            .iter()
            .try_for_each(|key| sv.cmux.send_key(workspace, key))
    } else {
        Ok(())
    };
    match sent {
        Ok(()) if ok => {
            sv.queue.record_runtime_event(
                run.id(),
                "auto_repaired",
                json!({
                    "layer": "runtime",
                    "repair": "dialog_answered",
                    "dialog": dialog,
                    "keys": answer.keys,
                    "conditions": conditions,
                    "detail": {"workspace_id": workspace, "excerpt": excerpt},
                }),
            )?;
            info!(run_id = %run.id(), "run {} was held by the {dialog} dialog in workspace {workspace}; answered it with {:?}", run.id(), answer.keys);
            Ok(true)
        }
        result => {
            let error = result.err().map(|error| format!("{error:#}"));
            sv.queue.record_runtime_event(
                run.id(),
                "known_dialog_unanswered",
                json!({
                    "dialog": dialog,
                    "conditions": conditions,
                    "error": error,
                    "workspace_id": workspace,
                    "excerpt": excerpt,
                }),
            )?;
            warn!(run_id = %run.id(), "run {} is held by the {dialog} dialog in workspace {workspace}, which is not answered: conditions {conditions}, error {error:?}", run.id());
            Ok(false)
        }
    }
}

/// At the exit timeout, answer the "Background work is running" dialog
/// that holds the session's `/exit` back ([`answer_known_dialog`];
/// `exit_typed` is whether the supervisor typed that `/exit`). Returns
/// whether keys were sent: the caller then waits the exit timeout again
/// instead of timing out. Another dialog, the Settings panel included, is
/// left to the timeout: closing the panel would not type the `/exit` it
/// took again. A screen that cannot be read answers nothing.
pub(super) fn answer_exit_dialog(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    workspace: &str,
    exit_typed: bool,
) -> Result<bool> {
    match sv.cmux.capture(workspace) {
        Ok(screen)
            if sv
                .signals
                .known_dialog(&screen)
                .is_some_and(|answer| answer.dialog == KnownDialog::BackgroundWork) =>
        {
            answer_known_dialog(sv, run, workspace, &screen, exit_typed)
        }
        Ok(_) => Ok(false),
        Err(error) => {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "screen of {} could not be read for a dialog at its exit timeout: {error:#}", run.id());
            Ok(false)
        }
    }
}

/// Whether `dialog` was answered or recorded unanswered since the last
/// event that began a stage.
fn handled_in_stage(events: &[RunEvent], dialog: &str) -> bool {
    events
        .iter()
        .rev()
        .take_while(|e| !STAGE_EVENTS.contains(&e.kind.as_str()))
        .any(|e| {
            let answered = e.kind == "auto_repaired"
                && e.payload.get("repair").and_then(Value::as_str) == Some("dialog_answered");
            (answered || e.kind == "known_dialog_unanswered")
                && e.payload.get("dialog").and_then(Value::as_str) == Some(dialog)
        })
}

/// Whether "Exit and stop tasks" may stop the session's background work:
/// the supervisor asked the session to exit, and nothing the work does can
/// still change the result (the worktree is clean and the receipt names its
/// HEAD). Returns the verdict and the values checked.
fn background_work_conditions(
    sv: &Supervisor<'_>,
    run: &TaskRun,
    exit_requested: bool,
) -> (bool, Value) {
    let worktree = run.worktree_path().map(Path::new);
    let clean = worktree.and_then(|w| sv.repository.status(w).ok().map(|s| s.trim().is_empty()));
    let head = worktree.and_then(|w| sv.repository.head(w).ok());
    let receipt = run
        .receipt_path()
        .and_then(|path| sv.files.read_to_string(Path::new(path)).ok())
        .and_then(|text| Receipt::parse(&text).ok())
        .filter(|receipt| receipt.run_id == *run.id().as_str());
    let receipt_commit = receipt.map(|receipt| receipt.commit);
    let at_head = matches!((&receipt_commit, &head), (Some(commit), Some(head)) if commit.eq_ignore_ascii_case(head.as_str()));
    (
        exit_requested && clean == Some(true) && at_head,
        json!({
            "exit_requested": exit_requested,
            "clean": clean,
            "head": head,
            "receipt_commit": receipt_commit,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: String::new(),
        }
    }

    #[test]
    fn a_dialog_is_handled_once_in_a_stage() {
        let answered = event(
            "auto_repaired",
            json!({"repair": "dialog_answered", "dialog": "settings_panel"}),
        );
        let unanswered = event(
            "known_dialog_unanswered",
            json!({"dialog": "background_work"}),
        );
        let exit = event("exit_requested", json!({}));
        assert!(handled_in_stage(
            std::slice::from_ref(&answered),
            "settings_panel"
        ));
        assert!(!handled_in_stage(
            std::slice::from_ref(&answered),
            "background_work"
        ));
        assert!(handled_in_stage(
            std::slice::from_ref(&unanswered),
            "background_work"
        ));
        // A new stage begins with /exit.
        assert!(!handled_in_stage(
            &[unanswered.clone(), exit.clone()],
            "background_work"
        ));
        assert!(handled_in_stage(&[exit, unanswered], "background_work"));
        // Another repair is not a dialog answered.
        let other = event(
            "auto_repaired",
            json!({"repair": "stop_processes", "dialog": "settings_panel"}),
        );
        assert!(!handled_in_stage(&[other], "settings_panel"));
    }
}
