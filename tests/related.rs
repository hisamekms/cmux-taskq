//! `related` on a queue shaped like this repository's (ADR-0046 decision
//! 4): the duplicates the 2026-09-25 inventory found, summarized from
//! their real titles and descriptions, among tasks that share goals, files,
//! ADRs and paths with them.
use std::collections::HashMap;

use dagq::{
    application::TaskStore,
    domain::{
        GoalId, NewGoal, NewTask, TaskId,
        related::{ClueKind, RelatedPage},
    },
    infrastructure::sqlite::SqliteQueue,
};
use rusqlite::{Connection, params};
use tempfile::TempDir;

const PLUGIN: &[&str] = &["plugins/**", "docs/**", "*.md"];
const DOCS: &[&str] = &["docs/**", "*.md"];

/// A task as it stands in the real queue: its ID there, its goal there,
/// its paths, title, description and context. `{N}` in a text is the
/// fixture's ID of real task N; a bare number is a task outside the
/// fixture, and is kept.
struct Fixture {
    real: i64,
    goal: Option<i64>,
    paths: &'static [&'static str],
    title: &'static str,
    description: &'static str,
    context: &'static str,
}

const fn f(
    real: i64,
    goal: Option<i64>,
    paths: &'static [&'static str],
    title: &'static str,
    description: &'static str,
    context: &'static str,
) -> Fixture {
    Fixture {
        real,
        goal,
        paths,
        title,
        description,
        context,
    }
}

const NONE: &[&str] = &[];

const FIXTURES: &[Fixture] = &[
    f(
        71,
        Some(8),
        NONE,
        "runtime: needs_session の run を supervisor が自動で resume する",
        "src/application/supervise/resume.rs に resume の試行を足し、MAX_RESUME_ATTEMPTS を超えたら ask にする。docs/design/supervisor-lifecycle.md を更新する。",
        "goal 8 の土台。",
    ),
    f(
        164,
        Some(3),
        NONE,
        "test: a_concern_sent_back_is_resumed_reviewed_again_and_landed が負荷下で不安定",
        "tests/runtime.rs の a_concern_sent_back_is_resumed_reviewed_again_and_landed が resume_finished を 2 件期待して 1 件になる。",
        "task 150 の run の receipt が提案した follow_up",
    ),
    f(
        178,
        Some(20),
        NONE,
        "runtime: task が goal の完了を待つ goal 依存を足す",
        "ADR-0031 の goal 依存を実装する。add --depends-on-goal と dependency add --goal、graph の ready_after、goal show の dependents。src/infrastructure/sqlite.rs と migrations を変える。",
        "planner との対話で決めた。",
    ),
    f(
        179,
        Some(20),
        PLUGIN,
        "plugin: ゴールをまたぐ待ちは goal 依存で書くと skill に書く",
        "claude-dagq plugin の skill に task {178} で入った goal 依存を書く。dagq skill（SKILL.md の Register the tasks と reference/inspect.md）に --depends-on-goal と dependency add/remove --goal、show / list / goal show / graph の新しい field、充足条件を足す。dagq-planner skill に使い分けを書く。skill の大きさの上限（tests/plugin.rs）を守る。",
        "task {178} の receipt summary と ADR-0031 の CLI 名・field 名に合わせる。",
    ),
    f(
        185,
        Some(3),
        NONE,
        "application: runtime.rs に残る review・rebind・stats と inbox / planner の prompt を application に移す",
        "src/runtime.rs に残る処理を src/application に移す。tests/runtime.rs は変えずに通す。",
        "goal 3 の整理。",
    ),
    f(
        186,
        Some(3),
        NONE,
        "test: tests/runtime.rs の「run lease is missing or stale」の不安定な失敗をなくす",
        "負荷の下で run lease の失敗が出る。a_run_missing_any_condition_of_the_skip_is_resumed も時間がかかる。",
        "task 164 と同じく負荷の下の不安定さ。",
    ),
    f(
        203,
        Some(3),
        NONE,
        "test: a_run_parked_again_after_a_skip_is_resumed_not_skipped が負荷下で落ちる",
        "task {185} の integrate 1 回目（llvm-cov の全体実行）で、tests/runtime.rs:5228 の assert が errors 2 件を期待して 1 件になり落ちた。単独では通る。タイミングに依存しない待ち方にする。",
        "task {185} の run 77be44f3 の receipt が提案した follow_up",
    ),
    f(
        230,
        Some(3),
        NONE,
        "test: a_run_parked_again_after_a_skip_is_resumed_not_skipped fails under heavy load (errors 1, expected 2)",
        "In task {186}'s stress, at load average about 100, it failed once at tests/runtime.rs:5234: outcome.errors had 1 entry, not 2. a_concern_sent_back_is_resumed_reviewed_again_and_landed also failed once. Related: task {164}.",
        "task {186} の run 6d13ebde の receipt が提案した follow_up",
    ),
    f(
        247,
        None,
        NONE,
        "test: 負荷の下で不安定な runtime / e2e の test をタイミングに依存しない条件待ちにする",
        "(a) tests/runtime.rs の a_run_parked_again_after_a_skip_is_resumed_not_skipped。(b) a_concern_sent_back_is_resumed_reviewed_again_and_landed。(c) tests/e2e.rs の killed_supervisor_run_is_adopted_by_the_next_supervisor_and_lands。(d) e2e happy_path の workspace close の確認。固定の timeout を条件待ちに置き換える。",
        "draft 棚卸し（2026-09-25）で人が登録を決めた、follow_up の draft（task {203},{164},153,121）の置き換え。",
    ),
    f(
        268,
        Some(20),
        PLUGIN,
        "plugin: dagq skill に goal 依存を書く",
        "task {178} の follow_up。reference/inspect.md に goal_dependencies を書く。",
        "task {178} の run の receipt が提案した follow_up",
    ),
    f(
        273,
        Some(29),
        DOCS,
        "docs: ADR-0041 でオンデマンドの planner と proposal と plan review の job を決める",
        "docs/adr/0041-on-demand-planners-proposals-submitted-and-plan-review-job.md を書く。",
        "planner との対話で決めた。",
    ),
    f(
        274,
        Some(29),
        NONE,
        "runtime: draft と submitted の task を編集できるようにする（dagq edit）",
        "ADR-0041 の決定 8。src/infrastructure/sqlite.rs に edit を足す。tests/cli.rs に test を足す。",
        "goal 29 の constraints にある。",
    ),
    f(
        275,
        Some(29),
        NONE,
        "runtime: task に submitted の状態と proposal を足し、submit と bypass を実装する",
        "ADR-0041 の決定 8 と 9。migrations に proposals の表を足す。ready --bypass-review。",
        "goal 29 の constraints にある。",
    ),
    f(
        278,
        Some(29),
        NONE,
        "runtime: dagq plan でオンデマンドの planner を開く",
        "ADR-0041 の決定 6。up は planner を開かない。",
        "goal 29 の constraints にある。",
    ),
    f(
        280,
        Some(29),
        PLUGIN,
        "plugin: planner・inbox・recover・dagq の skill と AGENTS.md を、オンデマンドの planner と plan review の流れに直す",
        "plugins/claude-dagq の skill を新しい流れに直す。dagq-planner: dagq plan で開かれること、proposal を作って submit すること、revise を受けたら直して submit し直すこと。dagq-inbox: plan review の concern を人に見せること。dagq skill: submit、edit、lint、submitted の状態。AGENTS.md の役割の節と起動と停止の節を直す。skill の大きさの上限（tests/plugin.rs）を守る。",
        "goal 29 の constraints にある。task {278} と 279 が実装した CLI の形に合わせる。",
    ),
    f(
        281,
        Some(29),
        NONE,
        "runtime: plan review の job が submitted の proposal を見て ready にする",
        "ADR-0041 の決定 10 と 11。headless の job。",
        "goal 29 の constraints にある。",
    ),
    f(
        285,
        Some(8),
        NONE,
        "runtime: session への送信（resume の解消依頼・/exit・answer・revise）が届いたかを画面で確かめ、Enter の取りこぼしを直す",
        "src/application/supervise/resume.rs の poll は resume_prompt_delay 待つだけで send_text し、src/infrastructure/adapters.rs の Cmux::send_text は長い文面の直後に Enter を送る。実例 1（task 221）: 依頼は入力欄ができる前に届いて消え stuck_exit になった。実例 2（task 205）: 文面が入力欄に貼られたまま Enter が送信として扱われなかった。送信後に文面が残っていれば Enter だけ送り直し、なお動かなければ ask にする。",
        "inbox 経由の人の指示。task {71} が土台。",
    ),
    f(
        287,
        Some(30),
        DOCS,
        "docs: ADR-0043 で、止まった worker の session の検知と促しと ask を決める",
        "docs/adr/0043-detect-stalled-sessions.md を書く。",
        "planner との対話で決めた。",
    ),
    f(
        288,
        Some(30),
        NONE,
        "runtime: receipt の無い idle を検知し、一度促して、解消しなければ stalled の ask を出す",
        "ADR-0043（task {287}）の決定 1 を実装する。",
        "goal 30 の constraints にある。",
    ),
    f(
        289,
        Some(30),
        NONE,
        "runtime: supervisor が送った文が処理されたかを確かめ、されなければ Enter を再送し、なお動かなければ ask にする",
        "ADR-0043（task {287}）を実装する。task 205 では、差し戻しの文が入力欄に貼られたまま送信されなかった。supervisor が session に送る文ごとに、session が動き出した印があるかを確かめる。無ければ Enter を一度だけ送り直し、なお動かなければ inbox の ask にする。",
        "goal 30 の constraints にある。task {288} と同じファイルを触る。",
    ),
    f(
        297,
        Some(30),
        NONE,
        "runtime: 検知ごとの結果と検知までの時間を記録し、stats が閾値ごとの妥当性を返す",
        "ADR-0043（task {287}）の検知（task {288}、{289}）の結果を記録する。",
        "goal 30 の constraints にある。",
    ),
    f(
        302,
        Some(20),
        PLUGIN,
        "plugin: dagq skill に goal 依存の使い方と使い分けを書く",
        "plugins/claude-dagq/skills/dagq/SKILL.md（dependency add|remove、candidates の条件）と reference/inspect.md（graph の ready_after、goal_dependencies、goal show の dependents）を ADR-0038 に合わせる。goal をまたぐ待ちは --depends-on-goal / dependency add --goal で書くのが標準であること、abandoned で閉じた goal は依存を解かないことを書く。",
        "draft 整理で人が登録を決めた、follow_up の draft（task {268}）の置き換え。",
    ),
    f(
        306,
        Some(32),
        DOCS,
        "docs: ADR-0045 で ADR-0014 を丸ごと置き換え、build 識別子・明示の migrate・自動更新を決める",
        "docs/adr/0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md を書く。",
        "planner との対話で決めた。",
    ),
    f(
        307,
        Some(32),
        NONE,
        "runtime: build 識別子を埋め込み、up の入れ替えの判定に使う",
        "ADR-0045 の決定 1 と 2。src/build_id.rs を足す。AGENTS.md は触らない。",
        "goal 32 の constraints にある。",
    ),
    f(
        309,
        Some(32),
        NONE,
        "runtime: open で migrate せず、dagq migrate で明示に上げる",
        "ADR-0045 の決定 5 と 6。src/infrastructure/schema.rs。",
        "goal 32 の constraints にある。",
    ),
    f(
        310,
        Some(32),
        NONE,
        "runtime: dagq install と up --auto-update",
        "ADR-0045 の決定 8。",
        "goal 32 の constraints にある。",
    ),
    f(
        311,
        Some(32),
        PLUGIN,
        "plugin: AGENTS.md・dagq-recover の up-down・release skill を、build 識別子・dagq install・自動更新・明示の migrate に合わせる",
        "AGENTS.md の『作業中』と『起動と停止』の節（固定バイナリの更新の手順、version を上げずに build し直したときの down --wait）を、ADR-0045 と実装に合わせて直す。up の起動例に --auto-update を足す。plugins/claude-dagq/skills/dagq-recover の reference/up-down.md の手順を dagq install と自動更新に直す。",
        "goal 32 の constraints にある。",
    ),
    f(
        313,
        Some(29),
        NONE,
        "runtime: a_run_parked_again_after_a_skip_is_resumed_not_skipped が全体実行で不安定",
        "cargo test --locked の全体実行で tests/runtime.rs:5339 が 1 回失敗した（'no resume script for task 2'）。単独実行では通る。llvm-cov の関門を誤らせうる。",
        "task {274}（dagq edit）の run 3a8812b5 の receipt が提案した follow_up",
    ),
    f(
        316,
        Some(32),
        NONE,
        "AGENTS.md・dagq-recover skill・release skill・design 文書を ADR-0045 の手順に書き直す",
        "ADR-0045 の実装が入った後、固定バイナリ更新の手順（dagq install、up --auto-update、dagq migrate）と release skill の段を書き直す。兄弟 task が既にあれば不要。",
        "task {306} の run 95bdd37d の receipt が提案した follow_up",
    ),
    f(
        317,
        None,
        NONE,
        "test: runtime の test fixture が起動するスタブの agent shell を test の終了時に必ず止める",
        "tests/runtime.rs の TestProvider が /bin/sh -c \"$AGENT_PRELUDE ...\" を起動する。スクリプトが孤児として残り、cargo test のパイプを握り続ける。process group ごと kill する。同じ問題を持つ fixture が tests/lifecycle.rs・cli.rs にもあれば直す。",
        "実例: task 182（run 81514af7、約 10.5 時間停止）、task {285}（run 79ba226d）。",
    ),
    f(
        320,
        Some(32),
        NONE,
        "AGENTS.md の CARGO_PKG_VERSION による up の reuse の注意を、build 識別子の固定バイナリに入れ替えた後に更新する",
        "AGENTS.md の「作業中」は version が CARGO_PKG_VERSION なので version を上げずに build し直すと up が reuse すると書いている。入れ替えた後は ADR-0045 の残りの実装と合わせて AGENTS.md を更新する。",
        "task {307} の run 99d02cb6 の receipt が提案した follow_up",
    ),
    f(
        323,
        Some(29),
        NONE,
        "plugin skill と AGENTS.md を submit / plan review の流れに書き換える",
        "この run では dagq skill を最小限だけ直した（ready --bypass-review、status に submitted）。dagq-planner などの skill と AGENTS.md に、submit・proposal・plan review の流れを書く（ADR-0041 決定 6 の skill task）。",
        "task {275} の run 4b46e0fb の receipt が提案した follow_up",
    ),
    f(
        324,
        None,
        NONE,
        "test: 317 の着地後に、テストに上限の無い待ちが残っていないかを確かめる",
        "task 182 の run 81514af7 で、cargo test --locked 2>&1 | grep が約 10.5 時間戻らなかった。task {317} が直している孤児がこの原因の可能性が高い。tests/runtime.rs、cli.rs、lifecycle.rs、e2e.rs の上限の無い待ちを洗い出す。a_run_missing_any_condition_of_the_skip_is_resumed も調べる。task {247} と調整する。",
        "draft 棚卸しの後の調査で見つかった。",
    ),
    f(
        346,
        Some(32),
        NONE,
        "runtime: up が別の build の supervisor を待たずに引き継ぐ",
        "ADR-0045 の決定 3 と 4。AGENTS.md の手順は task {311} が直す。",
        "goal 32 の constraints にある。",
    ),
    // Tasks that share the common clues and are no one's duplicate.
    f(
        350,
        Some(31),
        PLUGIN,
        "plugin: dagq-inbox skill に finding の ask の見せ方を書く",
        "plugins/claude-dagq/skills/dagq-inbox/SKILL.md に finding を足す。AGENTS.md の inbox の節も直す。tests/plugin.rs の上限を守る。",
        "goal 31 の constraints にある。",
    ),
    f(
        351,
        Some(31),
        NONE,
        "runtime: review の finding から planner を起こす",
        "src/application/review.rs。tests/runtime.rs に test を足す。AGENTS.md は触らない。",
        "goal 31 の constraints にある。",
    ),
    f(
        352,
        None,
        NONE,
        "runtime: stats に重複の件数を出す",
        "src/domain/stats.rs と tests/cli.rs。ADR-0040 の決定 5 と同じく run_events から導く。",
        "goal 33。",
    ),
    f(
        353,
        None,
        DOCS,
        "docs: design の frontmatter を直す",
        "docs/frontmatter.md に合わせ、AGENTS.md の文書のルールを直す。",
        "人の指示。",
    ),
    f(
        354,
        Some(8),
        NONE,
        "runtime: prompt_waiting のダイアログを検知して ask にする",
        "src/infrastructure/claude.rs の detect_prompt。src/application/supervise/session.rs。tests/runtime.rs。",
        "goal 8。",
    ),
    f(
        355,
        Some(3),
        NONE,
        "application: supervise を小さなモジュールに分ける",
        "src/application/supervise/mod.rs を分ける。tests/runtime.rs は変えずに通す。",
        "goal 3 の整理。",
    ),
];

/// The duplicates of 2026-09-25 (ADR-0046 decision 4), the goals 16/17
/// aside: 316, 320 and 311 are one group, counted as its three pairs.
const PAIRS: &[(i64, i64)] = &[
    (179, 302),
    (203, 230),
    (313, 247),
    (316, 320),
    (316, 311),
    (320, 311),
    (323, 280),
    (289, 285),
    (324, 317),
];

fn text(template: &str, ids: &HashMap<i64, i64>) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let close = open + rest[open..].find('}').unwrap();
        out.push_str(&rest[..open]);
        let real: i64 = rest[open + 1..close].parse().unwrap();
        out.push_str(&ids[&real].to_string());
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

fn build(dir: &TempDir) -> (SqliteQueue, HashMap<i64, i64>, std::path::PathBuf) {
    let db = dir.path().join("queue.db");
    let mut queue = SqliteQueue::init(&db).unwrap();
    let ids: HashMap<i64, i64> = FIXTURES
        .iter()
        .enumerate()
        .map(|(i, fixture)| (fixture.real, i as i64 + 1))
        .collect();
    let mut goals: HashMap<i64, GoalId> = HashMap::new();
    for fixture in FIXTURES {
        let goal_id = fixture.goal.map(|real| {
            *goals.entry(real).or_insert_with(|| {
                queue
                    .add_goal(NewGoal {
                        title: format!("goal {real}"),
                        description: String::new(),
                        acceptance: "done".into(),
                        constraints: String::new(),
                        doc: None,
                        draft: false,
                    })
                    .unwrap()
                    .id()
            })
        });
        let task = queue
            .add(NewTask {
                title: fixture.title.into(),
                description: text(fixture.description, &ids),
                acceptance: String::new(),
                verification_commands: Vec::new(),
                dependencies: Vec::new(),
                goal_dependencies: Vec::new(),
                goal_id,
                context: text(fixture.context, &ids),
                required_evidence: Vec::new(),
                paths: fixture.paths.iter().map(|p| (*p).to_owned()).collect(),
                priority: Default::default(),
            })
            .unwrap();
        assert_eq!(task.id().as_i64(), ids[&fixture.real]);
    }
    (queue, ids, db)
}

fn top(queue: &SqliteQueue, id: i64, limit: usize) -> RelatedPage {
    queue.related(id, &[], limit).unwrap()
}

#[test]
fn at_least_half_of_the_known_duplicates_are_in_each_others_top_five() {
    let dir = tempfile::tempdir().unwrap();
    let (queue, ids, _) = build(&dir);
    let real: HashMap<i64, i64> = ids.iter().map(|(r, f)| (*f, *r)).collect();
    let top_five = |id: i64| -> Vec<i64> {
        top(&queue, ids[&id], 5)
            .related
            .iter()
            .map(|task| real[&task.id])
            .collect()
    };
    let mut found = Vec::new();
    let mut missed = Vec::new();
    for &(a, b) in PAIRS {
        let (of_a, of_b) = (top_five(a), top_five(b));
        if of_a.contains(&b) || of_b.contains(&a) {
            found.push((a, b));
        } else {
            missed.push((a, b, of_a, of_b));
        }
    }
    assert!(
        found.len() * 2 >= PAIRS.len(),
        "found {found:?}; missed (pair, top 5 of each) {missed:?}"
    );
    // On this fixture every pair is found; a change of weights that loses
    // one should be looked at, even while half still pass.
    assert_eq!(missed.len(), 0, "missed {missed:?}");
}

#[test]
fn related_explains_each_candidate_and_filters_by_status() {
    let dir = tempfile::tempdir().unwrap();
    let (mut queue, ids, db) = build(&dir);
    // 203 and 230 share a test name, a file, goal 3 and their titles.
    let page = top(&queue, ids[&203], 3);
    let twin = page
        .related
        .iter()
        .find(|task| task.id == ids[&230])
        .unwrap();
    let kinds: Vec<ClueKind> = twin.clues.iter().map(|clue| clue.clue).collect();
    for kind in [
        ClueKind::Test,
        ClueKind::File,
        ClueKind::Goal,
        ClueKind::Search,
    ] {
        assert!(kinds.contains(&kind), "{kind:?} in {:?}", twin.clues);
    }
    assert!(twin.clues.iter().any(|clue| clue.clue == ClueKind::Test
        && clue.value == "a_run_parked_again_after_a_skip_is_resumed_not_skipped"));
    let sum: f64 = twin.clues.iter().map(|clue| clue.weight).sum();
    assert!((twin.score - sum).abs() < 0.01, "{twin:?}");
    assert_eq!(page.related.len(), 3);
    assert!(page.total > 3);
    // 324 names 317; 317 is named by 324.
    let named = top(&queue, ids[&317], 10);
    let by = named.related.iter().find(|t| t.id == ids[&324]).unwrap();
    assert!(
        by.clues
            .iter()
            .any(|clue| clue.clue == ClueKind::MentionedBy)
    );
    // Same paths: the plugin tasks of goal 20.
    let plugin = top(&queue, ids[&179], 10);
    let other = plugin.related.iter().find(|t| t.id == ids[&302]).unwrap();
    assert!(
        other
            .clues
            .iter()
            .any(|clue| clue.clue == ClueKind::Path && clue.value == "plugins/**"),
        "{other:?}"
    );

    // A canceled duplicate names its original; a status filter keeps it.
    queue
        .cancel_duplicate(TaskId::new(ids[&230]), TaskId::new(ids[&203]))
        .unwrap();
    let raw = Connection::open(&db).unwrap();
    let canceled = queue
        .related(ids[&203], &["canceled".to_owned()], 10)
        .unwrap();
    assert_eq!(canceled.related.len(), 1, "{canceled:?}");
    assert_eq!(canceled.related[0].id, ids[&230]);
    assert_eq!(canceled.related[0].duplicate_of, Some(ids[&203]));
    assert!(
        queue
            .related(ids[&203], &["in_progress".to_owned()], 10)
            .unwrap()
            .related
            .is_empty()
    );

    // Follow-ups of one run, and a landed commit's message.
    raw.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    for registered in [ids[&352], ids[&353]] {
        raw.execute(
            "INSERT INTO run_events (task_id, run_id, kind, payload)
             VALUES (?1, 'run-a', 'follow_up_registered', json_object('task_id', ?2))",
            params![ids[&355], registered],
        )
        .unwrap();
    }
    raw.execute(
        "INSERT INTO landed_commits (run_id, task_id, commit_sha, message, landed_at)
         VALUES ('run-b', ?1, 'abc', 'runtime: touches migrations/0099_unique_name.sql', 'now')",
        params![ids[&354]],
    )
    .unwrap();
    raw.execute(
        "INSERT INTO landed_commits (run_id, task_id, commit_sha, message, landed_at)
         VALUES ('run-c', ?1, 'def', 'runtime: also migrations/0099_unique_name.sql', 'now')",
        params![ids[&310]],
    )
    .unwrap();
    let follow = top(&queue, ids[&352], 20);
    let sibling = follow.related.iter().find(|t| t.id == ids[&353]).unwrap();
    assert!(sibling.clues.iter().any(|c| c.clue == ClueKind::SameRun));
    let source = follow.related.iter().find(|t| t.id == ids[&355]).unwrap();
    assert!(source.clues.iter().any(|c| c.clue == ClueKind::FollowUpOf));
    let landed = top(&queue, ids[&354], 20);
    let commit = landed.related.iter().find(|t| t.id == ids[&310]).unwrap();
    assert!(
        commit
            .clues
            .iter()
            .any(|c| c.clue == ClueKind::File && c.value == "migrations/0099_unique_name.sql"),
        "{commit:?}"
    );

    assert!(queue.related(9999, &[], 5).is_err());
    assert!(queue.related(ids[&203], &[], 0).is_err());
}
