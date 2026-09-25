//! `dagq search` on the queue's FTS5 index (ADR-0046 decisions 1–3). The
//! index and the triggers that keep it current are migration 0026's; this
//! module only reads it, and fills the landed commits whose message the
//! `run_integrated` payload lacked.
use anyhow::{Result, ensure};
use rusqlite::{Row, params, params_from_iter, types::Value};
use serde_json::{Map, json};

use super::sqlite::SqliteQueue;
use crate::domain::DomainError;
use crate::domain::search::{
    SearchHit, SearchKind, SearchPage, SearchQuery, SearchRef, excerpt, field_name, first_line,
    parse_terms,
};

/// The indexed columns, in the order of the table, and their index in it.
const COLUMNS: [(&str, usize); 5] = [
    ("title", 7),
    ("description", 8),
    ("acceptance", 9),
    ("context", 10),
    ("text", 11),
];
/// Around the match in an excerpt: characters on each side, and FTS5
/// tokens (trigrams, the most `snippet` takes) for the fallback.
const SNIPPET_TOKENS: usize = 64;
const EXCERPT_CHARS: usize = 24;
/// What `snippet` marks a match with, replaced by `«` and `»` once read, so
/// a column containing those characters is not taken for a match.
const OPEN: &str = "\u{2}";
const CLOSE: &str = "\u{3}";
const TITLE_CHARS: usize = 80;

impl SqliteQueue {
    /// Documents matching `query`, best first. Terms of three or more
    /// characters are looked up in the index and ranked by bm25; shorter
    /// ones are `LIKE` filters, and a query with only those is ordered by
    /// the most recent update.
    pub fn search(&self, query: &SearchQuery) -> Result<SearchPage> {
        ensure!(query.limit > 0, "limit must be at least 1");
        let terms = parse_terms(&query.terms)?;
        let mut filters = Vec::new();
        let mut values: Vec<Value> = Vec::new();
        if let Some(fts) = &terms.fts {
            filters.push("search_index MATCH ?".to_owned());
            values.push(Value::from(fts.clone()));
        }
        let all = COLUMNS
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(" || char(10) || ");
        for short in &terms.short {
            filters.push(format!("({all}) LIKE ? ESCAPE '\\'"));
            let escaped = short
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            values.push(Value::from(format!("%{escaped}%")));
        }
        if !query.kinds.is_empty() {
            filters.push(format!("kind IN ({})", marks(query.kinds.len())));
            values.extend(
                query
                    .kinds
                    .iter()
                    .map(|kind| Value::from(kind.as_str().to_owned())),
            );
        }
        if !query.statuses.is_empty() {
            filters.push(format!("status IN ({})", marks(query.statuses.len())));
            values.extend(query.statuses.iter().cloned().map(Value::from));
        }
        if let Some(goal_id) = query.goal_id {
            filters.push("goal_id = ?".into());
            values.push(Value::from(goal_id.as_i64()));
        }
        let filter = filters.join(" AND ");
        // FTS5 judges the operators' placement (`foo OR`, `NOT foo`, `()`).
        let total: i64 = self
            .conn
            .query_row(
                &format!("SELECT count(*) FROM search_index WHERE {filter}"),
                params_from_iter(&values),
                |row| row.get(0),
            )
            .map_err(|error| match error.to_string() {
                message if message.contains("fts5") => {
                    anyhow::Error::from(DomainError::SearchQuery { reason: message })
                }
                _ => error.into(),
            })?;
        let ranked = terms.fts.is_some();
        let (score, order) = if ranked {
            ("bm25(search_index)", "score, rowid DESC")
        } else {
            ("NULL", "updated_at DESC, rowid DESC")
        };
        let snippets: String = COLUMNS
            .iter()
            .map(|(name, index)| {
                if ranked {
                    format!(
                        ", snippet(search_index, {index}, char(2), char(3), '…', {SNIPPET_TOKENS}) \
                         AS snippet_{name}"
                    )
                } else {
                    format!(", NULL AS snippet_{name}")
                }
            })
            .collect();
        values.push(Value::from(i64::try_from(query.limit)?));
        let sql = format!(
            "SELECT kind, ref, task_id, goal_id, run_id, status, title, description, acceptance, \
                    context, text, {score} AS score{snippets}
             FROM search_index WHERE {filter} ORDER BY {order} LIMIT ?"
        );
        let words: Vec<String> = terms.long.iter().chain(&terms.short).cloned().collect();
        let hits = self
            .conn
            .prepare(&sql)?
            .query_map(params_from_iter(&values), |row| {
                hit(row, &words, query.full)
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(SearchPage {
            hits,
            total: usize::try_from(total)?,
        })
    }

    /// Fill the message of each landed commit recorded without one, read by
    /// `read(git_common_dir, commit)` (the Git directory the landing
    /// recorded, if any). Returns how many were filled; a commit `read`
    /// cannot find keeps waiting.
    pub fn fill_commit_messages(
        &mut self,
        read: impl Fn(Option<&str>, &str) -> Option<String>,
    ) -> Result<usize> {
        let missing: Vec<(i64, String, Option<String>)> = self
            .conn
            .prepare(
                "SELECT id, commit_sha, git_common_dir FROM landed_commits
                 WHERE message IS NULL ORDER BY id",
            )?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut filled = 0;
        for (id, commit, common_dir) in missing {
            if let Some(message) = read(common_dir.as_deref(), &commit) {
                filled += self.conn.execute(
                    "UPDATE landed_commits SET message = ?2 WHERE id = ?1 AND message IS NULL",
                    params![id, message],
                )?;
            }
        }
        Ok(filled)
    }
}

fn marks(n: usize) -> String {
    vec!["?"; n].join(", ")
}

fn hit(row: &Row<'_>, terms: &[String], full: bool) -> rusqlite::Result<SearchHit> {
    let kind: String = row.get("kind")?;
    let kind: SearchKind = kind.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(format!("{error}"))),
        )
    })?;
    let texts: Vec<(&str, String)> = COLUMNS
        .iter()
        .map(|(name, _)| Ok((*name, row.get::<_, String>(*name)?)))
        .collect::<rusqlite::Result<_>>()?;
    let snippets: Vec<Option<String>> = COLUMNS
        .iter()
        .map(|(name, _)| row.get(format!("snippet_{name}").as_str()))
        .collect::<rusqlite::Result<_>>()?;
    // The first column holding a term, around its first occurrence; else
    // the column FTS5 matched, as its snippet (a trigram snippet can cut a
    // match short, so it is only the fallback); else the first text.
    let found = texts.iter().find_map(|(name, text)| {
        terms
            .iter()
            .find_map(|term| excerpt(text, term, EXCERPT_CHARS))
            .map(|excerpt| (*name, excerpt))
    });
    let (column, excerpt) = found
        .or_else(|| {
            snippets
                .iter()
                .zip(&texts)
                .find_map(|(snippet, (name, _))| {
                    snippet
                        .as_deref()
                        .filter(|s| s.contains(OPEN))
                        .map(|s| (*name, s.replace(OPEN, "«").replace(CLOSE, "»")))
                })
        })
        .unwrap_or_else(|| {
            let (name, text) = texts
                .iter()
                .find(|(_, text)| !text.is_empty())
                .unwrap_or(&texts[0]);
            (*name, first_line(text, TITLE_CHARS))
        });
    let text_of = |name: &str| {
        texts
            .iter()
            .find(|(column, _)| *column == name)
            .map(|(_, text)| text.as_str())
            .unwrap_or("")
    };
    let title = match kind {
        SearchKind::Task | SearchKind::Goal => text_of("title").to_owned(),
        SearchKind::Note | SearchKind::Commit => first_line(text_of("text"), TITLE_CHARS),
    };
    let id = match kind {
        SearchKind::Commit => SearchRef::Commit(row.get("ref")?),
        _ => SearchRef::Id(row.get("ref")?),
    };
    let fields = full.then(|| {
        texts
            .iter()
            .filter(|(name, text)| match kind {
                SearchKind::Task | SearchKind::Goal => *name != "text",
                SearchKind::Note | SearchKind::Commit => *name == "text" || !text.is_empty(),
            })
            .map(|(name, text)| (field_name(kind, name).to_owned(), json!(text)))
            .collect::<Map<_, _>>()
    });
    Ok(SearchHit {
        kind,
        id,
        status: row.get("status")?,
        title,
        field: field_name(kind, column).to_owned(),
        excerpt,
        task_id: row
            .get::<_, Option<i64>>("task_id")?
            .filter(|_| kind != SearchKind::Task),
        run_id: row.get("run_id")?,
        goal_id: row
            .get::<_, Option<i64>>("goal_id")?
            .filter(|_| kind != SearchKind::Goal),
        score: if full { row.get("score")? } else { None },
        fields,
    })
}
