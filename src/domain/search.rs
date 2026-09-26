//! Full-text search over tasks, goals, notes and the messages of landed
//! commits (ADR-0046 decision 1). The index is an FTS5 table with the
//! trigram tokenizer (migration 0026): it matches any substring of three or
//! more characters, so Japanese text, which does not separate words with
//! spaces, is found by any part of it. Shorter terms cannot be looked up in
//! a trigram index and are matched with `LIKE` instead.
use serde::{Deserialize, Serialize};

use super::{DomainError, GoalId, TaskStatus};

string_enum!(SearchKind {
    Task => "task",
    Goal => "goal",
    Note => "note",
    Commit => "commit",
});

/// The statuses of a goal as the index records them: its `status` while
/// it is not closed, its verdict once it is.
pub const GOAL_STATUSES: [&str; 4] = ["draft", "open", "achieved", "abandoned"];

/// The characters a term needs to be looked up in the trigram index.
pub const TRIGRAM: usize = 3;

/// Checks a `--status` value: a task's status or a goal's.
pub fn parse_status(value: &str) -> Result<String, DomainError> {
    let value = value.trim();
    if value.parse::<TaskStatus>().is_ok() || GOAL_STATUSES.contains(&value) {
        Ok(value.to_owned())
    } else {
        Err(DomainError::UnknownValue {
            kind: "status",
            value: value.to_owned(),
        })
    }
}

/// What `search` looks for: `terms` as [`parse_terms`] reads them, kept to
/// the given kinds and statuses (empty: all) and to one goal (its own
/// document, its tasks and their notes and commits).
#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub terms: String,
    pub kinds: Vec<SearchKind>,
    pub statuses: Vec<String>,
    pub goal_id: Option<GoalId>,
    pub limit: usize,
    /// Every indexed field in full and the bm25 score, besides the excerpt.
    pub full: bool,
}

/// A query split into what the index runs: an FTS5 expression of the terms
/// of at least [`TRIGRAM`] characters (`long`), and the shorter terms, each
/// of which a document must also contain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchTerms {
    pub fts: Option<String>,
    pub long: Vec<String>,
    pub short: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Term(String),
    Operator(&'static str),
}

/// Reads a query: words separated by white space, `"..."` for a phrase
/// with spaces, and FTS5's `AND`, `OR`, `NOT` and parentheses. Every term is
/// quoted for FTS5, so a path such as `src/domain/task.rs` is one term that
/// matches as a substring. Terms under [`TRIGRAM`] characters must all be
/// present, so they cannot be combined with `OR`, `NOT` or parentheses.
pub fn parse_terms(query: &str) -> Result<SearchTerms, DomainError> {
    let tokens = tokens(query);
    let is_short = |term: &str| term.chars().count() < TRIGRAM;
    let (short, long): (Vec<String>, Vec<String>) = tokens
        .iter()
        .filter_map(|token| match token {
            Token::Term(term) => Some(term.clone()),
            Token::Operator(_) => None,
        })
        .partition(|term| is_short(term));
    if short.is_empty() && long.is_empty() {
        return Err(DomainError::SearchQuery {
            reason: "the query has no term".into(),
        });
    }
    let combines = tokens
        .iter()
        .any(|t| matches!(t, Token::Operator(op) if *op != "AND"));
    if !short.is_empty() && combines {
        return Err(DomainError::SearchQuery {
            reason: format!(
                "terms shorter than {TRIGRAM} characters ({}) cannot be combined with OR, NOT or \
                 parentheses",
                short.join(", ")
            ),
        });
    }
    // A term never contains `"`: it ends a word and delimits a phrase.
    let quote = |term: &str| format!("\"{term}\"");
    let parts: Vec<String> = if short.is_empty() {
        tokens
            .iter()
            .map(|token| match token {
                Token::Term(term) => quote(term),
                Token::Operator(op) => (*op).to_owned(),
            })
            .collect()
    } else {
        // Only AND can remain, which juxtaposition already means.
        tokens
            .iter()
            .filter_map(|token| match token {
                Token::Term(term) if !is_short(term) => Some(quote(term)),
                _ => None,
            })
            .collect()
    };
    Ok(SearchTerms {
        fts: (!parts.is_empty()).then(|| parts.join(" ")),
        long,
        short,
    })
}

fn tokens(query: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut chars = query.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '(' || c == ')' {
            chars.next();
            tokens.push(Token::Operator(if c == '(' { "(" } else { ")" }));
        } else if c == '"' {
            chars.next();
            let phrase: String = chars.by_ref().take_while(|&c| c != '"').collect();
            if !phrase.trim().is_empty() {
                tokens.push(Token::Term(phrase));
            }
        } else {
            let mut word = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() || c == '(' || c == ')' || c == '"' {
                    break;
                }
                word.push(c);
                chars.next();
            }
            tokens.push(match word.as_str() {
                "AND" => Token::Operator("AND"),
                "OR" => Token::Operator("OR"),
                "NOT" => Token::Operator("NOT"),
                _ => Token::Term(word),
            });
        }
    }
    tokens
}

/// The most words of a text [`any_word_query`] asks the index for.
const MAX_QUERY_WORDS: usize = 20;

/// A query for documents holding any word of `text` (a task's title, for
/// the plan review's candidates): its runs of letters, digits and `_ - . /`
/// of at least [`TRIGRAM`] characters, each once (ASCII case ignored), each
/// quoted and joined with `OR`; `None` when it has none.
pub fn any_word_query(text: &str) -> Option<String> {
    let mut words: Vec<String> = Vec::new();
    for word in text.split(|c: char| !(c.is_alphanumeric() || "_-./".contains(c))) {
        let word = word.trim_matches(|c: char| ".-/".contains(c));
        if word.chars().count() < TRIGRAM
            || words.iter().any(|seen| seen.eq_ignore_ascii_case(word))
        {
            continue;
        }
        words.push(word.to_owned());
        if words.len() == MAX_QUERY_WORDS {
            break;
        }
    }
    (!words.is_empty()).then(|| {
        words
            .iter()
            .map(|word| format!("\"{word}\""))
            .collect::<Vec<_>>()
            .join(" OR ")
    })
}

/// What a hit is: a task or goal ID, a note's event ID or a commit's SHA.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SearchRef {
    Id(i64),
    Commit(String),
}

/// One document `search` found. `field` names where the excerpt comes
/// from, with the match between `«` and `»`. A note or a commit carries the
/// task (and run) it belongs to; `fields` and `score` come with `--full`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub kind: SearchKind,
    pub id: SearchRef,
    pub status: Option<String>,
    pub title: String,
    pub field: String,
    pub excerpt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<serde_json::Map<String, serde_json::Value>>,
}

/// The hits, best first (bm25, or the most recently updated when the
/// query has only short terms), and how many documents match in all.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    pub total: usize,
}

/// The name a document's indexed column has in its kind: a goal keeps its
/// constraints in `context`, a note its text and a commit its message in
/// `text`.
pub fn field_name(kind: SearchKind, column: &str) -> &str {
    match (kind, column) {
        (SearchKind::Goal, "context") => "constraints",
        (SearchKind::Commit, "text") => "message",
        _ => column,
    }
}

/// The part of `text` around the first occurrence of `term` (ASCII case
/// ignored, as `LIKE` does), with the occurrence between `«` and `»` and
/// `…` where text is cut; `None` when `text` does not contain it.
pub fn excerpt(text: &str, term: &str, context: usize) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let needle: Vec<char> = term.chars().map(|c| c.to_ascii_lowercase()).collect();
    if needle.is_empty() || needle.len() > chars.len() {
        return None;
    }
    let start = (0..=chars.len() - needle.len()).find(|&i| {
        chars[i..i + needle.len()]
            .iter()
            .zip(&needle)
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
    })?;
    let end = start + needle.len();
    let from = start.saturating_sub(context);
    let to = (end + context).min(chars.len());
    let slice = |a: usize, b: usize| chars[a..b].iter().collect::<String>();
    Some(format!(
        "{}{}«{}»{}{}",
        if from > 0 { "…" } else { "" },
        slice(from, start),
        slice(start, end),
        slice(end, to),
        if to < chars.len() { "…" } else { "" },
    ))
}

/// The first line of `text`, cut to `max` characters with `…`.
pub fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let line = line.trim();
    if line.chars().count() > max {
        format!("{}…", line.chars().take(max).collect::<String>())
    } else {
        line.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(query: &str) -> SearchTerms {
        parse_terms(query).unwrap()
    }

    #[test]
    fn long_terms_are_quoted_for_fts5_with_their_operators() {
        assert_eq!(
            terms("src/domain/task.rs 全文検索"),
            SearchTerms {
                fts: Some("\"src/domain/task.rs\" \"全文検索\"".into()),
                long: vec!["src/domain/task.rs".into(), "全文検索".into()],
                short: vec![],
            }
        );
        assert_eq!(
            terms("(search OR \"related task\") NOT embed AND fts5").fts,
            Some("( \"search\" OR \"related task\" ) NOT \"embed\" AND \"fts5\"".into())
        );
        assert_eq!(terms("a\"b c\"").short, vec!["a".to_owned()]);
    }

    #[test]
    fn short_terms_are_matched_apart_and_only_with_and() {
        assert_eq!(
            terms("重複 AND search"),
            SearchTerms {
                fts: Some("\"search\"".into()),
                long: vec!["search".into()],
                short: vec!["重複".into()],
            }
        );
        assert_eq!(terms("重複 id").fts, None);
        assert!(matches!(
            parse_terms("重複 OR search"),
            Err(DomainError::SearchQuery { .. })
        ));
        assert!(matches!(
            parse_terms("  AND \"\" "),
            Err(DomainError::SearchQuery { .. })
        ));
    }

    #[test]
    fn any_word_query_takes_each_long_word_once() {
        let query =
            any_word_query("runtime: plan review の prompt に、Plan AND (src/domain/task.rs).")
                .unwrap();
        assert_eq!(
            query,
            "\"runtime\" OR \"plan\" OR \"review\" OR \"prompt\" OR \"AND\" OR \"src/domain/task.rs\""
        );
        assert_eq!(
            parse_terms(&query).unwrap().fts.as_deref(),
            Some(query.as_str())
        );
        assert_eq!(any_word_query("の に 、 ab"), None);
        let many: String = (100..200).map(|n| format!("w{n} ")).collect();
        assert_eq!(
            any_word_query(&many).unwrap().matches(" OR ").count(),
            MAX_QUERY_WORDS - 1
        );
    }

    #[test]
    fn statuses_are_those_of_tasks_and_goals() {
        assert_eq!(parse_status(" completed").unwrap(), "completed");
        assert_eq!(parse_status("achieved").unwrap(), "achieved");
        assert!(parse_status("closed").is_err());
    }

    #[test]
    fn excerpts_mark_the_first_occurrence() {
        assert_eq!(
            excerpt("計画の重複を見つける", "重複", 2).as_deref(),
            Some("…画の«重複»を見…")
        );
        assert_eq!(excerpt("Rust ID", "id", 10).as_deref(), Some("Rust «ID»"));
        assert_eq!(excerpt("abc", "abcd", 3), None);
        assert_eq!(excerpt("abc", "", 3), None);
        assert_eq!(excerpt("abc", "x", 3), None);
    }

    #[test]
    fn titles_and_field_names() {
        assert_eq!(
            first_line("\n  runtime: search\nbody", 80),
            "runtime: search"
        );
        assert_eq!(first_line("abcdef", 3), "abc…");
        assert_eq!(field_name(SearchKind::Goal, "context"), "constraints");
        assert_eq!(field_name(SearchKind::Commit, "text"), "message");
        assert_eq!(field_name(SearchKind::Note, "text"), "text");
    }
}
