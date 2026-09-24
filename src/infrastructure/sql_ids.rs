//! SQLite conversions of the domain's ID and commit newtypes. They bind as
//! the bare value; reading one back goes through the same check as creating
//! it, so a malformed stored run ID or commit fails the row as a conversion
//! error instead of producing an unchecked value.

use rusqlite::{
    ToSql,
    types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef},
};

use crate::domain::{AskId, CommitSha, EventId, GoalId, RunId, TaskId};

impl ToSql for TaskId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_i64()))
    }
}

impl FromSql for TaskId {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        i64::column_result(value).map(TaskId::new)
    }
}

impl ToSql for GoalId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_i64()))
    }
}

impl FromSql for GoalId {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        i64::column_result(value).map(GoalId::new)
    }
}

impl ToSql for AskId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_i64()))
    }
}

impl FromSql for AskId {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        i64::column_result(value).map(AskId::new)
    }
}

impl ToSql for EventId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_i64()))
    }
}

impl FromSql for EventId {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        i64::column_result(value).map(EventId::new)
    }
}

impl ToSql for RunId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for RunId {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        RunId::new(value.as_str()?).map_err(|error| FromSqlError::Other(Box::new(error)))
    }
}

impl ToSql for CommitSha {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for CommitSha {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        CommitSha::try_from(value.as_str()?).map_err(|error| FromSqlError::Other(Box::new(error)))
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;

    const SHA1: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn ids_round_trip_through_sqlite_and_are_checked_on_the_way_back() {
        let conn = Connection::open_in_memory().unwrap();
        let (task, goal, run, commit): (TaskId, GoalId, RunId, CommitSha) = conn
            .query_row(
                "SELECT ?1, ?2, ?3, ?4",
                (
                    TaskId::new(4),
                    GoalId::new(2),
                    RunId::new("r").unwrap(),
                    CommitSha::try_from(SHA1).unwrap(),
                ),
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(task, TaskId::new(4));
        assert_eq!(goal, GoalId::new(2));
        assert_eq!(run.as_str(), "r");
        assert_eq!(commit.as_str(), SHA1);

        let (ask, event): (AskId, EventId) = conn
            .query_row("SELECT ?1, ?2", (AskId::new(3), EventId::new(8)), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(ask, AskId::new(3));
        assert_eq!(event, EventId::new(8));

        let blank = conn.query_row("SELECT ''", [], |row| row.get::<_, RunId>(0));
        assert!(blank.is_err());
        let short = conn.query_row("SELECT 'abc'", [], |row| row.get::<_, CommitSha>(0));
        assert!(short.is_err());
    }
}
