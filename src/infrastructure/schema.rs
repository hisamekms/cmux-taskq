//! The queue's migrations and what each one promises binaries that predate
//! it (ADR-0045 decisions 5–7).
//!
//! Every migration file starts with a declaration line, `-- dagq-schema:
//! compatible` or `-- dagq-schema: breaking`. A compatible migration only
//! adds what an older binary can ignore (a table, a nullable or defaulted
//! column, an index), so a binary that stops before it still reads and
//! writes the migrated queue. Anything else is breaking, and the queue then
//! refuses binaries older than it through its floor.

/// The line every migration file starts with.
const DECLARATION: &str = "-- dagq-schema:";

/// The queue's migrations; the migration at index `i` brings the queue to
/// schema version `i + 1`. `build.rs` lists `migrations/NNNN_<name>.sql` in
/// order of their number (ADR-0067 decision 1), so adding a migration is
/// adding its file.
pub const MIGRATIONS: &[&str] = include!(concat!(env!("OUT_DIR"), "/migrations.rs"));

/// The schema version this binary knows: a fully migrated queue's
/// `user_version`.
pub const BINARY_SCHEMA: i64 = MIGRATIONS.len() as i64;

/// The first schema version whose queue records its floor. A queue below
/// it has no floor table, and its floor is its own version.
pub const FLOOR_SCHEMA: i64 = 24;

/// Whether a migration declares itself compatible. A missing or unknown
/// declaration counts as breaking, the safe reading.
pub fn is_compatible(migration: &str) -> bool {
    declaration(migration) == Some("compatible")
}

/// The declared word of a migration, if its first line is a declaration.
pub fn declaration(migration: &str) -> Option<&str> {
    migration
        .lines()
        .next()?
        .strip_prefix(DECLARATION)
        .map(str::trim)
        .filter(|word| matches!(*word, "compatible" | "breaking"))
}

/// The floor a queue at schema `version` gets from this binary's
/// migrations: the version of the last breaking migration at or below it.
pub fn floor_for(version: i64) -> i64 {
    MIGRATIONS
        .iter()
        .enumerate()
        .take(usize::try_from(version).unwrap_or(0))
        .filter(|(_, migration)| !is_compatible(migration))
        .map(|(index, _)| index as i64 + 1)
        .next_back()
        .unwrap_or(0)
}

/// Why `migration` may not be declared compatible: every statement that
/// could break a binary unaware of it. Empty means the declaration holds.
/// Allowed: `CREATE TABLE`, `CREATE VIRTUAL TABLE`, a non-unique `CREATE
/// INDEX`, `ALTER TABLE ... ADD COLUMN` whose column is nullable or has a
/// default, `INSERT` into a table the same migration creates, and an
/// `AFTER` trigger whose body only inserts into, updates or deletes from
/// tables the same migration creates (an older binary's write then only
/// adds to what it does not read), none of them with a foreign key, a
/// block comment or `RAISE`. A statement with a CHECK that names a kind
/// ([`kind_enumerations`]) is a violation too, even in a table the
/// migration creates.
pub fn compatibility_violations(migration: &str) -> Vec<String> {
    let text = without_comments(migration);
    let mut created = Vec::new();
    let mut violations = Vec::new();
    for statement in statements(&text) {
        let words: Vec<String> = statement
            .split_whitespace()
            .map(str::to_ascii_uppercase)
            .collect();
        if words.is_empty() {
            continue;
        }
        let head: Vec<&str> = words.iter().map(String::as_str).collect();
        let ok = match head.as_slice() {
            ["CREATE", "TABLE", "IF", "NOT", "EXISTS", name, ..]
            | ["CREATE", "TABLE", name, ..]
            | ["CREATE", "VIRTUAL", "TABLE", name, ..] => {
                created.push(table_name(name));
                true
            }
            ["CREATE", "TRIGGER", ..] => trigger_writes_only(&head, &created),
            ["CREATE", "INDEX", ..] => true,
            ["ALTER", "TABLE", _, "ADD", rest @ ..] => {
                let definition = rest.strip_prefix(&["COLUMN"]).unwrap_or(rest).join(" ");
                // An older binary's INSERT leaves the column out.
                !definition.contains("NOT NULL") || definition.contains("DEFAULT")
            }
            ["INSERT", "INTO", name, ..] | ["INSERT", "OR", _, "INTO", name, ..] => {
                created.contains(&table_name(name))
            }
            _ => false,
        };
        // A foreign key would make an older binary's DELETE of the parent
        // row fail once a child row exists; a block comment could hide a
        // word from these checks.
        let ok = ok
            && !words
                .iter()
                .any(|w| w.contains("REFERENCES") || w.contains("/*"))
            && !names_a_kind(&words);
        if !ok {
            violations.push(words.join(" "));
        }
    }
    violations
}

/// The statements of `migration` that enumerate an ask's or event's kind in
/// a CHECK, or bind a rule to a kind's value: a CHECK whose expression
/// names a `kind` column and a string literal (ADR-0073 decision 19). Kinds
/// are checked where they are written instead, so a kind is added without
/// a migration; no migration after the one that dropped those CHECKs may
/// bring one back, whatever it declares. A CHECK on a kind's form
/// (`length(kind) > 0`) names no value and is allowed.
pub fn kind_enumerations(migration: &str) -> Vec<String> {
    statements(&without_comments(migration))
        .iter()
        .map(|statement| {
            statement
                .split_whitespace()
                .map(str::to_ascii_uppercase)
                .collect::<Vec<_>>()
        })
        .filter(|words| names_a_kind(words))
        .map(|words| words.join(" "))
        .collect()
}

/// `migration` without its line comments.
fn without_comments(migration: &str) -> String {
    migration
        .lines()
        .map(|line| line.split("--").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a statement (upper-cased words) has a CHECK whose expression
/// names a `kind` column and a value of it.
fn names_a_kind(words: &[String]) -> bool {
    let text = words.join(" ");
    let mut rest = text.as_str();
    while let Some(at) = rest.find("CHECK") {
        rest = &rest[at + "CHECK".len()..];
        let expression = parenthesized(rest);
        let names_kind = expression
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .any(|word| word == "KIND");
        if names_kind && expression.contains('\'') {
            return true;
        }
    }
    false
}

/// The balanced parenthesized expression `text` starts with (after
/// whitespace), or all of `text` when it does not close.
fn parenthesized(text: &str) -> &str {
    let text = text.trim_start();
    let mut depth = 0usize;
    for (at, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' if depth <= 1 => return &text[..=at],
            ')' => depth -= 1,
            _ if depth == 0 => return "",
            _ => {}
        }
    }
    text
}

/// The statements of `text`, split at `;` except inside a trigger, whose
/// body runs to the `END` after its last statement.
fn statements(text: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut trigger: Option<String> = None;
    for chunk in text.split(';') {
        if let Some(open) = trigger.as_mut() {
            open.push(';');
            open.push_str(chunk);
            if chunk.trim().eq_ignore_ascii_case("END") {
                statements.extend(trigger.take());
            }
            continue;
        }
        let words: Vec<String> = chunk
            .split_whitespace()
            .take(2)
            .map(str::to_ascii_uppercase)
            .collect();
        if words == ["CREATE", "TRIGGER"] {
            trigger = Some(chunk.to_owned());
        } else {
            statements.push(chunk.to_owned());
        }
    }
    // An unterminated trigger is still judged, and fails.
    statements.extend(trigger);
    statements
}

/// Whether a `CREATE TRIGGER` statement (upper-cased words) runs after the
/// write that fires it and only writes tables in `created`.
fn trigger_writes_only(words: &[&str], created: &[String]) -> bool {
    let Some(begin) = words.iter().position(|w| *w == "BEGIN") else {
        return false;
    };
    // RAISE anywhere, the WHEN clause included, would fail the write.
    if !words[..begin].contains(&"AFTER")
        || words.last() != Some(&"END")
        || words.iter().any(|w| w.contains("RAISE"))
    {
        return false;
    }
    let body = words[begin + 1..words.len() - 1].join(" ");
    body.split(';')
        .map(|statement| statement.split_whitespace().collect::<Vec<_>>())
        .filter(|statement| !statement.is_empty())
        .all(|statement| {
            let target = match statement.as_slice() {
                ["INSERT", "INTO", name, ..]
                | ["INSERT", "OR", _, "INTO", name, ..]
                | ["UPDATE", name, ..]
                | ["DELETE", "FROM", name, ..] => table_name(name),
                _ => return false,
            };
            created.contains(&target)
        })
}

/// A table name as a statement spells it, without its column list or quotes.
fn table_name(word: &str) -> String {
    word.split('(')
        .next()
        .unwrap_or(word)
        .trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_migration_declares_itself_and_compatible_ones_only_add() {
        for (index, migration) in MIGRATIONS.iter().enumerate() {
            let version = index + 1;
            assert!(
                declaration(migration).is_some(),
                "migration {version} does not start with `{DECLARATION} compatible|breaking`"
            );
            if is_compatible(migration) {
                assert_eq!(
                    compatibility_violations(migration),
                    Vec::<String>::new(),
                    "migration {version} is declared compatible"
                );
            }
        }
        // Binaries before the floor table reject every newer user_version,
        // so it and everything before it are breaking (ADR-0045 decision 7).
        for migration in &MIGRATIONS[..FLOOR_SCHEMA as usize] {
            assert!(!is_compatible(migration));
        }
        const { assert!(BINARY_SCHEMA >= FLOOR_SCHEMA) };
    }

    #[test]
    fn no_migration_after_the_open_kinds_one_names_a_kind_in_a_check() {
        // The migration that dropped the CHECKs naming a kind (ADR-0073
        // decision 23), found by what it creates so a renumbering on
        // landing does not move it.
        let open = MIGRATIONS
            .iter()
            .position(|migration| migration.contains("CREATE TABLE asks_v39"))
            .expect("the migration that opens the kinds");
        assert!(!is_compatible(MIGRATIONS[open]));
        assert!(
            MIGRATIONS[..open]
                .iter()
                .any(|migration| !kind_enumerations(migration).is_empty()),
            "the kinds were enumerated before it"
        );
        for (index, migration) in MIGRATIONS.iter().enumerate().skip(open) {
            assert_eq!(
                kind_enumerations(migration),
                Vec::<String>::new(),
                "migration {} enumerates a kind",
                index + 1
            );
        }
    }

    #[test]
    fn checks_naming_a_kind_are_found_and_are_never_compatible() {
        let sql = "-- dagq-schema: compatible
            CREATE TABLE a (kind TEXT NOT NULL CHECK (kind IN ('x', 'y')));
            CREATE TABLE b (task_id INTEGER, kind TEXT,
                CHECK (task_id IS NOT NULL OR (kind = 'x' AND 1)));
            CREATE TABLE c (kind TEXT NOT NULL CHECK (length(kind) > 0),
                reason TEXT CHECK (reason IN ('r')), CHECK (length(kind) < 9));
            CREATE TABLE d (kinds TEXT CHECK (kinds IN ('x')));
            CREATE TABLE e (kind TEXT CHECK";
        let found = kind_enumerations(sql);
        assert_eq!(found.len(), 2, "{found:#?}");
        assert!(found[0].starts_with("CREATE TABLE A "));
        assert!(found[1].starts_with("CREATE TABLE B "));
        assert_eq!(compatibility_violations(sql), found);
    }

    #[test]
    fn the_migrations_are_the_files_of_the_directory_in_order_of_their_number() {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(crate::migration_numbers::DIRECTORY);
        let names: Vec<String> = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let ordered = crate::migration_numbers::ordered(&names).unwrap();
        assert_eq!(BINARY_SCHEMA, ordered.len() as i64);
        for (migration, name) in MIGRATIONS.iter().zip(&ordered) {
            assert_eq!(
                *migration,
                std::fs::read_to_string(directory.join(name)).unwrap(),
                "{name}"
            );
        }
    }

    #[test]
    fn declarations_are_read_from_the_first_line_only() {
        assert!(is_compatible(
            "-- dagq-schema: compatible\nCREATE TABLE t(x);"
        ));
        assert!(!is_compatible("-- dagq-schema: breaking\n"));
        assert!(!is_compatible("-- dagq-schema: maybe\n"));
        assert!(!is_compatible("-- note\n-- dagq-schema: compatible\n"));
        assert_eq!(declaration(""), None);
    }

    #[test]
    fn floor_is_the_last_breaking_migration_at_or_below_the_version() {
        assert_eq!(floor_for(0), 0);
        assert_eq!(floor_for(1), 1);
        assert_eq!(floor_for(FLOOR_SCHEMA), FLOOR_SCHEMA);
        // Beyond what this binary knows, the floor stays at its last breaking one.
        assert_eq!(floor_for(BINARY_SCHEMA + 5), floor_for(BINARY_SCHEMA));
    }

    #[test]
    fn additive_statements_are_compatible() {
        let sql = "-- dagq-schema: compatible
            -- A comment; with a semicolon.
            CREATE TABLE IF NOT EXISTS extra (id INTEGER PRIMARY KEY, note TEXT NOT NULL);
            CREATE TABLE \"more\"(id INTEGER);
            CREATE INDEX extra_by_note ON extra(note);
            ALTER TABLE tasks ADD COLUMN hint TEXT;
            ALTER TABLE tasks ADD weight INTEGER NOT NULL DEFAULT 0;
            INSERT INTO extra(note) VALUES ('seed');
            INSERT OR IGNORE INTO more(id) VALUES (1);";
        assert_eq!(compatibility_violations(sql), Vec::<String>::new());
    }

    #[test]
    fn triggers_writing_only_created_tables_are_compatible() {
        let sql = "-- dagq-schema: compatible
            CREATE VIRTUAL TABLE idx USING fts5(body, tokenize = 'trigram');
            CREATE TABLE log (id INTEGER);
            CREATE TRIGGER a AFTER UPDATE OF status ON tasks WHEN old.status IS NOT new.status BEGIN
                DELETE FROM idx WHERE rowid = old.id;
                INSERT INTO idx (rowid, body) VALUES (new.id, new.title);
                UPDATE idx SET body = '' WHERE rowid = 0;
                INSERT OR IGNORE INTO log (id) VALUES (new.id);
            END;";
        assert_eq!(compatibility_violations(sql), Vec::<String>::new());
        let sql = "CREATE TABLE log (id INTEGER);
            CREATE TRIGGER before BEFORE INSERT ON tasks BEGIN INSERT INTO log VALUES (1); END;
            CREATE TRIGGER other AFTER INSERT ON tasks BEGIN UPDATE goals SET title = ''; END;
            CREATE TRIGGER raise AFTER INSERT ON tasks BEGIN
                INSERT INTO log SELECT RAISE(ABORT, 'no'); END;
            CREATE TRIGGER read AFTER INSERT ON tasks BEGIN SELECT 1; END;
            CREATE TRIGGER nested AFTER INSERT ON tasks BEGIN
                INSERT INTO log VALUES ((RAISE(ABORT, 'no'))); END;
            CREATE TRIGGER guarded AFTER INSERT ON tasks WHEN (SELECT RAISE(ABORT, 'no')) BEGIN
                INSERT INTO log VALUES (1); END;";
        let violations = compatibility_violations(sql);
        assert_eq!(violations.len(), 6, "{violations:#?}");
        assert!(violations.iter().all(|v| v.starts_with("CREATE TRIGGER")));
    }

    #[test]
    fn changes_an_older_binary_cannot_ignore_are_violations() {
        let sql = "CREATE UNIQUE INDEX one ON tasks(title);
            ALTER TABLE tasks ADD COLUMN must TEXT NOT NULL;
            ALTER TABLE tasks RENAME COLUMN title TO name;
            ALTER TABLE tasks DROP COLUMN context;
            DROP TABLE goals;
            UPDATE tasks SET status = 'new';
            INSERT INTO tasks(title) VALUES ('x');
            CREATE TABLE child (token TEXT REFERENCES supervisors(token));
            ALTER TABLE tasks ADD COLUMN hint TEXT /* NOT NULL */;
            CREATE TRIGGER t AFTER INSERT ON tasks BEGIN SELECT 1";
        let violations = compatibility_violations(sql);
        assert_eq!(violations.len(), 10, "{violations:#?}");
        assert!(violations[0].starts_with("CREATE UNIQUE INDEX"));
    }
}
