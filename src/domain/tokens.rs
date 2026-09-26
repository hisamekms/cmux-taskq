//! The tokens a Claude session used (task 199): the `message.usage` of the
//! assistant records of its transcript that fall in a span, summed once per
//! message. The cost is Claude Code's own `costUSD` when every message
//! counted has one; it is never computed from prices. Reading the file is
//! the infrastructure's (`infrastructure::transcripts`); this is pure.
//! The model and effort a span's messages were written with (task 579)
//! come from the same records ([`span_models`]).

use std::collections::{BTreeMap, HashSet};

use serde_json::{Value, json};

use super::transcript::TranscriptRecord;

/// The span's assistant records carry a `usage` this version of the reader
/// does not understand: no count is recorded.
pub const USAGE_UNSUPPORTED: &str = "usage_unsupported";

/// The tokens of a span, per kind of token, and the messages they came
/// from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenUsage {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
    /// Assistant messages counted (records of one message are one).
    pub messages: i64,
    /// The sum of Claude Code's `costUSD`, when every message had one.
    pub cost_usd: Option<f64>,
}

impl TokenUsage {
    /// The `tokens` of a `session_closed`: the counts, and `cost_usd` only
    /// when there is one.
    pub fn payload(&self) -> Value {
        let mut payload = json!({
            "input": self.input,
            "output": self.output,
            "cache_read": self.cache_read,
            "cache_creation": self.cache_creation,
            "messages": self.messages,
        });
        if let Some(cost) = self.cost_usd {
            payload["cost_usd"] = json!((cost * 1e6).round() / 1e6);
        }
        payload
    }
}

/// One message's counts: the largest of its records' (the last block of a
/// message carries its final output count).
#[derive(Default)]
struct Message {
    counts: [i64; 4],
    cost: Option<f64>,
}

/// The tokens of the assistant records of `records` from `start` to `end`
/// (unix milliseconds, `[start, end)`), a subagent's included. `Err` with
/// [`USAGE_UNSUPPORTED`] when a record's usage has no numeric input and
/// output counts, or no assistant record has a usage at all.
pub fn span_usage(
    records: &[TranscriptRecord],
    start: i64,
    end: i64,
) -> Result<TokenUsage, &'static str> {
    let mut messages: BTreeMap<String, Message> = BTreeMap::new();
    let mut assistants = 0;
    for (at, record) in records.iter().enumerate() {
        if !record.assistant || record.at < start || record.at >= end {
            continue;
        }
        assistants += 1;
        let Some(usage) = &record.usage else {
            continue;
        };
        let count = |key: &str| usage.get(key).map(Value::as_i64);
        let (Some(Some(input)), Some(Some(output))) =
            (count("input_tokens"), count("output_tokens"))
        else {
            return Err(USAGE_UNSUPPORTED);
        };
        let cache = |key: &str| count(key).flatten().unwrap_or(0);
        let counts = [
            input,
            output,
            cache("cache_read_input_tokens"),
            cache("cache_creation_input_tokens"),
        ];
        // A record without a message id is a message of its own.
        let key = record
            .message_id
            .clone()
            .unwrap_or_else(|| format!("#{at}"));
        let message = messages.entry(key).or_default();
        for (kept, count) in message.counts.iter_mut().zip(counts) {
            *kept = (*kept).max(count);
        }
        if record.cost_usd.is_some() {
            message.cost = record.cost_usd;
        }
    }
    if assistants > 0 && messages.is_empty() {
        return Err(USAGE_UNSUPPORTED);
    }
    let mut usage = TokenUsage {
        messages: messages.len() as i64,
        cost_usd: messages
            .values()
            .map(|message| message.cost)
            .sum::<Option<f64>>(),
        ..TokenUsage::default()
    };
    if messages.is_empty() {
        usage.cost_usd = None;
    }
    for message in messages.values() {
        usage.input += message.counts[0];
        usage.output += message.counts[1];
        usage.cache_read += message.counts[2];
        usage.cache_creation += message.counts[3];
    }
    Ok(usage)
}

/// The model Claude Code names for a message it made up itself (an API
/// error, an interruption): no model wrote it.
const SYNTHETIC_MODEL: &str = "<synthetic>";

/// The model and effort of the messages of a span, and how many messages
/// each pair wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelUse {
    pub model: String,
    /// Absent when the records do not say (older Claude Code).
    pub effort: Option<String>,
    pub messages: i64,
}

/// The models and efforts of the session's own (not a subagent's)
/// assistant messages from `start` to `end` (unix milliseconds, `[start,
/// end)`), the pair that wrote the most messages first (the later one on a
/// tie). A message is counted once, with its first record that names a
/// model. Empty when no message names one.
pub fn span_models(records: &[TranscriptRecord], start: i64, end: i64) -> Vec<ModelUse> {
    let mut seen: HashSet<String> = HashSet::new();
    // (model, effort) → (messages, the last message's position).
    let mut pairs: BTreeMap<(String, Option<String>), (i64, usize)> = BTreeMap::new();
    for (at, record) in records.iter().enumerate() {
        if !record.assistant || record.sidechain || record.at < start || record.at >= end {
            continue;
        }
        let Some(model) = record.model.as_deref().filter(|m| *m != SYNTHETIC_MODEL) else {
            continue;
        };
        let key = record
            .message_id
            .clone()
            .unwrap_or_else(|| format!("#{at}"));
        if !seen.insert(key) {
            continue;
        }
        let pair = pairs
            .entry((model.to_owned(), record.effort.clone()))
            .or_default();
        pair.0 += 1;
        pair.1 = at;
    }
    let mut uses: Vec<(ModelUse, usize)> = pairs
        .into_iter()
        .map(|((model, effort), (messages, last))| {
            (
                ModelUse {
                    model,
                    effort,
                    messages,
                },
                last,
            )
        })
        .collect();
    uses.sort_by(|(a, a_last), (b, b_last)| b.messages.cmp(&a.messages).then(b_last.cmp(a_last)));
    uses.into_iter().map(|(model, _)| model).collect()
}

/// What a `session_closed` records of `uses`: `model` and `effort` of the
/// pair that wrote the most, and `models` (each pair with its `messages`)
/// when there was more than one. Nothing when `uses` is empty.
pub fn models_payload(uses: &[ModelUse], payload: &mut Value) {
    let Some(main) = uses.first() else {
        return;
    };
    payload["model"] = json!(main.model);
    payload["effort"] = json!(main.effort);
    if uses.len() > 1 {
        payload["models"] = uses
            .iter()
            .map(|pair| json!({"model": pair.model, "effort": pair.effort, "messages": pair.messages}))
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::transcript::{Transcript, millis_text};

    const SESSION: &str = "22222222-2222-4222-8222-222222222222";

    fn line(secs: i64, extra: Value) -> String {
        let mut value = json!({
            "type": "assistant",
            "timestamp": millis_text(secs * 1000),
            "sessionId": SESSION,
            "version": "2.1.283",
        });
        for (key, field) in extra.as_object().unwrap() {
            value[key] = field.clone();
        }
        value.to_string()
    }

    fn usage(id: &str, input: i64, output: i64, read: i64, created: i64) -> Value {
        json!({"message": {"id": id, "content": [], "usage": {
            "input_tokens": input,
            "output_tokens": output,
            "cache_read_input_tokens": read,
            "cache_creation_input_tokens": created,
        }}})
    }

    fn records(lines: &[String]) -> Vec<TranscriptRecord> {
        Transcript::parse(&lines.join("\n"), SESSION)
            .unwrap()
            .records
    }

    /// The records of one message count once, with its largest counts; a
    /// subagent's count; records outside the span do not.
    #[test]
    fn a_span_sums_its_messages_once_each() {
        let user = json!({"type": "user", "message": {"content": "hi"}});
        let records = records(&[
            line(5, usage("m0", 1000, 1000, 0, 0)),
            line(10, user),
            line(11, usage("m1", 3, 10, 100, 20)),
            line(12, usage("m1", 3, 40, 100, 20)),
            line(13, {
                let mut sub = usage("m2", 5, 7, 50, 0);
                sub["isSidechain"] = json!(true);
                sub
            }),
            line(
                14,
                json!({"message": {"content": [], "usage": {"input_tokens": 1, "output_tokens": 2}}}),
            ),
            line(20, usage("m3", 1000, 1000, 0, 0)),
        ]);
        let usage = span_usage(&records, 10_000, 20_000).unwrap();
        assert_eq!(
            usage,
            TokenUsage {
                input: 9,
                output: 49,
                cache_read: 150,
                cache_creation: 20,
                messages: 3,
                cost_usd: None,
            }
        );
        let payload = usage.payload();
        assert_eq!(payload["output"], 49);
        assert!(payload.get("cost_usd").is_none());
        // Nothing in the span: zero, not an error.
        assert_eq!(
            span_usage(&records, 30_000, 40_000),
            Ok(TokenUsage::default())
        );
    }

    /// The cost is Claude Code's, and only when every message has one.
    #[test]
    fn the_cost_is_recorded_only_when_every_message_has_one() {
        let costed = |secs, id: &str, cost: f64| {
            let mut value = usage(id, 1, 1, 0, 0);
            value["costUSD"] = json!(cost);
            line(secs, value)
        };
        let all = records(&[costed(1, "a", 0.25), costed(2, "b", 0.125)]);
        let usage = span_usage(&all, 0, 10_000).unwrap();
        assert_eq!(usage.cost_usd, Some(0.375));
        assert_eq!(usage.payload()["cost_usd"], 0.375);
        let some = records(&[costed(1, "a", 0.25), line(2, usage_of("b"))]);
        assert_eq!(span_usage(&some, 0, 10_000).unwrap().cost_usd, None);
    }

    fn usage_of(id: &str) -> Value {
        usage(id, 1, 1, 0, 0)
    }

    fn written(secs: i64, id: &str, model: &str, effort: Option<&str>) -> String {
        let mut value = json!({"message": {"id": id, "model": model, "content": []}});
        if let Some(effort) = effort {
            value["effort"] = json!(effort);
        }
        line(secs, value)
    }

    /// Each message counts once for its model and effort; subagents,
    /// synthetic messages and records outside the span do not count.
    #[test]
    fn a_span_records_the_models_and_efforts_its_messages_used() {
        let opus = "claude-opus-5-5";
        let records = records(&[
            written(5, "m0", "claude-sonnet-5", Some("low")),
            written(10, "m1", opus, Some("medium")),
            written(11, "m1", opus, Some("medium")),
            written(12, "m2", opus, Some("high")),
            written(13, "m3", opus, Some("medium")),
            {
                let mut sub = json!({"isSidechain": true,
                    "message": {"id": "s1", "model": "claude-haiku-4-5", "content": []}});
                sub["effort"] = json!("medium");
                line(14, sub)
            },
            written(15, "m4", "<synthetic>", None),
            written(20, "m5", "claude-sonnet-5", Some("low")),
        ]);
        let uses = span_models(&records, 10_000, 20_000);
        assert_eq!(
            uses,
            vec![
                ModelUse {
                    model: opus.into(),
                    effort: Some("medium".into()),
                    messages: 2,
                },
                ModelUse {
                    model: opus.into(),
                    effort: Some("high".into()),
                    messages: 1,
                },
            ]
        );
        let mut payload = json!({});
        models_payload(&uses, &mut payload);
        assert_eq!(
            payload,
            json!({"model": opus, "effort": "medium", "models": [
                {"model": opus, "effort": "medium", "messages": 2},
                {"model": opus, "effort": "high", "messages": 1},
            ]})
        );
        // One pair: no breakdown. A tie goes to the later pair; an effort
        // the records do not name is null.
        let tie = records_of(&[
            written(1, "a", opus, None),
            written(2, "b", "claude-sonnet-5", Some("medium")),
        ]);
        let uses = span_models(&tie, 0, 10_000);
        assert_eq!(uses[0].model, "claude-sonnet-5");
        let mut payload = json!({});
        models_payload(&uses[1..], &mut payload);
        assert_eq!(payload, json!({"model": opus, "effort": null}));
        // No message names a model: nothing.
        assert!(span_models(&records, 30_000, 40_000).is_empty());
        let mut payload = json!({});
        models_payload(&[], &mut payload);
        assert_eq!(payload, json!({}));
    }

    fn records_of(lines: &[String]) -> Vec<TranscriptRecord> {
        records(lines)
    }

    /// A usage without numeric counts, or assistant records none of which
    /// has a usage, is a format this reader does not know.
    #[test]
    fn an_unknown_usage_is_unsupported() {
        let text = records(&[line(
            1,
            json!({"message": {"id": "a", "usage": {"input_tokens": "3"}}}),
        )]);
        assert_eq!(span_usage(&text, 0, 10_000), Err(USAGE_UNSUPPORTED));
        let none = records(&[line(1, json!({"message": {"id": "a", "content": []}}))]);
        assert_eq!(span_usage(&none, 0, 10_000), Err(USAGE_UNSUPPORTED));
    }
}
