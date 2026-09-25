//! `dagq related TASK` (ADR-0046 decision 4): the tasks most related to
//! one task, scored by fixed rules on clues that strings and structure
//! give, each candidate with the clues that scored it. Nothing here reads
//! meaning: a person or an LLM judges the few candidates it lists.
//!
//! The clues and their weights (docs/design/persistence.md lists them):
//! declared `paths` that overlap, file names, test names (snake_case
//! identifiers of three or more words, and `--test NAME`), ADR numbers and
//! task numbers in the texts, follow-ups of the same run, the same goal,
//! and how strongly the index matches one title against the other task
//! (`search`). A clue many tasks share counts less: its weight is scaled by
//! [`rarity`].
use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use super::{DomainError, scope::glob_matches};

/// A clue at full weight (shared by two tasks only).
pub const PATH_WEIGHT: f64 = 1.0;
pub const FILE_WEIGHT: f64 = 2.0;
pub const TEST_WEIGHT: f64 = 3.0;
pub const ADR_WEIGHT: f64 = 2.0;
/// One task's text names the other.
pub const MENTION_WEIGHT: f64 = 3.0;
/// Both texts name the same third task.
pub const SHARED_MENTION_WEIGHT: f64 = 2.0;
/// One task was registered as a follow-up of the other's run.
pub const FOLLOW_UP_OF_WEIGHT: f64 = 3.0;
/// Both were registered as follow-ups of the same run.
pub const SAME_RUN_WEIGHT: f64 = 2.0;
pub const GOAL_WEIGHT: f64 = 1.5;
/// The index matching the task's title against the other task as well as
/// against the task itself.
pub const SEARCH_WEIGHT: f64 = 4.0;
/// A search match weaker than this, relative to the task's own, is noise.
pub const SEARCH_FLOOR: f64 = 0.1;
/// Test-name clues need this many snake_case words.
const TEST_NAME_WORDS: usize = 3;
/// File extensions a path clue has.
const FILE_EXTENSIONS: [&str; 8] = ["rs", "md", "sql", "toml", "sh", "json", "yml", "yaml"];

/// What `related` knows of one task.
#[derive(Debug, Clone, Default)]
pub struct RelatedDoc {
    pub id: i64,
    pub status: String,
    pub title: String,
    pub goal_id: Option<i64>,
    pub paths: Vec<String>,
    /// Title, description, acceptance, context and, once landed, the
    /// messages of its landed commits.
    pub texts: Vec<String>,
    /// The runs whose receipts registered it as a follow-up, with the task
    /// of each run.
    pub follow_up_of: Vec<(String, i64)>,
    /// The task it was canceled as a duplicate of (ADR-0046 decision 5).
    pub duplicate_of: Option<i64>,
}

/// The clues a task's texts give.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextClues {
    pub files: BTreeSet<String>,
    pub tests: BTreeSet<String>,
    pub adrs: BTreeSet<u32>,
    pub tasks: BTreeSet<i64>,
}

string_enum!(ClueKind {
    Path => "path",
    File => "file",
    Test => "test",
    Adr => "adr",
    Mentions => "mentions",
    MentionedBy => "mentioned_by",
    SharedMention => "shared_mention",
    FollowUpOf => "follow_up_of",
    SameRun => "same_run",
    Goal => "goal",
    Search => "search",
});

/// One clue that scored a candidate: its kind, what matched and what it
/// added to the score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clue {
    pub clue: ClueKind,
    pub value: String,
    pub weight: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelatedTask {
    pub id: i64,
    pub status: String,
    pub title: String,
    pub score: f64,
    pub clues: Vec<Clue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<i64>,
}

/// The candidates, best first, and how many tasks scored above zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelatedPage {
    pub task_id: i64,
    pub related: Vec<RelatedTask>,
    pub total: usize,
}

/// How much a clue shared by `df` of `n` tasks counts, from 1 (only the
/// two) down to 0 (every task): `ln((n+1)/df) / ln((n+1)/2)`.
pub fn rarity(df: usize, n: usize) -> f64 {
    let (df, n) = (df.max(1) as f64, n as f64);
    let full = ((n + 1.0) / 2.0).ln();
    if full <= 0.0 {
        return 1.0;
    }
    (((n + 1.0) / df).ln() / full).clamp(0.0, 1.0)
}

/// Files, test names, ADR numbers and task numbers named in `text`.
pub fn text_clues(text: &str) -> TextClues {
    let mut clues = TextClues::default();
    add_text_clues(&mut clues, text);
    clues
}

fn add_text_clues(clues: &mut TextClues, text: &str) {
    for (start, word) in ascii_words(text) {
        if let Some(file) = file_name(word) {
            clues.files.insert(file.to_owned());
        }
        if let Some(adr) = adr_number(word) {
            clues.adrs.insert(adr);
        }
        let word = word.trim_end_matches('.');
        if is_test_name(word) {
            clues.tests.insert(word.to_owned());
        }
        if word == "--test"
            && let Some((_, name)) = ascii_words(&text[start + word.len()..]).next()
            && text[start + word.len()..].trim_start().starts_with(name)
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            clues.tests.insert(name.to_owned());
        }
    }
    clues.tasks.extend(task_numbers(text));
}

/// The maximal runs of ASCII characters a path, an identifier or an ADR
/// reference is made of, with their byte offsets.
fn ascii_words(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let is_word = |c: char| c.is_ascii_alphanumeric() || "_-./*".contains(c);
    let mut words = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        match (is_word(c), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                words.push((s, &text[s..i]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        words.push((s, &text[s..]));
    }
    words.into_iter()
}

/// A repository path or file name: a word ending in a known extension,
/// without the `./` before it or the `.` of a sentence after it.
fn file_name(word: &str) -> Option<&str> {
    let word = word.trim_start_matches("./").trim_end_matches('.');
    let (stem, extension) = word.rsplit_once('.')?;
    let name = stem.rsplit('/').next().unwrap_or(stem);
    (FILE_EXTENSIONS.contains(&extension)
        && !name.is_empty()
        && name.bytes().any(|b| b.is_ascii_alphanumeric()))
    .then_some(word)
}

/// `ADR-0046`, `adr-0046` or `docs/adr/0046-...`.
fn adr_number(word: &str) -> Option<u32> {
    let lower = word.to_ascii_lowercase();
    let rest = lower
        .find("adr-")
        .map(|i| &lower[i + 4..])
        .or_else(|| lower.find("adr/").map(|i| &lower[i + 4..]))?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    (digits.len() == 4).then(|| digits.parse().ok()).flatten()
}

/// The shape of a test function's name: lowercase snake_case of at least
/// [`TEST_NAME_WORDS`] words, starting with a letter.
fn is_test_name(word: &str) -> bool {
    word.starts_with(|c: char| c.is_ascii_lowercase())
        && word
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && word.split('_').all(|part| !part.is_empty())
        && word.split('_').count() >= TEST_NAME_WORDS
}

/// Task numbers after `task`, `tasks` or `タスク`, including lists such as
/// `task 203,164` and `task 178 と 179`.
fn task_numbers(text: &str) -> Vec<i64> {
    let lower = text.to_ascii_lowercase();
    let mut numbers = Vec::new();
    for keyword in ["task", "タスク"] {
        let mut from = 0;
        while let Some(found) = lower[from..].find(keyword) {
            let at = from + found;
            from = at + keyword.len();
            let before = lower[..at].chars().next_back();
            if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                continue;
            }
            let mut rest = &lower[from..];
            rest = rest.strip_prefix('s').unwrap_or(rest);
            if !rest.starts_with([' ', '\u{3000}'])
                && !rest.starts_with(|c: char| c.is_ascii_digit())
            {
                continue;
            }
            loop {
                let trimmed = rest.trim_start_matches([' ', '\u{3000}', '#']);
                let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
                let after = &trimmed[digits.len()..];
                // `task 5a` and `task 1.5` are not task numbers; `task 12.`
                // at the end of a sentence is.
                let decimal = after
                    .strip_prefix('.')
                    .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()));
                if digits.is_empty()
                    || decimal
                    || after.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                {
                    break;
                }
                if let Ok(number) = digits.parse() {
                    numbers.push(number);
                }
                let next = after.trim_start_matches([' ', '\u{3000}']);
                let Some(separated) = ["、", ",", "・", "と", "and ", "/"]
                    .iter()
                    .find_map(|sep| next.strip_prefix(sep))
                else {
                    break;
                };
                if !separated
                    .trim_start_matches([' ', '\u{3000}', '#'])
                    .starts_with(|c: char| c.is_ascii_digit())
                {
                    break;
                }
                rest = separated;
            }
        }
    }
    numbers
}

/// Whether two declared globs can match the same path: the same glob, or
/// one matching the other's text (`src/**` and `src/domain/*.rs`).
pub fn globs_overlap(a: &str, b: &str) -> bool {
    a == b || glob_matches(a, b) || glob_matches(b, a)
}

/// What `rank` compares a task with: every task's clues, and how many
/// tasks share each, read once.
struct RelatedIndex<'a> {
    docs: &'a [RelatedDoc],
    clues: Vec<TextClues>,
    files: HashMap<String, usize>,
    tests: HashMap<String, usize>,
    adrs: HashMap<u32, usize>,
    mentions: HashMap<i64, usize>,
    globs: HashMap<String, usize>,
    goals: HashMap<i64, usize>,
}

impl<'a> RelatedIndex<'a> {
    fn new(docs: &'a [RelatedDoc]) -> Self {
        fn count<K: std::hash::Hash + Eq>(map: &mut HashMap<K, usize>, key: K) {
            *map.entry(key).or_default() += 1;
        }
        let mut index = Self {
            docs,
            clues: Vec::with_capacity(docs.len()),
            files: HashMap::new(),
            tests: HashMap::new(),
            adrs: HashMap::new(),
            mentions: HashMap::new(),
            globs: HashMap::new(),
            goals: HashMap::new(),
        };
        for doc in docs {
            let mut clues = TextClues::default();
            for text in &doc.texts {
                add_text_clues(&mut clues, text);
            }
            clues.tasks.remove(&doc.id);
            clues
                .files
                .iter()
                .for_each(|f| count(&mut index.files, f.clone()));
            clues
                .tests
                .iter()
                .for_each(|t| count(&mut index.tests, t.clone()));
            clues.adrs.iter().for_each(|a| count(&mut index.adrs, *a));
            clues
                .tasks
                .iter()
                .for_each(|t| count(&mut index.mentions, *t));
            doc.paths
                .iter()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .for_each(|g| count(&mut index.globs, g.clone()));
            if let Some(goal) = doc.goal_id {
                count(&mut index.goals, goal);
            }
            index.clues.push(clues);
        }
        index
    }
}

/// The tasks related to `target`, best first: every other task scoring
/// above zero, kept to `statuses` (empty: all) and cut to `limit`.
/// `search` holds, per task, how strongly the index matched the target's
/// title against it relative to the target itself (0 to 1).
pub fn rank(
    target: i64,
    docs: &[RelatedDoc],
    search: &HashMap<i64, f64>,
    statuses: &[String],
    limit: usize,
) -> Option<RelatedPage> {
    let index = RelatedIndex::new(docs);
    let t = docs.iter().position(|doc| doc.id == target)?;
    let n = docs.len();
    let mut related: Vec<RelatedTask> = docs
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != t)
        .map(|(i, doc)| {
            let clues = score_pair(&index, t, i, search.get(&doc.id).copied(), n);
            let score = clues.iter().map(|clue| clue.weight).sum::<f64>();
            RelatedTask {
                id: doc.id,
                status: doc.status.clone(),
                title: doc.title.clone(),
                score: round(score),
                clues,
                duplicate_of: doc.duplicate_of,
            }
        })
        .filter(|task| task.score > 0.0)
        .filter(|task| statuses.is_empty() || statuses.contains(&task.status))
        .collect();
    related.sort_by(|a, b| b.score.total_cmp(&a.score).then(b.id.cmp(&a.id)));
    let total = related.len();
    related.truncate(limit);
    Some(RelatedPage {
        task_id: target,
        related,
        total,
    })
}

fn round(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn score_pair(
    index: &RelatedIndex<'_>,
    t: usize,
    o: usize,
    search: Option<f64>,
    n: usize,
) -> Vec<Clue> {
    let (a, b) = (&index.docs[t], &index.docs[o]);
    let (ca, cb) = (&index.clues[t], &index.clues[o]);
    let mut clues = Vec::new();
    let mut push = |clue: ClueKind, value: String, weight: f64| {
        if weight > 0.0 {
            clues.push(Clue {
                clue,
                value,
                weight: round(weight),
            });
        }
    };
    // Paths: each of the task's globs once, at the rarer glob's weight.
    let mut seen = BTreeSet::new();
    for ga in &a.paths {
        let best = b
            .paths
            .iter()
            .filter(|gb| globs_overlap(ga, gb))
            .map(|gb| {
                let df = index.globs[ga].max(index.globs[gb]);
                (rarity(df, n), gb)
            })
            .max_by(|x, y| x.0.total_cmp(&y.0));
        if let Some((weight, gb)) = best
            && seen.insert(ga)
        {
            let value = if ga == gb {
                ga.clone()
            } else {
                format!("{ga} ~ {gb}")
            };
            push(ClueKind::Path, value, PATH_WEIGHT * weight);
        }
    }
    for file in ca.files.intersection(&cb.files) {
        push(
            ClueKind::File,
            file.clone(),
            FILE_WEIGHT * rarity(index.files[file], n),
        );
    }
    for test in ca.tests.intersection(&cb.tests) {
        push(
            ClueKind::Test,
            test.clone(),
            TEST_WEIGHT * rarity(index.tests[test], n),
        );
    }
    for adr in ca.adrs.intersection(&cb.adrs) {
        push(
            ClueKind::Adr,
            format!("{adr:04}"),
            ADR_WEIGHT * rarity(index.adrs[adr], n),
        );
    }
    if ca.tasks.contains(&b.id) {
        push(ClueKind::Mentions, format!("task {}", b.id), MENTION_WEIGHT);
    }
    if cb.tasks.contains(&a.id) {
        push(
            ClueKind::MentionedBy,
            format!("task {}", a.id),
            MENTION_WEIGHT,
        );
    }
    for task in ca.tasks.intersection(&cb.tasks) {
        push(
            ClueKind::SharedMention,
            format!("task {task}"),
            SHARED_MENTION_WEIGHT * rarity(index.mentions[task], n),
        );
    }
    let runs =
        |doc: &RelatedDoc| -> BTreeMap<String, i64> { doc.follow_up_of.iter().cloned().collect() };
    let (ra, rb) = (runs(a), runs(b));
    if let Some((run, _)) = ra.iter().find(|(_, task)| **task == b.id) {
        push(
            ClueKind::FollowUpOf,
            format!("run {run} of task {}", b.id),
            FOLLOW_UP_OF_WEIGHT,
        );
    } else if let Some((run, _)) = rb.iter().find(|(_, task)| **task == a.id) {
        push(
            ClueKind::FollowUpOf,
            format!("run {run} of task {}", a.id),
            FOLLOW_UP_OF_WEIGHT,
        );
    }
    if let Some((run, task)) = ra.iter().find(|(run, _)| rb.contains_key(*run)) {
        push(
            ClueKind::SameRun,
            format!("run {run} of task {task}"),
            SAME_RUN_WEIGHT,
        );
    }
    if let (Some(ga), Some(gb)) = (a.goal_id, b.goal_id)
        && ga == gb
    {
        push(
            ClueKind::Goal,
            format!("goal {ga}"),
            GOAL_WEIGHT * rarity(index.goals[&ga], n),
        );
    }
    if let Some(strength) = search.filter(|s| *s >= SEARCH_FLOOR) {
        let strength = strength.min(1.0);
        push(
            ClueKind::Search,
            format!("{:.2}", strength),
            SEARCH_WEIGHT * strength,
        );
    }
    clues
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set<T: Ord + Clone>(items: &[T]) -> BTreeSet<T> {
        items.iter().cloned().collect()
    }

    #[test]
    fn texts_give_files_tests_adrs_and_tasks() {
        let clues = text_clues(
            "tests/runtime.rs:5228 の a_run_parked_again が落ちた。./src/domain/task.rs と \
             AGENTS.md。`cargo test --locked --test e2e -- --ignored`。ADR-0046（docs/adr/0041-plan.md）\
             と adr-12。task 203,164、153 と 121 と Task #7 の follow_up。subtask 9、task 5a、task 1.5、tasks 8 and 9、see task 11.\
             file.log と .md と resume_prompt_delay.",
        );
        assert_eq!(
            clues.files,
            set(&[
                "AGENTS.md".to_owned(),
                "docs/adr/0041-plan.md".into(),
                "src/domain/task.rs".into(),
                "tests/runtime.rs".into(),
            ])
        );
        assert_eq!(
            clues.tests,
            set(&[
                "a_run_parked_again".to_owned(),
                "e2e".into(),
                "resume_prompt_delay".into(),
            ])
        );
        assert_eq!(clues.adrs, set(&[41, 46]));
        assert_eq!(clues.tasks, set(&[7, 8, 9, 11, 121, 153, 164, 203]));
        assert_eq!(
            text_clues("タスク 12 と task\u{3000}13").tasks,
            set(&[12, 13])
        );
        assert!(text_clues("--test").tests.is_empty());
        assert!(text_clues("--test Foo-bar").tests.is_empty());
    }

    #[test]
    fn a_clue_many_tasks_share_counts_less() {
        assert_eq!(rarity(2, 1), 1.0);
        assert_eq!(rarity(2, 100), 1.0);
        assert!(rarity(10, 100) < rarity(3, 100));
        assert_eq!(rarity(101, 100), 0.0);
        assert!(globs_overlap("src/**", "src/domain/*.rs"));
        assert!(globs_overlap("docs/*.md", "docs/**"));
        assert!(!globs_overlap("docs/**", "*.md"));
    }

    fn doc(id: i64, texts: &[&str]) -> RelatedDoc {
        RelatedDoc {
            id,
            status: "ready".into(),
            title: format!("task {id}"),
            texts: texts.iter().map(|t| (*t).to_owned()).collect(),
            ..RelatedDoc::default()
        }
    }

    #[test]
    fn rank_scores_each_clue_and_orders_best_first() {
        let mut docs = vec![
            RelatedDoc {
                goal_id: Some(1),
                paths: vec!["src/**".into(), "docs/**".into()],
                follow_up_of: vec![("run-1".into(), 9)],
                ..doc(1, &["ADR-0046 と tests/cli.rs と the_same_test_name"])
            },
            RelatedDoc {
                goal_id: Some(1),
                paths: vec!["src/domain/*.rs".into()],
                follow_up_of: vec![("run-1".into(), 9)],
                ..doc(
                    2,
                    &["ADR-0046 と tests/cli.rs と the_same_test_name、task 1"],
                )
            },
            RelatedDoc {
                status: "canceled".into(),
                duplicate_of: Some(1),
                ..doc(3, &["nothing shared"])
            },
            RelatedDoc {
                follow_up_of: vec![("run-2".into(), 1)],
                ..doc(4, &[""])
            },
            doc(9, &["task 1 は docs"]),
        ];
        let search = HashMap::from([(3, 0.5), (4, 0.05), (5, 1.0)]);
        let page = rank(1, &docs, &search, &[], 10).unwrap();
        let ids: Vec<i64> = page.related.iter().map(|t| t.id).collect();
        assert_eq!(ids, [2, 9, 4, 3]);
        let kinds: Vec<ClueKind> = page.related[0].clues.iter().map(|c| c.clue).collect();
        assert_eq!(
            kinds,
            [
                ClueKind::Path,
                ClueKind::File,
                ClueKind::Test,
                ClueKind::Adr,
                ClueKind::MentionedBy,
                ClueKind::SameRun,
                ClueKind::Goal,
            ]
        );
        assert_eq!(page.related[0].clues[0].value, "src/** ~ src/domain/*.rs");
        assert_eq!(page.related[0].clues[4].value, "task 1");
        assert_eq!(page.related[1].clues[0].clue, ClueKind::MentionedBy);
        assert_eq!(page.related[2].clues[0].value, "run run-2 of task 1");
        assert_eq!(page.related[3].clues[0].clue, ClueKind::Search);
        assert_eq!(page.related[3].duplicate_of, Some(1));
        assert_eq!(page.total, 4);
        // From the follow-up's side, and the task it names.
        let from_four = rank(4, &docs, &HashMap::new(), &[], 10).unwrap();
        assert_eq!(from_four.related[0].clues[0].clue, ClueKind::FollowUpOf);
        let from_two = rank(2, &docs, &HashMap::new(), &[], 1).unwrap();
        assert_eq!(from_two.related[0].id, 1);
        assert!(
            from_two.related[0]
                .clues
                .iter()
                .any(|c| c.clue == ClueKind::Mentions)
        );
        docs[0].texts.push("task 9".into());
        docs[1].texts.push("task 9".into());
        let shared = rank(2, &docs, &HashMap::new(), &["ready".into()], 10).unwrap();
        assert!(
            shared.related[0]
                .clues
                .iter()
                .any(|c| c.clue == ClueKind::SharedMention && c.value == "task 9")
        );
        assert!(shared.related.iter().all(|t| t.status == "ready"));
        assert!(
            rank(1, &docs, &HashMap::new(), &["completed".into()], 10)
                .unwrap()
                .related
                .is_empty()
        );
        assert!(rank(42, &docs, &HashMap::new(), &[], 10).is_none());
    }
}
