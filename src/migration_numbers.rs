//! How the queue's migration files are named and numbered (ADR-0067).
//!
//! A migration is `migrations/NNNN_<name>.sql`, `NNNN` its schema version in
//! four digits. `build.rs` includes this file to list the migrations it
//! embeds and to refuse numbers that do not run from 0001 without a gap or a
//! repeat; `integrate` uses it to renumber a run's migration whose number
//! `main` took meanwhile. The library compiles it as a module so the rule is
//! tested like any other code.

/// The directory of the migrations, relative to the repository root.
pub const DIRECTORY: &str = "migrations";

/// The number of a migration file named `NNNN_<name>.sql`, or `None` when
/// `file_name` is not named so.
pub fn number(file_name: &str) -> Option<u32> {
    let (digits, rest) = file_name.split_at_checked(4)?;
    let name = rest.strip_prefix('_')?.strip_suffix(".sql")?;
    if name.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// `number` as the four digits a file name starts with.
pub fn digits(number: u32) -> String {
    format!("{number:04}")
}

/// `file_name` (a migration's) with its number replaced by `number`.
pub fn renumbered(file_name: &str, number: u32) -> String {
    format!("{}{}", digits(number), &file_name[4..])
}

/// The migration files among `file_names` (the entries of the migrations
/// directory) in order of their number, or why they cannot be: a `.sql` file
/// not named `NNNN_<name>.sql`, a number more than one file uses, or a
/// number missing between 0001 and the last. Files that are not `.sql` are
/// not migrations and are left out.
pub fn ordered(file_names: &[String]) -> Result<Vec<String>, String> {
    let mut problems = Vec::new();
    let mut numbered: Vec<(u32, &str)> = Vec::new();
    for name in file_names.iter().filter(|name| name.ends_with(".sql")) {
        match number(name) {
            Some(n) => numbered.push((n, name)),
            None => problems.push(format!(
                "{DIRECTORY}/{name} is not named NNNN_<name>.sql (four digits, an underscore, a name)"
            )),
        }
    }
    numbered.sort();
    let mut expected = 1;
    for (index, &(n, _)) in numbered.iter().enumerate() {
        if index > 0 && numbered[index - 1].0 == n {
            continue;
        }
        let sharing: Vec<String> = numbered
            .iter()
            .filter(|(m, _)| *m == n)
            .map(|(_, name)| format!("{DIRECTORY}/{name}"))
            .collect();
        if sharing.len() > 1 {
            problems.push(format!(
                "migration number {} is used by more than one file: {}",
                digits(n),
                sharing.join(", ")
            ));
        }
        if n != expected {
            let missing = if n > expected + 1 {
                format!("{} to {}", digits(expected), digits(n - 1))
            } else if n == expected + 1 {
                digits(expected)
            } else {
                // Only 0000 comes before the first expected number.
                String::new()
            };
            problems.push(if missing.is_empty() {
                format!(
                    "{DIRECTORY}/{} has number {}, but the numbers start at 0001",
                    numbered[index].1,
                    digits(n)
                )
            } else {
                format!(
                    "migration number {missing} is missing before {DIRECTORY}/{} (the numbers run from 0001 without a gap)",
                    numbered[index].1
                )
            });
        }
        expected = expected.max(n + 1);
    }
    if problems.is_empty() {
        Ok(numbered
            .into_iter()
            .map(|(_, name)| name.to_owned())
            .collect())
    } else {
        Err(problems.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn only_four_digits_an_underscore_a_name_and_sql_make_a_migration() {
        assert_eq!(number("0001_queue.sql"), Some(1));
        assert_eq!(number("0032_run_env_program_events.sql"), Some(32));
        for name in [
            "001_queue.sql",
            "00a1_queue.sql",
            "0001queue.sql",
            "0001_.sql",
            "0001_queue.txt",
            "",
            "あいうえ_x.sql",
        ] {
            assert_eq!(number(name), None, "{name}");
        }
        assert_eq!(renumbered("0033_findings.sql", 35), "0035_findings.sql");
        assert_eq!(digits(7), "0007");
    }

    #[test]
    fn numbers_from_one_without_gaps_are_ordered_and_other_files_left_out() {
        assert_eq!(
            ordered(&names(&[
                "0002_b.sql",
                "README.md",
                "0001_a.sql",
                "0003_c.sql"
            ])),
            Ok(names(&["0001_a.sql", "0002_b.sql", "0003_c.sql"]))
        );
        assert_eq!(ordered(&[]), Ok(Vec::new()));
    }

    #[test]
    fn a_repeat_a_gap_a_zero_and_a_misnamed_file_name_the_files() {
        let error = ordered(&names(&[
            "0001_a.sql",
            "0002_b.sql",
            "0002_c.sql",
            "0003_d.sql",
        ]))
        .unwrap_err();
        assert_eq!(
            error,
            "migration number 0002 is used by more than one file: migrations/0002_b.sql, migrations/0002_c.sql"
        );
        let error = ordered(&names(&["0001_a.sql", "0003_c.sql"])).unwrap_err();
        assert_eq!(
            error,
            "migration number 0002 is missing before migrations/0003_c.sql (the numbers run from 0001 without a gap)"
        );
        let error = ordered(&names(&["0001_a.sql", "0005_e.sql"])).unwrap_err();
        assert!(error.contains("0002 to 0004 is missing before migrations/0005_e.sql"));
        let error = ordered(&names(&["0000_z.sql", "0001_a.sql"])).unwrap_err();
        assert_eq!(
            error,
            "migrations/0000_z.sql has number 0000, but the numbers start at 0001"
        );
        let error = ordered(&names(&["0001_a.sql", "2_b.sql"])).unwrap_err();
        assert!(error.contains("migrations/2_b.sql is not named NNNN_<name>.sql"));
        // Every problem is named, not only the first.
        let error = ordered(&names(&["0002_b.sql", "0002_c.sql", "x.sql"])).unwrap_err();
        assert_eq!(error.lines().count(), 3, "{error}");
    }
}
