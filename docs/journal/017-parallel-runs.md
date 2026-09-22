---
id: journal-017
type: journal
title: Run dependency-free tasks in parallel
status: done
created: 2026-09-22
updated: 2026-09-22
plan_step: 6
queue_task: null
depends_on_journal: [16]
verify:
  - cargo fmt --all --check
  - cargo test --locked
  - cargo clippy --locked --all-targets -- -D warnings
related:
  - plan-rust-runtime-mvp
  - design-supervisor-lifecycle
  - design-persistence
---

# 017: Run dependency-free tasks in parallel

## Goal

[plans/current.md](../plans/current.md) ステップ6。1つのqueueで、依存が解けたtaskを上限まで同時に実行する。

- queue全体のactive runを1件に絞る部分UNIQUE indexをやめ、Taskごとの未完了run 1件の制約だけ残す。
- leaseをqueue単位からrun単位（`task_runs`のsupervisor tokenとheartbeat）にする。`doctor`と`recover`はrunごとに判定し、1つのrunだけrecoverしても他のrunは走り続ける。
- `supervise --parallel N`（既定4）を常駐ループにする。候補を上限までclaim → 起動 → 各runのheartbeat・idle・exitを監視 → 検証、を繰り返し、`integrate`で依存が解けたtaskも拾う。1件の状態機械は現行のまま並べる。
- 1 runの異常終了・検証失敗・cleanup失敗が他のrunに波及しない。
- `plugins/claude-taskq/`のskill（実行・復旧）を`supervise --parallel`とrun単位の`doctor`/`recover`に追従させる。
- ADRを追加し、`domain-model.md`・`persistence.md`・`supervisor-lifecycle.md`・READMEを更新する。

完了条件: 依存のないtaskが同時に走り、依存のあるtaskは先行taskの`completed`まで待つこと、1 runの失敗とrecoverが他のrunに影響しないことがテストで確認できる。`tests/e2e.rs`に2件同時のハッピーパスがある。

## Log

### 2026-09-22 claude (worker)

- worker として開始。branch `journal/017-parallel-runs`、worktree `.worktrees/017-parallel-runs`。016 のジャーナル、ADR-0006、design 3件、runtime.rs / runtime_store.rs / sqlite.rs / adapters.rs / main.rs、tests/{runtime,queue,cli,e2e,plugin}.rs、plugin skill を読了
- 決定（lease の置き場）: `supervisor_leases`（queue singleton）をやめ、新テーブル `run_leases(run_id PK → task_runs, token, pid, heartbeat_at)` にする。`task_runs.supervisor_token` は所有者の記録として残し、lease 行は「いま supervisor が面倒を見ている」ことを表す。token は supervisor プロセスに1つで、その supervisor の全 run の lease 行が同じ token を持つ（heartbeat は `UPDATE run_leases ... WHERE token=?` 1文）。列を task_runs に足す案は「解放済み」を null で表すことになり、run の記録と揮発する所有権が混ざるので不採用
- 決定（migration 0005 / schema v5）: `run_leases` 作成、v4 の `supervisor_leases` を実行中 run（`supervisor_token` 一致）へ移してから DROP、`one_executing_run_per_queue` を DROP INDEX。table の作り直しは不要。`one_unfinished_run_per_task` と `one_integrated_run_per_task` は残す
- 決定（claim）: `TaskQueue::claim` から Busy を外し（queue 全体の枠は無い）、`ClaimOutcome::Busy` を削除。supervisor は `claim_for_supervisor(base, token)` で claim・`supervisor_token`・lease 行の作成を1トランザクションで行い、lease のない claimed run を作らない。`plan_run` は同じ token の claimed run だけを受け付ける
- 決定（ループ）: `supervise` は既定で常駐（候補がなければ 2 秒ごとに `candidates` を見て、あれば `refs/heads/main` を読み直して claim）。`--once` で「active run が無く claim できる task も無ければ終了」。`--parallel N`（既定 4）。SIGINT/SIGTERM は1回目で claim を止めて active run の終了を待ち（graceful drain）、2回目で既定動作（即終了、lease は stale になる）。テストは `SuperviseOptions.stop` の AtomicBool で同じ経路を使う
- 決定（1 run の失敗の扱い）: 監視中・検証・close の runtime error は run 単位で `last_error` + `runtime_error` イベントを書き、その run の lease 行を削除して supervisor が手放す（abandon）。常駐 supervisor が生きている間も `recover` が run の process だけで判断できるようにするため。lease を残すと「supervisor pid が生きている」で永久に recover できない。workspace・worktree・run_processes は触らない。wrapper は登録後は lease を使わないので影響なし。登録前なら lease が無いため登録できず Claude は起動しない（孤児 session を作らない方向で安全）
- 決定（provisioning error）: 環境要因（cmux / git 不通）で全候補を順に潰さないよう、provisioning に失敗したら以後の claim を止め、active run を drain してから非0で終了する。失敗した run は上と同じ abandon
- 決定（検証の並列）: 検証コマンドは最長 30 分かかりうるので、`validating` になった run の receipt 検証は run ごとの thread（専用 SQLite 接続）で走らせ、ループは `is_finished` を見る。`finish_validation` / close / lease 解放はループ側で行う。1件の状態機械（plan → worktree → workspace → 監視 → supervision_finished → 検証 → close）は変えず、ループが run ごとの `Slot` を並べる
- 決定（複数 supervisor）: queue 全体の排他は持たない。同じ queue に2つの supervisor を起動しても claim はトランザクションで直列化され、同じ task に二重 run はできない。`status` / `doctor` は lease の pid ごとに supervisor を並べる

### 2026-09-22 claude (worker) 続き

- 実装: migration `0005_run_leases.sql`（`run_leases` 作成、v4 lease の移行、`supervisor_leases` と `one_executing_run_per_queue` の DROP）、`domain::RunLease`（`SupervisorLease` を置換）、`ClaimOutcome::Busy` 削除、`sqlite::claim_task`（トランザクション内の claim を共有）、`runtime_store::{claim_for_supervisor, heartbeat_leases, release_lease, abandon_run, run_leases, run_lease}` と run 単位の `assert_lease` / `recover_run`、`adapters::GitRepository::main_head`（+ `Clone`）、`runtime::{SuperviseOptions, Supervisor, Slot, Phase, SessionWatch, spawn_validation, status}`、main.rs の `--parallel` / `--once` と SIGINT/SIGTERM handler（1回目 drain、2回目 SIG_DFL）
- 気付き（テスト fixture）: 依存 task が `integrate` 後の main から始まるとき、fake agent の `commit` が main と同じ内容を書くと「no commit was made」で reject される。`change.txt` の内容を `$RUN_ID` 入りにして解決。e2e の stub も同様に `e2e.txt` に session id を含める
- 気付き（テスト）: 8 thread 同時 claim の結果は thread 順で返るので、task id は sort して比較する（llvm-cov 実行時に 2/3 で落ちた）
- テスト: tests/runtime.rs 30 件（新規: 2 件同時 + `integrate` 後の後追い + graceful stop、timeout 1 件 + 正常 1 件の同居、失敗 2 件 + 正常 1 件の同居、2 orphan の片方だけ recover、provisioning 失敗で claim 停止、claim と lease の所有）、tests/queue.rs 14 件（v5 migration で lease が run に移ること、同時 claim が各 task 1 回ずつ）、tests/cli.rs（`status` の新形式）、tests/e2e.rs 2 件（1 件 `--once`、`--parallel 2` で 2 workspace が同時に list され、依存 task が `integrate` 後の pass で新 main から）
- ゲート: fmt / test 60 件（e2e 2 件は ignored）/ clippy / llvm-cov 行 88.21% / e2e（cmux 0.64.25、2 件同時の pass 9.4 秒、後追い 8.3 秒）通過。`claude plugin validate plugins/claude-taskq` → Validation passed。使い捨て repository で常駐 `supervise` に SIGINT を送り `{"outcome":"stopped"}` で終了することを手で確認
- docs: ADR-0007、design/{supervisor-lifecycle, persistence, domain-model, overview, plugin-integration}、README、plans/current.md ステップ6、plugin skill 3 件。`docs/journal/README.md` の Open は触っていない（SV が merge 時に更新）
- 未検証: 実 Claude を 2 件同時に走らせる経路（010 / 013 のドッグフーディングで確認）。SIGINT 2 回目の即終了後に lease が 30 秒で stale になる経路は設計どおりだが手動未確認

## Result

lease を run 単位の `run_leases`（schema v5）にし、queue 全体の実行枠を外した。`supervise --parallel N`（既定 4）は常駐ループで、候補を上限まで claim（毎回 `refs/heads/main` を読み直す）→ provision → run ごとの `SessionWatch` で監視 → run ごとの thread で receipt 検証 → close → lease 解放を繰り返し、`integrate` で依存が解けた task を次の poll で拾う。`--once` で 1 batch、SIGINT/SIGTERM で graceful drain。1 run の runtime error はその run だけを abandon（`last_error` + `runtime_error` + lease 削除、status と資源はそのまま）し、provisioning 失敗は claim を止めて drain してから非0終了。`doctor` / `recover` / `status` は run ごとの lease で判定し、`recover` はその run の lease だけを消す。plugin skill は `--parallel` と run 単位の復旧に追従した。

完了条件: 依存のない 2 task が同時に `running` になり両方 `awaiting_integration` になること、依存 task が `integrate` まで待って新しい main から始まること、timeout / 非0終了 / receipt 拒否の run が隣の run に影響しないこと、2 orphan の片方だけを recover しても他方の lease と process が残ることを tests/runtime.rs で、2 件同時のハッピーパスを tests/e2e.rs で確認。fmt / test / clippy / llvm-cov 88.21% / e2e 通過。

## Promoted

- run 単位 lease、並列ループ、abandon、provisioning 失敗時の扱い、複数 supervisor の許容とその理由 → [ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)
- `supervise` のループ手順、`status` / `doctor` / `recover` の run 単位の判定 → [design/supervisor-lifecycle.md](../design/supervisor-lifecycle.md)
- `run_leases` の所有規則、claim と lease の同一トランザクション、migration 0005 → [design/persistence.md](../design/persistence.md)
- `RunLease` と「1 run の失敗は他に波及しない」不変条件 → [design/domain-model.md](../design/domain-model.md)
- 利用手順（`--parallel`、`--once`、Ctrl-C、`errors`）→ README、plugin skill
