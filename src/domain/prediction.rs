//! The weight of a task as plan review predicts it (ADR-0079 decision 2):
//! the shape of one prediction, the check of the predictions a verdict
//! carries against the tasks it reviewed, and where a prediction's
//! `expected_output_tokens` falls among the latest ones. The values run 2-3
//! times high, so they are read by rank, never by value.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{DomainError, TaskId};

/// How many of the latest predictions (one per task, the task's last) a
/// prediction is ranked among (ADR-0079 decision 4).
pub const PREDICTION_WINDOW: usize = 60;

string_enum!(TaskSize {
    S => "S",
    M => "M",
    L => "L",
});

string_enum!(TaskNature {
    Mechanical => "mechanical",
    Implementation => "implementation",
    DesignJudgment => "design_judgment",
    Investigation => "investigation",
});

/// One task's predicted weight, as plan review prints it in the
/// `predictions` of its verdict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskWeightPrediction {
    pub task_id: TaskId,
    pub size: TaskSize,
    pub nature: TaskNature,
    /// 0 to 1, 1 the least certain.
    pub uncertainty: f64,
    /// The output tokens (thinking included) of one worker run.
    pub expected_output_tokens: u64,
    /// 0 to 1. Recorded only: no choice is made on it (ADR-0079 decision 4).
    pub rework_probability: f64,
    #[serde(default)]
    pub reason: String,
}

/// The predictions of a verdict's `predictions`, one for each of
/// `expected` (the proposal's submitted tasks) and nothing else, with the
/// probabilities within 0 to 1; or why they are not recorded. A verdict
/// without them is applied all the same (ADR-0079 decision 2).
pub fn parse_predictions(
    raw: Option<&Value>,
    expected: &[TaskId],
) -> Result<Vec<TaskWeightPrediction>, String> {
    let raw = match raw {
        Some(raw) => raw,
        None if expected.is_empty() => return Ok(Vec::new()),
        None => return Err("the verdict has no predictions".to_owned()),
    };
    let predictions: Vec<TaskWeightPrediction> = serde_json::from_value(raw.clone())
        .map_err(|error| format!("the predictions are malformed: {error}"))?;
    let mut seen = HashSet::new();
    for prediction in &predictions {
        let task = prediction.task_id;
        if !expected.contains(&task) {
            return Err(format!(
                "task {task} is predicted but is no submitted task of the proposal"
            ));
        }
        if !seen.insert(task) {
            return Err(format!("task {task} is predicted more than once"));
        }
        for (name, value) in [
            ("uncertainty", prediction.uncertainty),
            ("rework_probability", prediction.rework_probability),
        ] {
            if !(0.0..=1.0).contains(&value) {
                return Err(format!("task {task}: {name} {value} is not within 0 to 1"));
            }
        }
        if prediction.expected_output_tokens == 0 {
            return Err(format!("task {task}: expected_output_tokens is 0"));
        }
    }
    let missing: Vec<String> = expected
        .iter()
        .filter(|task| !seen.contains(task))
        .map(ToString::to_string)
        .collect();
    if !missing.is_empty() {
        return Err(format!("task {} is not predicted", missing.join(", ")));
    }
    Ok(predictions)
}

/// The `expected_output_tokens` `task` is ranked among: the last prediction
/// of each other task, newest first, at most `size` of them. `history` is
/// every `(task, expected_output_tokens)` recorded before the point it is
/// ranked at, oldest first.
pub fn window(history: &[(TaskId, u64)], task: TaskId, size: usize) -> Vec<u64> {
    let mut seen = HashSet::from([task]);
    history
        .iter()
        .rev()
        .filter(|(other, _)| seen.insert(*other))
        .map(|&(_, tokens)| tokens)
        .take(size)
        .collect()
}

/// Where `value` falls among `window`, 0 to 100: the share below it, with
/// ties counted half, to one decimal. `None` for an empty window. The
/// lower third is 33.3 or less (ADR-0079 decision 4).
pub fn percentile(value: u64, window: &[u64]) -> Option<f64> {
    if window.is_empty() {
        return None;
    }
    let below = window.iter().filter(|&&other| other < value).count();
    let equal = window.iter().filter(|&&other| other == value).count();
    #[allow(clippy::cast_precision_loss)]
    let share = (below as f64 + equal as f64 / 2.0) / window.len() as f64;
    Some((share * 1000.0).round() / 10.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn one(task: i64, tokens: u64) -> Value {
        json!({"task_id": task, "size": "M", "nature": "mechanical", "uncertainty": 0.3,
               "expected_output_tokens": tokens, "rework_probability": 0.1, "reason": "small"})
    }

    #[test]
    fn predictions_cover_the_submitted_tasks_exactly() {
        let tasks = [TaskId::new(3), TaskId::new(4)];
        let parsed =
            parse_predictions(Some(&json!([one(4, 9000), one(3, 30000)])), &tasks).expect("valid");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].task_id, TaskId::new(4));
        assert_eq!(parsed[0].size, TaskSize::M);
        assert_eq!(parsed[0].nature, TaskNature::Mechanical);
        assert_eq!(parsed[1].expected_output_tokens, 30000);
        // An unknown key and a missing reason are tolerated.
        let mut extra = one(3, 1);
        extra["extra"] = json!(true);
        extra.as_object_mut().unwrap().remove("reason");
        let lone = parse_predictions(Some(&json!([extra])), &tasks[..1]).expect("valid");
        assert_eq!(lone[0].reason, "");
        assert!(parse_predictions(Some(&json!([])), &[]).unwrap().is_empty());
        // Nothing to predict: none are needed.
        assert!(parse_predictions(None, &[]).unwrap().is_empty());
    }

    #[test]
    fn broken_predictions_say_why() {
        let tasks = [TaskId::new(3), TaskId::new(4)];
        let error = |raw: Option<Value>| parse_predictions(raw.as_ref(), &tasks).unwrap_err();
        assert_eq!(error(None), "the verdict has no predictions");
        assert!(error(Some(json!("S"))).starts_with("the predictions are malformed"));
        let mut size = one(3, 1);
        size["size"] = json!("XL");
        assert!(error(Some(json!([size, one(4, 1)]))).starts_with("the predictions are malformed"));
        assert_eq!(error(Some(json!([one(3, 1)]))), "task 4 is not predicted");
        assert_eq!(
            error(Some(json!([one(3, 1), one(4, 1), one(5, 1)]))),
            "task 5 is predicted but is no submitted task of the proposal"
        );
        assert_eq!(
            error(Some(json!([one(3, 1), one(3, 1)]))),
            "task 3 is predicted more than once"
        );
        let mut odds = one(4, 1);
        odds["rework_probability"] = json!(1.5);
        assert_eq!(
            error(Some(json!([one(3, 1), odds]))),
            "task 4: rework_probability 1.5 is not within 0 to 1"
        );
        assert_eq!(
            error(Some(json!([one(3, 1), one(4, 0)]))),
            "task 4: expected_output_tokens is 0"
        );
        let mut negative = one(4, 1);
        negative["expected_output_tokens"] = json!(-5);
        assert!(
            error(Some(json!([one(3, 1), negative]))).starts_with("the predictions are malformed")
        );
    }

    #[test]
    fn a_prediction_is_ranked_among_the_last_of_each_other_task() {
        let t = TaskId::new;
        let history = [(t(1), 10), (t(2), 20), (t(1), 50), (t(9), 5), (t(3), 30)];
        // Task 9 itself is left out, task 1 counts with its last value.
        assert_eq!(window(&history, t(9), 10), [30, 50, 20]);
        assert_eq!(window(&history, t(9), 2), [30, 50]);
        assert_eq!(window(&[], t(1), 60), Vec::<u64>::new());
        assert_eq!(percentile(25, &[30, 50, 20]), Some(33.3));
        assert_eq!(percentile(30, &[30, 50, 20]), Some(50.0));
        assert_eq!(percentile(1, &[30, 50, 20]), Some(0.0));
        assert_eq!(percentile(99, &[30, 50, 20]), Some(100.0));
        assert_eq!(percentile(1, &[]), None);
    }

    #[test]
    fn sizes_and_natures_read_back() {
        assert_eq!("L".parse::<TaskSize>().unwrap().as_str(), "L");
        assert_eq!(
            "design_judgment".parse::<TaskNature>().unwrap(),
            TaskNature::DesignJudgment
        );
        assert!("huge".parse::<TaskNature>().is_err());
    }
}
