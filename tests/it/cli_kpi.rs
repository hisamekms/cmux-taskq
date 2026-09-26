//! `dagq kpi` (ADR-0051 decisions 1–9 and 14–19) on a real queue: the
//! periods, the kinds, the host's `[kpi]` settings and targets, and a
//! comparison across a mark.
use std::{path::Path, process::Command};

use dagq::{
    domain::{ClaimOutcome, CommitSha},
    infrastructure::sqlite::SqliteQueue,
};
use serde_json::{Value, json};

use crate::common::{Bounded, cli::*};

/// `dagq kpi` in UTC, with the host-wide `host.toml` read from `config`
/// rather than the home of the person running the tests.
fn kpi(role: Option<&str>, db: &Path, config: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
    command
        .env("TZ", "UTC")
        .env("XDG_CONFIG_HOME", config)
        .env_remove("DAGQ_ROLE");
    if let Some(role) = role {
        command.env("DAGQ_ROLE", role);
    }
    command
        .arg("--db")
        .arg(db)
        .arg("kpi")
        .args(args)
        .bounded_output()
        .unwrap()
}

fn kpi_ok(db: &Path, config: &Path, args: &[&str]) -> Value {
    let output = kpi(None, db, config, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// A runtime task and a task without a kind land today; the host's
/// `host.toml` sets `min_samples` and a target; a mark splits a comparison.
#[test]
fn kpi_reads_the_queue_the_host_settings_and_the_marks() {
    let (dir, db) = queue();
    let config = dir.path().join("config");
    std::fs::create_dir_all(config.join("dagq")).unwrap();
    std::fs::write(
        config.join("dagq/host.toml"),
        "[push]\ncommand = [\"true\"]\n[kpi]\nmin_samples = 1\nbreach_periods = 2\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("host.toml"),
        "[kpi.targets.landings]\nmin = 5\n",
    )
    .unwrap();
    ok(&db, &["add", "first", "--kind", "runtime"]);
    ok(&db, &["add", "second"]);
    ok(&db, &["ready", "1", "--bypass-review"]);
    ok(&db, &["ready", "2", "--bypass-review"]);
    let mark = ok(&db, &["mark", "sccache"])["id"].as_i64().unwrap();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let base = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
    for _ in 0..2 {
        let ClaimOutcome::Claimed { run } = queue.claim_for_supervisor(&base, "t").unwrap() else {
            panic!("nothing to claim");
        };
        for (kind, payload) in [
            ("receipt_observed", json!({})),
            (
                "validation_finished",
                json!({"status": "awaiting_integration"}),
            ),
            ("run_integrated", json!({"status": "integrated"})),
        ] {
            queue.record_runtime_event(run.id(), kind, payload).unwrap();
        }
    }

    let report = kpi_ok(&db, &config, &["--last", "2", "--by", "parallel"]);
    assert_eq!(report["period"], "day");
    assert_eq!(report["utc_offset_secs"], 0);
    assert_eq!(report["config"]["min_samples"], 1);
    assert_eq!(report["config"]["sources"]["min_samples"], "host");
    assert_eq!(report["config"]["sources"]["breach_weeks"], "default");
    let periods = report["periods"].as_array().unwrap();
    assert_eq!(periods.len(), 2);
    let today = &periods[1];
    assert_eq!(today["partial"], true);
    assert_eq!(today["runs"], 2);
    let landings = &today["kpis"]["landings"];
    assert_eq!(landings["all"], json!({"n": 2, "value": 2.0}));
    assert_eq!(landings["kind=runtime"]["value"], 1.0);
    assert_eq!(landings["kind=unknown"]["value"], 1.0);
    // Claimed by hand: no parallel recorded.
    assert_eq!(landings["parallel=unknown"]["value"], 2.0);
    assert_eq!(today["kpis"]["phase.work"]["all"]["n"], 2);
    assert!(today["kpis"]["lead_time"]["all"]["median"].is_number());
    assert_eq!(
        today["unavailable"]["improvement_proposals"],
        "not_recorded"
    );
    assert_eq!(today["marks"][0]["label"], "sccache");
    assert_eq!(today["comparison"]["landings"]["all"]["previous"], 0.0);
    assert_eq!(periods[0]["partial"], false);
    // The queue's host.toml adds the target; yesterday, without a landing,
    // is judged off it (min_samples 1), today is not judged yet.
    let target = &report["targets"][0];
    assert_eq!(target["kpi"], "landings");
    assert_eq!(target["stratum"], "all");
    assert_eq!(target["source"], "host");
    assert_eq!(target["state"], "breach");
    assert_eq!(target["periods"][1]["reason"], "partial");

    let week = kpi_ok(
        &db,
        &config,
        &["--period", "week", "--last", "1", "--kind", "docs"],
    );
    assert_eq!(week["period"], "week");
    let strata = week["periods"][0]["kpis"]["landings"].as_object().unwrap();
    assert!(strata.contains_key("all") && !strata.contains_key("kind=runtime"));
    assert!(week["periods"][0]["label"].as_str().unwrap().contains("-W"));

    let compared = kpi_ok(
        &db,
        &config,
        &["--last", "1", "--compare", &mark.to_string()],
    );
    let compare = &compared["compare"];
    assert_eq!(compare["split"]["marks"][0]["label"], "sccache");
    assert_eq!(compare["split"]["separable"], true);
    assert_eq!(compare["after"]["runs"], 2);
    assert_eq!(compare["before"]["runs"], 0);
    assert_eq!(compare["strata"]["landings"]["all"]["after"]["value"], 2.0);
    assert_eq!(compare["summary"]["runtime"]["phase.work"]["after"]["n"], 1);

    let window = kpi_ok(
        &db,
        &config,
        &["--since", "@1", "--until", &mark.to_string()],
    );
    assert_eq!(window["period"], "window");
    assert_eq!(
        window["periods"][0]["kpis"]["landings"]["all"]["value"],
        0.0
    );

    // The observer and the jobs read it.
    for role in ["observer", "reviewer"] {
        assert!(
            kpi(Some(role), &db, &config, &["--last", "1"])
                .status
                .success()
        );
    }
    for args in [
        &["--period", "month"][..],
        &["--compare", "x"],
        &["--last", "0"],
        &["--at", "1", "--since", "1"],
    ] {
        assert!(!kpi(None, &db, &config, args).status.success(), "{args:?}");
    }
    std::fs::write(dir.path().join("host.toml"), "[kpi]\nmin_samples = 0\n").unwrap();
    let output = kpi(None, &db, &config, &[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("host.toml:2"));
}
