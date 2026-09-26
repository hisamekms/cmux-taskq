//! The tokens a Claude session used (task 199): the `message.usage` of the
//! assistant records of its transcript that fall in a span, summed once per
//! message. The cost is Claude Code's own `costUSD` when every message
//! counted has one; it is never computed from prices. Reading the file is
//! the infrastructure's (`infrastructure::transcripts`); this is pure.

use std::collections::BTreeMap;

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
