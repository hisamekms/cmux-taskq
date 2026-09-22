---
id: design-persistence
type: design
title: SQLite persistence
status: current
created: 2026-09-21
updated: 2026-09-22
last_verified: 2026-09-22
scope: persistence
related:
  - adr-0003
  - adr-0006
  - adr-0007
  - adr-0008
  - adr-0009
  - adr-0010
  - design-domain-model
---

# SQLite persistence

SQLiteはキューの正本であり、プロセス間共有と再起動後の復旧に使う。stdoutは正本にしない。ステップ2で4テーブル、ステップ3で3テーブル、ステップ4で`task_runs.workspace_closed_at`列を実装し、[008](../journal/008-integration-confirm.md)で`task_runs`を作り直して`integrated`を加え（schema version 4）、[017](../journal/017-parallel-runs.md)でleaseをrun単位の`run_leases`に移してqueue全体の実行枠を外し（schema version 5、[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)）、[018](../journal/018-merge-queue.md)で`task_runs`を再び作り直して`integrating`と`needs_session`を加え、統合スロットの部分UNIQUE indexを足した（schema version 6、[ADR-0008](../adr/0008-merge-queue-squash-landing.md)）。`0007_supervisors.sql`は常駐supervisorの登録表`supervisors`を足した（schema version 7）。`0008_goals.sql`は`goals`、`tasks.goal_id` / `tasks.context`を足し、`run_events`を作り直してgoal単位のイベントを持てるようにした（schema version 8、[ADR-0009](../adr/0009-goal-groups-tasks.md)。ADRの「schema v6」は7がsupervisorsに使われたためv8と読み替える）。

```text
tasks
task_dependencies
task_runs
run_events
queue_repository     -- 0002: キューを束縛するGit common directory（1行）
run_processes        -- 0002: runごとのwrapper/agentのPID、heartbeat、終了コード
run_leases           -- 0005: runごとのsupervisor token、PID、heartbeat（0002のsupervisor_leasesを置き換え）
supervisors          -- 0007: 常駐superviseプロセスの登録（token主キー、PID、parallel、started_at、heartbeat_at）
goals                -- 0008: 複数taskが解く課題（title、description、acceptance、constraints、doc、closed_at、verdict）
```

`task_dependencies(task_id, predecessor_id)`は依存関係を保存する。TaskRunは試行ごとに新しい行を作り、Taskに履歴を持たせる。workspace、worktree、receipt、log、repo、run directory、supervisor token、last errorの参照列をtask_runsに置き、claim時点ではnullにする。`result_commit`は検証で確認したcommitで、着地後は`main`に積んだsquash commitに置き換わる（rebase後のrun headは`refs/taskq/runs/<run-id>`と`run_integrated`イベントの`source_commit`が持つ）。`last_error`は検証の拒否理由、`needs_session`の理由、cleanup失敗、またはruntime errorを持ち、着地で消える。`workspace_closed_at`（0003）はcmuxがcloseを確認した時刻で、nullの間はworkspaceを開いているものとして扱う。成果物hashは未実装。

`goals`（0008）はgoalを1行で持つ。`title`は空でなく、`description` / `acceptance` / `constraints`は既定`''`、`doc`はnull可。`verdict`は`'achieved'` | `'abandoned'` | nullで、CHECK `(closed_at IS NULL) = (verdict IS NULL)`により閉じたgoalだけがverdictを持つ。`tasks.goal_id`は`goals(id)`への外部キー（null可、index `tasks_by_goal`）、`tasks.context`は`TEXT NOT NULL DEFAULT ''`で、v7以前のtaskは移行後に`goal_id = NULL`、`context = ''`になる。`run_events`は0008で作り直し、`task_id`をnull可にして`goal_id`（`goals(id)`への外部キー、index `events_by_goal`）を足した。CHECKで`task_id`と`goal_id`の少なくとも一方が非null、`run_id`があれば`task_id`も非nullとし、`(run_id, task_id) → task_runs(id, task_id)`の複合外部キーは残す。goal単位のイベント（`goal_created`、`goal_updated`、`goal_closed`）は`goal_id`だけを持ち、`task_goal_changed`はtaskのイベントとして`task_id`を持つ。goalの操作（`add_goal` / `edit_goal` / `close_goal` / `set_goal`と`add --goal`）は他の状態変更と同じく`BEGIN IMMEDIATE`で直列化し、closeの判定（status別件数）とverdictの書き込み、set-goalのtask statusとgoalの開閉の確認を同じトランザクションで行う。

## Runtime ownership

- supervisorは実行中のrunごとに`run_leases`の行（`run_id`主キー、token、PID、heartbeat）を所有する。tokenはsupervisorプロセスに1つで、2秒ごとに同じtokenの全行のheartbeatを1文で更新する。claimはrun・`task_runs.supervisor_token`・lease行を1トランザクションで作り（`claim_for_supervisor`）、所有者のないclaimed runを作らない。run状態を変える操作はそのrunのtokenと30秒以内のheartbeatを要求する。runが`awaiting_integration`または`failed`になったらlease行を削除する（`lease_released`）。runtime errorでrunを手放すとき（abandon）は`last_error`と`runtime_error`（`lease_released`）を書いてlease行だけを削除し、statusとprocessは変えない。`supervisor_token`は記録として残る。leaseは自動で奪わない。
- `supervisors`は常駐`supervise`プロセスの登録で、runを持たないsupervisorを`status`/`doctor`から見えるようにする（leaseはrunがあるときしか存在しないため）。`supervise`は起動時、heartbeat threadを始める前に自分のtokenをキーに`pid`、`parallel`（`--parallel`、CHECKで1以上）、`started_at`を1行書く（`register_supervisor`）。heartbeatは`heartbeat(token)`が`supervisors`と`run_leases`の同じtokenの行を1トランザクションで2秒ごとに更新する。ループを抜けるとき（drain完了、`--once`の完了、provisioning失敗後のdrain、claimやGitのエラーによる終了）に行を消す（`deregister_supervisor`）。heartbeat失敗で終わるときだけは消さず（DBに書けない可能性がある）、leaseと同じくstaleになる。killされたsupervisorの行は残り、`status`/`doctor`が`stale`（PIDが死んでいるかheartbeatが30秒より古い）として報告する。runtimeは自動で削除せず、`recover`と`integrate`もこの表に触れない。[ADR-0010](../adr/0010-maintainer-and-resident-supervisor.md)は`cmux-taskq up`がsupervisor起動の前にPIDの死んだ登録行だけを消す例外を決めた（T2で実装予定。leaseには触らない）。`run_leases.token`と`supervisors.token`が対応し、`status`/`doctor`はtokenでleaseを登録に結び付ける。`integrate`プロセスは登録せずleaseだけを持つので、外部キーは張らない。
- `queue_repository`はqueueを束縛するGit common directoryを持つ。cwdから解決したqueueは`init`が記録し（`bind_repository`）、以後の全コマンドがopen直後に一致を検査する（`assert_repository`）。`--db`のqueueは最初の`supervise`が記録し、`supervise`と`integrate`が検査する。束縛の付け替えは行わない。
- `run_processes`は`(run_id, role)`を主キーとし、wrapperとagentの登録は1回限りにする。wrapperの操作は登録したPIDかつ未終了であることを要求する。
- ログ本体とreceiptはDBと同じdirの`runs/<run-id>/`のファイルに置き、pathをtask_runsへ記録する。idle marker `idle.json`とClaudeのsettingsは`run_dir`から導出し、列は持たない。
- receipt受領後の終了要求は`session_idle_observed`（markerとreceiptのmtime、hookのフィールド）、`exit_requested`、`exit_request_timed_out`のイベントだけで表し、runのstatusは変えない。
- receipt検証は`validating`かつ同じsupervisor tokenのrunだけを`awaiting_integration`または`failed`へ進める。検証コマンドの結果は`verification_command`イベント、判定とreceiptの内容は`validation_finished`イベントに置き、専用テーブルは持たない。
- workspaceのcloseは`awaiting_integration`かつ同じtokenで`workspace_closed_at`がnullのrunだけに記録でき、`workspace_closed`イベントと一緒に一度だけ書く。失敗は`cleanup_failed`イベントと`last_error`に残し、列はnullのままにする。イベントから導出せず列に持つのは、`doctor`/`recover`や統合確認が閉じていないworkspaceを1クエリで拾えるようにし、失敗後の再試行で`cleanup_failed`と`workspace_closed`の順序を追わずに済ませるため。
- `recover`だけがtokenなしで未完了runを`interrupted`（`integrating`なら`awaiting_integration`）にし、そのrunのlease行を削除する。同じトランザクションでそのrunのheartbeatが30秒以内のleaseがないことと`run_processes`の行数が事前確認と一致することを再検査する。確認したプロセス・leaseの状態は`run_recovered`イベントに残し、`run_processes`と他のrunのleaseは変更しない。
- 着地（`integrate`）は`integrate`プロセスのtokenでlease行を持つ。`begin_integration`は`awaiting_integration`または`needs_session`のrunを`integrating`にし、lease行と`integration_started`を1トランザクションで書く。`integrating`のrunはqueue全体で高々1件（`one_integrating_run_per_queue`）。`integrating`から出る遷移はすべてそのtokenの新鮮なleaseを要求し、lease行を消して`lease_released`を書く: `finish_integration`（`integrated`、`result_commit`を着地commit、`last_error`をnull、Taskを`completed`、`run_integrated`と`task_status_changed`）、`defer_integration`（`needs_session`、`last_error`に理由、`integration_deferred`）、`fail_integration`（`failed`、`integration_failed`）、`abort_integration`（着地開始時のstatusへ戻す、`integration_error`）。Gitの操作はトランザクションの外で行い、mainを進める前のerrorではDBを元のstatusに戻す。着地後のworktree削除の失敗は`cleanup_failed`イベントと`last_error`だけに残す（`record_cleanup_failure`）。

## Transactions and constraints

- `BEGIN IMMEDIATE`で状態変更、依存グラフ検証、claimを直列化する。ロック待機は最大5秒で、タイムアウトはエラーとして呼び出し元へ返す。
- claimは候補選択、Task更新、TaskRun作成、イベント保存（supervisorからはlease行も）を一つのトランザクションにまとめる。途中エラーでは全体をrollbackする。同時claimは`BEGIN IMMEDIATE`で直列化され、別々のtaskを取る。
- キュー全体の実行枠はない（0005で`one_executing_run_per_queue`を削除）。同時実行数は`supervise --parallel`が決める。
- 部分UNIQUE index `one_unfinished_run_per_task`で、Taskごとの未完了run（`awaiting_integration`、`integrating`、`needs_session`を含む）を1件に制限する。
- 部分UNIQUE index `one_integrated_run_per_task`で、Taskごとの`integrated` runを1件に制限する。
- 部分UNIQUE index `one_integrating_run_per_queue`で、queue全体の`integrating` runを1件に制限する（統合スロット）。
- foreign key、statusのCHECK制約、依存の複合主キーを設ける。自己依存はDBとapplicationの両方で拒否し、循環はトランザクション内の再帰CTEで検証する。
- Task詳細は一つのread transactionで読むため、Task・run・イベント間のスナップショットが揃う。

SQLiteの書き込みトランザクションとIMMEDIATEの挙動は[公式仕様](https://www.sqlite.org/lang_transaction.html)に従う。

## Queue location

[ADR-0006](../adr/0006-queue-per-repository.md)。queueはrepositoryごとに1つで、`src/infrastructure/location.rs`の`QueueLocation`が場所を決める。

```text
$XDG_DATA_HOME/cmux-taskq/<hash>/        XDG_DATA_HOME が未設定・空・相対 path なら $HOME/.local/share
  queue.db                               SQLite（WAL の -wal / -shm も隣に置かれる）
  repository                             束縛先の Git common directory（人向けの逆引き）
  runs/<run-id>/                         prompt、runner、worktree/、claude-settings.json、idle.json、receipt.json、ログ
  logs/supervisor-<started_at>.log       supervisor の起動ごとの log（T2 で実装予定、ADR-0010。現状は存在しない）
```

- `<hash>`はcanonicalizeしたGit common directoryのUTF-8 bytesのSHA-256のhex先頭16文字（`repository_hash`）。symlink経由やworktreeからでも同じhashになる。
- `--db PATH`は明示override。run dirは`dirname PATH`/`runs/`で、規則はcwd解決と同じ（`runs_dir`）。`git_common_dir`は持たず、`locate`の`source`は`db_flag`になる。
- `locate`は解決結果（`db`、`queue_dir`、`runs_dir`、`source`、`git_common_dir`、`db_exists`）をDBを開かずに返す。log dir（`logs/`）の出力は[ADR-0010](../adr/0010-maintainer-and-resident-supervisor.md)で決め、T2で足す。
- repositoryを移動するとhashが変わり新しいqueueに解決される。旧queueは`repository`ファイルで特定し、`--db`で開く。

## Database setup and migrations

`cmux-taskq init`（またはrepository外から`cmux-taskq --db PATH init`）で初期化する。`init`はDBのdirを`create_dir_all`で作り、cwdから解決したqueueなら`repository`ファイルを書いて`queue_repository`を束縛する。通常の操作で存在しないDBを暗黙に作成しない。

SQLiteはrusqliteのbundled機能で同梱する。初期化でWALを有効にし、各接続でforeign_keysを有効にする。migrationは`migrations/0001_queue.sql`から順に適用し、`PRAGMA user_version`とDDL更新を同じトランザクションでcommitする。

`application_id = 0x43545131`でcmux-taskqのDBを識別する。未知の新しいschemaや他アプリのDBは書き換えずに拒否する。`init`の再実行では登録済みデータを保持する。バイナリ更新時は既存DBをopenする際にも未適用migrationを確認する。

SQLiteはCHECK制約を変更できないため、statusの追加はtableの作り直し（`CREATE ... _vN` → `INSERT ... SELECT`（rowidも複写） → `DROP` → `RENAME` → index再作成）で行う。`DROP TABLE`はforeign keyが有効だと参照元の行があるとき失敗するので、migration実行中はトランザクションの外で`foreign_keys=OFF`にし、commit前に`pragma_foreign_key_check`が0件であることを確認してからONに戻す。`0004_integration.sql`がこの形の最初の例。`0005_run_leases.sql`は`run_leases`を作り、v4の`supervisor_leases`の行を`supervisor_token`が一致する実行中runへ移してから`supervisor_leases`と`one_executing_run_per_queue`を落とす（tableの作り直しは不要）。`0006_merge_queue.sql`は0004と同じ手順で`task_runs`を作り直して`integrating`と`needs_session`をCHECKに加え、`one_unfinished_run_per_task`を2状態込みで作り直し、`one_integrating_run_per_queue`を足す。`run_leases`の外部キーは`task_runs`を名前で参照しているので作り直し後もそのまま有効で、`foreign_key_check`で確認する。`0007_supervisors.sql`は`supervisors`を作るだけで、既存の行には触れない。v6のsupervisorが動いている最中にmigrationが走っても、そのsupervisorは登録を持たないままleaseだけで`status`/`doctor`に並ぶ。`0008_goals.sql`は`goals`を作り、`tasks`に`goal_id`と`context`を`ALTER TABLE ADD COLUMN`で足し（既定がnull / `''`なので作り直し不要）、`run_events`を0004と同じ手順で作り直す（`id`（AUTOINCREMENT）も複写するので、イベントIDと順序、`sqlite_sequence`の続きが保たれる）。`SqliteQueue::SCHEMA_VERSION`（`MIGRATIONS`の長さ）が最新の`user_version`で、testはこの定数と比較する。

## Planned runtime persistence

receipt検証結果はイベントとtask_runsの列で足りたため、`run_artifacts`テーブルは追加しなかった。成果物hashが必要になった時点で検討する。一定時間heartbeatが更新されないrunは自動再実行せず、`doctor`で確認して`recover`で明示的に閉じる。`interrupted`は最初からCHECK制約に含まれていたため、復旧のためのmigrationは不要だった。leaseの列を`task_runs`に足す案は、解放済みをnullで表すことになり実行の記録と揮発する所有権が混ざるため採らなかった（[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)）。

旧Python版の状態を読み込む移行コマンドは後続の配布段階で用意し、task ID、依存、run履歴、ログpathを保持する。
