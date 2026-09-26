//! A Claude Code transcript as the runtime reads it (ADR-0048 decisions 5
//! and 9): its records, reduced to what the turns (and task 199's token
//! counts) need, and the turns they make. Reading the file is the
//! infrastructure's (`infrastructure::transcripts`); this module is pure.

use serde::Serialize;
use serde_json::Value;

use super::stats::rfc3339_millis;

/// What a record is, for the turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordRole {
    /// An actual input: a `user` record that is not a sidechain's, not
    /// meta, not only `tool_result`s and not a compaction summary. It
    /// starts a turn.
    Input,
    /// An `assistant` record, or a `user` record of `tool_result`s: work
    /// of the turn, which ends at the last of them.
    Output,
    /// Anything else (attachments, system records, snapshots).
    Other,
}

/// One record of a transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptRecord {
    /// Unix milliseconds of its `timestamp`.
    pub at: i64,
    pub role: RecordRole,
    /// A subagent's record: never a turn's boundary (its time is in the
    /// parent's turn).
    pub sidechain: bool,
    /// The Claude Code version that wrote it.
    pub version: Option<String>,
    /// `message.usage` of an `assistant` record, for task 199.
    pub usage: Option<Value>,
}

impl TranscriptRecord {
    /// The record of one parsed JSONL line, `None` when it lacks what a
    /// turn needs (`type`, a parseable `timestamp`, `sessionId`), as the
    /// session's other bookkeeping lines do.
    pub fn from_line(line: &Value) -> Option<(Self, String)> {
        let kind = line.get("type")?.as_str()?;
        let at = rfc3339_millis(line.get("timestamp")?.as_str()?)?;
        let session_id = line.get("sessionId")?.as_str()?.to_owned();
        let sidechain = line["isSidechain"].as_bool().unwrap_or(false);
        let role = match kind {
            "assistant" => RecordRole::Output,
            "user" if only_tool_results(&line["message"]["content"]) => RecordRole::Output,
            "user"
                if !sidechain
                    && line["isMeta"].as_bool() != Some(true)
                    && line["isCompactSummary"].as_bool() != Some(true) =>
            {
                RecordRole::Input
            }
            _ => RecordRole::Other,
        };
        let usage = (kind == "assistant")
            .then(|| line["message"].get("usage").cloned())
            .flatten();
        Some((
            Self {
                at,
                role,
                sidechain,
                version: line["version"].as_str().map(str::to_owned),
                usage,
            },
            session_id,
        ))
    }
}

/// Whether a `user` message's content is `tool_result`s only.
fn only_tool_results(content: &Value) -> bool {
    content.as_array().is_some_and(|parts| {
        !parts.is_empty() && parts.iter().all(|part| part["type"] == "tool_result")
    })
}

/// Why a transcript could not be read; its code goes into
/// `active_unavailable` of the span's `session_closed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable {
    pub code: &'static str,
    /// The Claude Code version of the records that were read, if any.
    pub version: Option<String>,
    pub detail: String,
}

pub const TRANSCRIPT_MISSING: &str = "transcript_missing";
pub const TRANSCRIPT_UNPARSABLE: &str = "transcript_unparsable";
pub const TRANSCRIPT_UNSUPPORTED: &str = "transcript_unsupported";
pub const SESSION_MISMATCH: &str = "session_mismatch";
/// The span has no session id to find its transcript by.
pub const SESSION_UNKNOWN: &str = "session_unknown";

/// The records of a readable transcript, in file order, and the lines
/// skipped because they were not JSON.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Transcript {
    pub records: Vec<TranscriptRecord>,
    pub skipped: usize,
}

impl Transcript {
    /// Parse the JSONL `text` of the transcript of `session_id`. Lines that
    /// are not JSON are skipped; lines without the fields a turn needs are
    /// left out. Unreadable when no line is JSON, none has those fields, or
    /// they name another session.
    pub fn parse(text: &str, session_id: &str) -> Result<Self, Unreadable> {
        let mut transcript = Self::default();
        let mut parsed = 0;
        let mut version = None;
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                transcript.skipped += 1;
                continue;
            };
            parsed += 1;
            let Some((record, id)) = TranscriptRecord::from_line(&value) else {
                continue;
            };
            if record.version.is_some() {
                version.clone_from(&record.version);
            }
            if id != session_id {
                return Err(Unreadable {
                    code: SESSION_MISMATCH,
                    version,
                    detail: format!("a record names session {id}, not {session_id}"),
                });
            }
            transcript.records.push(record);
        }
        if parsed == 0 {
            return Err(Unreadable {
                code: TRANSCRIPT_UNPARSABLE,
                version,
                detail: format!("none of its lines is JSON ({} skipped)", transcript.skipped),
            });
        }
        if transcript.records.is_empty() {
            return Err(Unreadable {
                code: TRANSCRIPT_UNSUPPORTED,
                version,
                detail: format!("none of its {parsed} records has type, timestamp and sessionId"),
            });
        }
        Ok(transcript)
    }

    /// The Claude Code version of its last record that has one.
    pub fn version(&self) -> Option<&str> {
        self.records.iter().rev().find_map(|r| r.version.as_deref())
    }

    /// The time of its last record.
    pub fn last_at(&self) -> Option<i64> {
        self.records.iter().map(|r| r.at).max()
    }
}

/// One turn: from an actual input to the last output before the next one,
/// in unix milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Turn {
    pub start: i64,
    pub end: i64,
}

impl Turn {
    pub fn millis(self) -> i64 {
        self.end - self.start
    }

    /// Milliseconds of it between `from` and `to`.
    pub fn overlap(self, from: i64, to: i64) -> i64 {
        (self.end.min(to) - self.start.max(from)).max(0)
    }
}

/// The turns of `records`: every one but the last is complete (the next
/// input came); the last is complete only once the session ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Turns {
    pub complete: Vec<Turn>,
    pub last: Option<Turn>,
}

impl Turns {
    /// Every turn, the last one included (the session ended).
    pub fn all(&self) -> impl Iterator<Item = Turn> + '_ {
        self.complete.iter().copied().chain(self.last)
    }
}

/// The turns of `records` (ADR-0048 decision 5). A turn ends at the last
/// output before the next input, a sidechain's included; one with no
/// output is 0 long. Records before the first input belong to no turn.
pub fn turns(records: &[TranscriptRecord]) -> Turns {
    let mut turns = Turns::default();
    for record in records {
        match record.role {
            RecordRole::Input => {
                turns.complete.extend(turns.last.take());
                turns.last = Some(Turn {
                    start: record.at,
                    end: record.at,
                });
            }
            RecordRole::Output => {
                if let Some(turn) = turns.last.as_mut() {
                    turn.end = turn.end.max(record.at);
                }
            }
            RecordRole::Other => {}
        }
    }
    turns
}

/// The turns of a span open from `start` to `end` (unix milliseconds;
/// `None` while it is open) that start after `after`: a turn belongs to the
/// span its input is in, and is cut at the span's end.
pub fn span_turns(
    turns: impl IntoIterator<Item = Turn>,
    start: i64,
    end: Option<i64>,
    after: Option<i64>,
) -> Vec<Turn> {
    turns
        .into_iter()
        .filter(|turn| {
            turn.start >= start
                && end.is_none_or(|end| turn.start < end)
                && after.is_none_or(|after| turn.start > after)
        })
        .map(|turn| Turn {
            start: turn.start,
            end: end.map_or(turn.end, |end| turn.end.min(end)),
        })
        .collect()
}

/// RFC 3339 in UTC with milliseconds of unix milliseconds, the form of
/// `created_at`.
pub fn millis_text(millis: i64) -> String {
    let days = millis.div_euclid(86_400_000);
    let rest = millis.rem_euclid(86_400_000);
    // Civil date from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1000 % 60,
        rest % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SESSION: &str = "11111111-1111-4111-8111-111111111111";

    fn at(secs: i64) -> String {
        millis_text(1_790_000_000_000 + secs * 1000)
    }

    fn ms(secs: i64) -> i64 {
        1_790_000_000_000 + secs * 1000
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
        line(
            "user",
            secs,
            json!({"message": {"role": "user", "content": text}}),
        )
    }

    fn assistant(secs: i64) -> String {
        line(
            "assistant",
            secs,
            json!({"message": {"content": [{"type": "tool_use"}], "usage": {"output_tokens": 3}}}),
        )
    }

    fn tool_result(secs: i64) -> String {
        line(
            "user",
            secs,
            json!({"message": {"content": [{"type": "tool_result", "content": "ok"}]}}),
        )
    }

    fn parse(lines: &[String]) -> Result<Transcript, Unreadable> {
        Transcript::parse(&lines.join("\n"), SESSION)
    }

    /// Several turns, one with tools and a subagent in it: each runs from
    /// its input to the last output before the next input.
    #[test]
    fn turns_run_from_an_input_to_the_last_output_before_the_next() {
        let lines = [
            r#"{"type":"mode","mode":"normal","sessionId":"x"}"#.to_owned(),
            line("attachment", 0, json!({})),
            input(1, "do it"),
            assistant(3),
            tool_result(10),
            // A subagent's input is not a turn: its time is the parent's.
            line(
                "user",
                12,
                json!({"isSidechain": true, "message": {"content": "sub task"}}),
            ),
            line("assistant", 20, json!({"isSidechain": true})),
            tool_result(21),
            assistant(25),
            line("system", 26, json!({"subtype": "turn_duration"})),
            // A meta input and a compaction summary do not start a turn.
            line(
                "user",
                30,
                json!({"isMeta": true, "message": {"content": "caveat"}}),
            ),
            line(
                "user",
                31,
                json!({"isCompactSummary": true, "message": {"content": "summary"}}),
            ),
            input(100, "next"),
            assistant(104),
            input(200, "third"),
        ];
        let transcript = parse(&lines).unwrap();
        assert_eq!(transcript.version(), Some("2.1.283"));
        assert_eq!(transcript.last_at(), Some(ms(200)));
        let usage: Vec<_> = transcript
            .records
            .iter()
            .filter_map(|r| r.usage.clone())
            .collect();
        assert_eq!(usage.len(), 3);
        let turns = turns(&transcript.records);
        assert_eq!(
            turns.complete,
            vec![
                Turn {
                    start: ms(1),
                    end: ms(25)
                },
                Turn {
                    start: ms(100),
                    end: ms(104)
                },
            ]
        );
        // The input that got no answer is 0 long, and not complete yet.
        assert_eq!(
            turns.last,
            Some(Turn {
                start: ms(200),
                end: ms(200)
            })
        );
        assert_eq!(turns.all().map(Turn::millis).sum::<i64>(), 28_000);
    }

    /// A transcript cut mid-line keeps what was read; a mid-turn cut ends
    /// the turn at its last output.
    #[test]
    fn a_cut_transcript_keeps_its_complete_lines() {
        let mut lines = vec![input(0, "go"), assistant(5), tool_result(7)];
        let partial = assistant(9);
        lines.push(partial[..partial.len() / 2].to_owned());
        let transcript = parse(&lines).unwrap();
        assert_eq!(transcript.skipped, 1);
        let turns = turns(&transcript.records);
        assert!(turns.complete.is_empty());
        assert_eq!(
            turns.last,
            Some(Turn {
                start: ms(0),
                end: ms(7)
            })
        );
    }

    #[test]
    fn unreadable_transcripts_say_why() {
        let unparsable = Transcript::parse("not json\n{also not", SESSION).unwrap_err();
        assert_eq!(unparsable.code, TRANSCRIPT_UNPARSABLE);
        let unsupported =
            Transcript::parse(r#"{"kind":"user","time":"2026-01-01T00:00:00Z"}"#, SESSION)
                .unwrap_err();
        assert_eq!(unsupported.code, TRANSCRIPT_UNSUPPORTED);
        let other =
            Transcript::parse(&input(0, "hi"), "22222222-2222-4222-8222-222222222222").unwrap_err();
        assert_eq!(other.code, SESSION_MISMATCH);
        assert_eq!(other.version.as_deref(), Some("2.1.283"));
        assert_eq!(
            Transcript::parse("", SESSION).unwrap_err().code,
            TRANSCRIPT_UNPARSABLE
        );
    }

    /// A turn belongs to the span its input is in, cut at the span's end,
    /// and only turns after `after` are new.
    #[test]
    fn span_turns_keep_the_turns_that_start_in_the_span() {
        let turns = [
            Turn { start: 0, end: 50 },
            Turn {
                start: 100,
                end: 300,
            },
            Turn {
                start: 400,
                end: 500,
            },
        ];
        assert_eq!(
            span_turns(turns, 50, Some(250), None),
            vec![Turn {
                start: 100,
                end: 250
            }]
        );
        assert_eq!(
            span_turns(turns, 0, None, Some(100)),
            vec![Turn {
                start: 400,
                end: 500
            }]
        );
        assert_eq!(
            Turn {
                start: 100,
                end: 300
            }
            .overlap(200, 1000),
            100
        );
        assert_eq!(
            Turn {
                start: 100,
                end: 300
            }
            .overlap(400, 1000),
            0
        );
    }

    #[test]
    fn millis_text_is_the_created_at_form() {
        assert_eq!(millis_text(0), "1970-01-01T00:00:00.000Z");
        let text = "2026-09-26T07:26:05.985Z";
        assert_eq!(millis_text(rfc3339_millis(text).unwrap()), text);
        assert_eq!(
            millis_text(rfc3339_millis("2024-02-29T23:59:59.001Z").unwrap()),
            "2024-02-29T23:59:59.001Z"
        );
    }
}
