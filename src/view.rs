//! Compact views of `show` and `goal show` for the sessions (ADR-0016):
//! a size bounded by the number of runs or tasks, not by their history.
//! Keys keep the names of the full output; a view only omits keys or
//! truncates strings. An object with a truncated string carries
//! `truncated: true`, and `--full` prints the stored records unchanged.

use serde_json::{Map, Value, json};

use crate::domain::{GoalDetail, OBSERVATION_KIND, RunEvent, TaskDetail};

/// Characters a long text field keeps before `…`.
pub const TEXT_LIMIT: usize = 300;
/// Latest events a compact view keeps unless told otherwise.
pub const DEFAULT_EVENTS: usize = 10;
/// Latest notes (`observation` events) a compact view lists in full.
pub const DEFAULT_OBSERVATIONS: usize = 5;
/// Payload keys a compact event keeps: what happened, not where.
const EVENT_GIST: [&str; 5] = ["status", "reason", "last_error", "from", "to"];

pub use crate::application::health::truncate;

/// Truncate the string fields `keys` of `object` in place and mark it
/// `truncated` when any of them was cut.
fn truncate_fields(object: &mut Map<String, Value>, keys: &[&str]) {
    let mut truncated = false;
    for key in keys {
        if let Some(Value::String(text)) = object.get_mut(*key)
            && let Some(cut) = truncate(text, TEXT_LIMIT)
        {
            *text = cut;
            truncated = true;
        }
    }
    if truncated {
        object.insert("truncated".into(), Value::Bool(true));
    }
}

fn object(value: impl serde::Serialize) -> Map<String, Value> {
    match serde_json::to_value(value) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

fn pick(source: &Map<String, Value>, keys: &[&str]) -> Map<String, Value> {
    keys.iter()
        .filter_map(|key| source.get(*key).map(|v| ((*key).to_owned(), v.clone())))
        .collect()
}

fn latest<T>(items: &[T], count: usize) -> &[T] {
    &items[items.len().saturating_sub(count)..]
}

/// `show` without `--full`: the task with long texts truncated, the latest
/// run's identity and outcome, its processes, and the latest `events`
/// events with their run and the gist of their payload (no paths).
pub fn task_detail(detail: &TaskDetail, events: usize) -> Value {
    let mut task = object(&detail.task);
    truncate_fields(&mut task, &["description", "acceptance", "context"]);
    let latest_run = detail.runs.last();
    let runs: Vec<Value> = latest_run
        .map(|run| {
            let mut run = pick(
                &object(run),
                &[
                    "id",
                    "status",
                    "branch",
                    "result_commit",
                    "last_error",
                    "worktree_path",
                    "workspace_id",
                ],
            );
            truncate_fields(&mut run, &["last_error"]);
            Value::Object(run)
        })
        .into_iter()
        .collect();
    let processes: Vec<_> = detail
        .processes
        .iter()
        .filter(|p| latest_run.is_some_and(|run| *run.id() == p.run_id))
        .collect();
    let events: Vec<Value> = latest(&detail.events, events)
        .iter()
        .map(event_gist)
        .collect();
    json!({
        "task": task,
        "dependencies": detail.dependencies,
        "runs": runs,
        "runs_total": detail.runs.len(),
        "events": events,
        "events_total": detail.events.len(),
        "observations": observations(&detail.events),
        "processes": processes,
    })
}

/// The latest [`DEFAULT_OBSERVATIONS`] notes among `events`, oldest first,
/// with their text cut to [`TEXT_LIMIT`].
fn observations(events: &[RunEvent]) -> Vec<Value> {
    let notes: Vec<&RunEvent> = events
        .iter()
        .filter(|event| event.kind == OBSERVATION_KIND)
        .collect();
    latest(&notes, DEFAULT_OBSERVATIONS)
        .iter()
        .map(|event| {
            let mut note = pick(&object(event), &["id", "created_at"]);
            if let Some(run_id) = &event.run_id {
                note.insert("run_id".into(), json!(run_id));
            }
            if let Value::Object(payload) = &event.payload {
                note.extend(pick(payload, &["text", "kind", "by"]));
            }
            truncate_fields(&mut note, &["text"]);
            Value::Object(note)
        })
        .collect()
}

fn event_gist(event: &RunEvent) -> Value {
    let mut compact = pick(&object(event), &["id", "kind", "created_at"]);
    if let Some(run_id) = &event.run_id {
        compact.insert("run_id".into(), json!(run_id));
    }
    if let Value::Object(payload) = &event.payload {
        let mut gist = pick(payload, &EVENT_GIST);
        if !gist.is_empty() {
            truncate_fields(&mut gist, &EVENT_GIST);
            compact.insert("payload".into(), Value::Object(gist));
        }
    }
    Value::Object(compact)
}

/// `goal show` without `--full`: the goal with long texts truncated, every
/// task's id, status and title, and the kind and time of the latest events.
pub fn goal_detail(detail: &GoalDetail) -> Value {
    let mut goal = object(&detail.goal);
    truncate_fields(
        &mut goal,
        &["title", "description", "acceptance", "constraints"],
    );
    let events: Vec<Value> = latest(&detail.events, DEFAULT_EVENTS)
        .iter()
        .map(|event| Value::Object(pick(&object(event), &["kind", "created_at"])))
        .collect();
    json!({
        "goal": goal,
        "closed": detail.closed,
        "tasks": detail.tasks,
        "events": events,
        "events_total": detail.events.len(),
        "observations": observations(&detail.events),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        CommitSha, Goal, GoalId, GoalTask, Provider, RunId, RunProcess, RunStatus, Task, TaskId,
        TaskRun, TaskStatus,
    };

    fn task(description: &str) -> Task {
        Task::restore(crate::domain::TaskRecord {
            id: TaskId::new(1),
            title: "t".into(),
            description: description.into(),
            acceptance: "short".into(),
            verification_commands: vec!["true".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            status: TaskStatus::InProgress,
            goal_id: None,
            context: String::new(),
            created_at: "c".into(),
            updated_at: "u".into(),
        })
        .unwrap()
    }

    fn run(id: &str) -> TaskRun {
        TaskRun::restore(crate::domain::RunRecord {
            id: RunId::new(id).unwrap(),
            task_id: TaskId::new(1),
            status: RunStatus::Failed,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            base_commit: CommitSha::try_from("b".repeat(40)).unwrap(),
            branch: Some(format!("dagq/{id}")),
            worktree_path: Some("/w".into()),
            workspace_id: Some("ws".into()),
            receipt_path: Some("/r".into()),
            log_path: Some("/l".into()),
            result_commit: None,
            repo_path: Some("/repo".into()),
            run_dir: Some("/run".into()),
            last_error: Some("e".repeat(TEXT_LIMIT + 1)),
            workspace_closed_at: None,
            created_at: "c".into(),
        })
        .unwrap()
    }

    fn event(id: i64, payload: Value) -> RunEvent {
        RunEvent {
            id,
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: Some(RunId::new("b").unwrap()),
            kind: format!("kind{id}"),
            payload,
            created_at: format!("t{id}"),
        }
    }

    fn process(run_id: &str) -> RunProcess {
        RunProcess {
            run_id: RunId::new(run_id).unwrap(),
            role: "wrapper".into(),
            pid: 1,
            heartbeat_at: 0,
            exited_at: None,
            exit_code: None,
        }
    }

    #[test]
    fn truncate_counts_characters_and_marks_the_cut() {
        assert_eq!(truncate("abc", 3), None);
        assert_eq!(truncate("abcd", 3).as_deref(), Some("abc…"));
        assert_eq!(truncate("あいうえ", 2).as_deref(), Some("あい…"));
    }

    #[test]
    fn task_detail_keeps_the_latest_run_and_events_without_paths() {
        let detail = TaskDetail {
            task: task(&"d".repeat(TEXT_LIMIT + 5)),
            dependencies: vec![TaskId::new(3)],
            runs: vec![run("a"), run("b")],
            events: (1..=12)
                .map(|id| {
                    event(
                        id,
                        json!({"path": "/run/receipt.json", "status": "failed", "reason": "x"}),
                    )
                })
                .chain([RunEvent {
                    run_id: None,
                    ..event(13, json!({"path": "/p"}))
                }])
                .collect(),
            processes: vec![process("a"), process("b")],
        };
        let view = task_detail(&detail, 3);
        let description = view["task"]["description"].as_str().unwrap();
        assert!(description.ends_with('…'));
        assert_eq!(description.chars().count(), TEXT_LIMIT + 1);
        assert_eq!(view["task"]["truncated"], true);
        assert_eq!(view["task"]["acceptance"], "short");
        assert_eq!(view["task"]["verification_commands"], json!(["true"]));
        assert_eq!(view["runs_total"], 2);
        let runs = view["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["id"], "b");
        assert_eq!(runs[0]["truncated"], true);
        assert!(runs[0].get("run_dir").is_none());
        assert!(runs[0]["result_commit"].is_null());
        assert_eq!(view["processes"].as_array().unwrap().len(), 1);
        assert_eq!(view["processes"][0]["run_id"], "b");
        assert_eq!(view["events_total"], 13);
        let events = view["events"].as_array().unwrap();
        assert_eq!(
            events.iter().map(|e| e["id"].clone()).collect::<Vec<_>>(),
            vec![json!(11), json!(12), json!(13)]
        );
        assert_eq!(
            events[0],
            json!({"id": 11, "kind": "kind11", "created_at": "t11", "run_id": "b",
                   "payload": {"status": "failed", "reason": "x"}})
        );
        assert_eq!(
            events[2],
            json!({"id": 13, "kind": "kind13", "created_at": "t13"})
        );
    }

    #[test]
    fn task_detail_without_runs_is_empty() {
        let detail = TaskDetail {
            task: task("short"),
            dependencies: vec![],
            runs: vec![],
            events: vec![],
            processes: vec![],
        };
        let view = task_detail(&detail, DEFAULT_EVENTS);
        assert!(view["task"].get("truncated").is_none());
        assert_eq!(view["runs"], json!([]));
        assert_eq!(view["events"], json!([]));
    }

    #[test]
    fn goal_detail_truncates_texts_and_keeps_kinds_of_latest_events() {
        let detail = GoalDetail {
            goal: Goal::restore(crate::domain::GoalRecord {
                id: GoalId::new(1),
                title: "g".into(),
                description: "x".repeat(TEXT_LIMIT * 2),
                acceptance: "a".into(),
                constraints: "c".repeat(TEXT_LIMIT + 1),
                doc: None,
                status: crate::domain::GoalStatus::Draft,
                closed_at: None,
                verdict: None,
                created_at: "c".into(),
                updated_at: "u".into(),
            })
            .unwrap(),
            closed: false,
            tasks: vec![GoalTask {
                id: TaskId::new(2),
                title: "t".into(),
                status: TaskStatus::Ready,
            }],
            events: (1..=11).map(|id| event(id, json!({"goal": {}}))).collect(),
        };
        let view = goal_detail(&detail);
        assert_eq!(view["goal"]["truncated"], true);
        assert!(view["goal"]["constraints"].as_str().unwrap().ends_with('…'));
        assert_eq!(view["goal"]["acceptance"], "a");
        assert_eq!(
            view["tasks"],
            json!([{"id": 2, "title": "t", "status": "ready"}])
        );
        assert_eq!(view["events_total"], 11);
        let events = view["events"].as_array().unwrap();
        assert_eq!(events.len(), DEFAULT_EVENTS);
        assert_eq!(events[0], json!({"kind": "kind2", "created_at": "t2"}));
    }

    #[test]
    fn views_list_the_latest_notes_with_their_text() {
        let note = |id: i64, text: &str| RunEvent {
            kind: OBSERVATION_KIND.into(),
            ..event(id, json!({"text": text, "kind": "stall", "by": "observer"}))
        };
        let mut events: Vec<RunEvent> = (1..=7).map(|id| note(id, "slow")).collect();
        events.push(event(8, json!({})));
        events.push(RunEvent {
            run_id: None,
            ..note(9, &"n".repeat(TEXT_LIMIT + 1))
        });
        let detail = TaskDetail {
            task: task("short"),
            dependencies: vec![],
            runs: vec![],
            events,
            processes: vec![],
        };
        let view = task_detail(&detail, 1);
        let notes = view["observations"].as_array().unwrap();
        assert_eq!(notes.len(), DEFAULT_OBSERVATIONS);
        assert_eq!(
            notes[0],
            json!({"id": 4, "created_at": "t4", "run_id": "b",
                   "text": "slow", "kind": "stall", "by": "observer"})
        );
        assert_eq!(notes[4]["truncated"], true);
        assert!(notes[4].get("run_id").is_none());
        assert!(notes[4]["text"].as_str().unwrap().ends_with('…'));
    }
}
