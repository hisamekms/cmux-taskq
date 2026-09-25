//! `dagq related TASK` on the queue (ADR-0046 decision 4): reads every
//! task with its landed commits' messages, follow-up origins and duplicate
//! mark, asks the search index how strongly the task's title matches each
//! other task, and leaves the scoring to [`crate::domain::related`].
use std::collections::HashMap;

use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};

use super::sqlite::SqliteQueue;
use crate::domain::related::{RelatedDoc, RelatedPage, rank};
use crate::domain::search::TRIGRAM;

/// The most trigrams of a title the search clue asks the index for.
const MAX_TRIGRAMS: usize = 200;

impl SqliteQueue {
    /// The tasks most related to `task_id`, best first, kept to `statuses`
    /// (empty: all) and cut to `limit`.
    pub fn related(&self, task_id: i64, statuses: &[String], limit: usize) -> Result<RelatedPage> {
        ensure!(limit > 0, "limit must be at least 1");
        let title: String = self
            .conn
            .query_row("SELECT title FROM tasks WHERE id = ?1", [task_id], |row| {
                row.get(0)
            })
            .optional()?
            .with_context(|| format!("task {task_id} does not exist"))?;
        let docs = self.related_docs()?;
        let search = self.title_matches(task_id, &title)?;
        rank(task_id, &docs, &search, statuses, limit)
            .with_context(|| format!("task {task_id} does not exist"))
    }

    fn related_docs(&self) -> Result<Vec<RelatedDoc>> {
        let mut docs: Vec<RelatedDoc> = self
            .conn
            .prepare(
                "SELECT id, status, title, description, acceptance, context, goal_id, paths
                 FROM tasks ORDER BY id",
            )?
            .query_map([], |row| {
                let title: String = row.get("title")?;
                let paths: String = row.get("paths")?;
                Ok(RelatedDoc {
                    id: row.get("id")?,
                    status: row.get("status")?,
                    goal_id: row.get("goal_id")?,
                    paths: serde_json::from_str(&paths).unwrap_or_default(),
                    texts: vec![
                        title.clone(),
                        row.get("description")?,
                        row.get("acceptance")?,
                        row.get("context")?,
                    ],
                    title,
                    ..RelatedDoc::default()
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let position: HashMap<i64, usize> = docs
            .iter()
            .enumerate()
            .map(|(i, doc)| (doc.id, i))
            .collect();
        let messages: Vec<(i64, String)> = self
            .conn
            .prepare(
                "SELECT task_id, message FROM landed_commits
                 WHERE message IS NOT NULL ORDER BY id",
            )?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (task_id, message) in messages {
            if let Some(&i) = position.get(&task_id) {
                docs[i].texts.push(message);
            }
        }
        // The run whose receipt proposed a follow-up is on the event, with
        // the task it ran; the registered task is in the payload.
        let follow_ups: Vec<(i64, String, i64)> = self
            .conn
            .prepare(
                "SELECT CAST(json_extract(payload, '$.task_id') AS INTEGER), run_id, task_id
                 FROM run_events
                 WHERE kind = 'follow_up_registered' AND run_id IS NOT NULL
                   AND task_id IS NOT NULL
                   AND json_type(payload, '$.task_id') = 'integer'
                 ORDER BY id",
            )?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (registered, run_id, source) in follow_ups {
            if let Some(&i) = position.get(&registered) {
                docs[i].follow_up_of.push((run_id, source));
            }
        }
        // A cancel records what the task duplicates (decision 5); the last
        // cancel of a task still canceled is the one that holds.
        let duplicates: Vec<(i64, Option<i64>)> = self
            .conn
            .prepare(
                "SELECT e.task_id,
                        CASE WHEN json_type(e.payload, '$.duplicate_of') = 'integer'
                             THEN json_extract(e.payload, '$.duplicate_of') END
                 FROM run_events e JOIN tasks t ON t.id = e.task_id
                 WHERE e.kind = 'task_status_changed' AND t.status = 'canceled'
                   AND json_extract(e.payload, '$.to') = 'canceled'
                 ORDER BY e.id",
            )?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (task_id, duplicate_of) in duplicates {
            if let Some(&i) = position.get(&task_id) {
                docs[i].duplicate_of = duplicate_of;
            }
        }
        Ok(docs)
    }

    /// How strongly the index matches the trigrams of `title` against each
    /// other task's title and description, as the ratio of its bm25 to that
    /// of the task itself (1: as strongly as the task matches itself).
    fn title_matches(&self, task_id: i64, title: &str) -> Result<HashMap<i64, f64>> {
        let Some(query) = trigram_query(title) else {
            return Ok(HashMap::new());
        };
        let scores: Vec<(i64, f64)> = self
            .conn
            .prepare(
                "SELECT ref, bm25(search_index) FROM search_index
                 WHERE search_index MATCH ?1 AND kind = 'task'",
            )?
            .query_map(params![query], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let own = scores
            .iter()
            .find(|(id, _)| *id == task_id)
            .map(|(_, score)| *score)
            .filter(|score| *score < 0.0);
        let Some(own) = own else {
            return Ok(HashMap::new());
        };
        Ok(scores
            .into_iter()
            .filter(|(id, _)| *id != task_id)
            .map(|(id, score)| (id, (score / own).clamp(0.0, 1.0)))
            .collect())
    }
}

/// An FTS5 query for any of the distinct trigrams of `title` that hold a
/// letter or a digit, on the title and description columns. The index
/// folds case itself, so the title is taken as written.
fn trigram_query(title: &str) -> Option<String> {
    let chars: Vec<char> = title.chars().collect();
    let mut seen = Vec::new();
    for window in chars.windows(TRIGRAM) {
        let gram: String = window.iter().collect();
        if window.iter().any(|c| c.is_whitespace() || *c == '"')
            || !window.iter().any(|c| c.is_alphanumeric())
            || seen.contains(&gram)
        {
            continue;
        }
        seen.push(gram);
        if seen.len() == MAX_TRIGRAMS {
            break;
        }
    }
    (!seen.is_empty()).then(|| {
        let terms: Vec<String> = seen.iter().map(|gram| format!("\"{gram}\"")).collect();
        format!("{{title description}} : ({})", terms.join(" OR "))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigrams_skip_spaces_and_punctuation_and_repeat_once() {
        assert_eq!(
            trigram_query("ab abab: ..").as_deref(),
            Some("{title description} : (\"aba\" OR \"bab\" OR \"ab:\")")
        );
        assert_eq!(trigram_query("a b"), None);
        // FTS5's operators and special characters stay inside the quotes.
        assert_eq!(
            trigram_query("(a*b) ^cd").as_deref(),
            Some("{title description} : (\"(a*\" OR \"a*b\" OR \"*b)\" OR \"^cd\")")
        );
    }
}
