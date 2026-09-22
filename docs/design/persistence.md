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
  - design-domain-model
---

# SQLite persistence

SQLiteはキューの正本であり、プロセス間共有と再起動後の復旧に使う。stdoutは正本にしない。ステップ2で4テーブル、ステップ3で3テーブル、ステップ4で`task_runs.workspace_closed_at`列を実装し、[008](../journal/008-integration-confirm.md)で`task_runs`を作り直して`integrated`を加えた（schema version 4）。

```text
tasks
task_dependencies
task_runs
run_events
queue_repository     -- 0002: キューを束縛するGit common directory（1行）
supervisor_leases    -- 0002: supervisor token、PID、heartbeat（1行）
run_processes        -- 0002: runごとのwrapper/agentのPID、heartbeat、終了コード
```

`task_dependencies(task_id, predecessor_id)`は依存関係を保存する。TaskRunは試行ごとに新しい行を作り、Taskに履歴を持たせる。workspace、worktree、receipt、log、repo、run directory、supervisor token、last errorの参照列をtask_runsに置き、claim時点ではnullにする。`result_commit`は検証で確認したcommit、`last_error`は検証の拒否理由、cleanup失敗、またはruntime errorを持つ。`workspace_closed_at`（0003）はcmuxがcloseを確認した時刻で、nullの間はworkspaceを開いているものとして扱う。成果物hashは未実装。

## Runtime ownership

- supervisorは`supervisor_leases`の唯一行をtokenで所有し、2秒ごとにheartbeatを更新する。run状態を変える操作はtokenと30秒以内のheartbeatを要求する。leaseは自動で奪わない。
- `queue_repository`はqueueを束縛するGit common directoryを持つ。cwdから解決したqueueは`init`が記録し（`bind_repository`）、以後の全コマンドがopen直後に一致を検査する（`assert_repository`）。`--db`のqueueは最初の`supervise`が記録し、`supervise`と`integrate`が検査する。束縛の付け替えは行わない。
- `run_processes`は`(run_id, role)`を主キーとし、wrapperとagentの登録は1回限りにする。wrapperの操作は登録したPIDかつ未終了であることを要求する。
- ログ本体とreceiptはDBと同じdirの`runs/<run-id>/`のファイルに置き、pathをtask_runsへ記録する。idle marker `idle.json`とClaudeのsettingsは`run_dir`から導出し、列は持たない。
- receipt受領後の終了要求は`session_idle_observed`（markerとreceiptのmtime、hookのフィールド）、`exit_requested`、`exit_request_timed_out`のイベントだけで表し、runのstatusは変えない。
- receipt検証は`validating`かつ同じsupervisor tokenのrunだけを`awaiting_integration`または`failed`へ進める。検証コマンドの結果は`verification_command`イベント、判定とreceiptの内容は`validation_finished`イベントに置き、専用テーブルは持たない。
- workspaceのcloseは`awaiting_integration`かつ同じtokenで`workspace_closed_at`がnullのrunだけに記録でき、`workspace_closed`イベントと一緒に一度だけ書く。失敗は`cleanup_failed`イベントと`last_error`に残し、列はnullのままにする。イベントから導出せず列に持つのは、`doctor`/`recover`や統合確認が閉じていないworkspaceを1クエリで拾えるようにし、失敗後の再試行で`cleanup_failed`と`workspace_closed`の順序を追わずに済ませるため。
- `recover`だけがtokenなしで未完了runを`interrupted`にし、leaseを削除する。同じトランザクションでheartbeatが30秒以内のleaseがないことと`run_processes`の行数が事前確認と一致することを再検査する。確認したプロセス・leaseの状態は`run_recovered`イベントに残し、`run_processes`の行は変更しない。
- 統合確認はleaseを要求しない。`awaiting_integration`のrunを`integrated`、そのTaskを`in_progress`から`completed`へ、statusを条件にした2つのUPDATEと`run_integrated`・`task_status_changed`イベントで1トランザクションに進める。Git上の判定はトランザクションの外で行い、不一致なら何も書かない。

## Transactions and constraints

- `BEGIN IMMEDIATE`で状態変更、依存グラフ検証、claimを直列化する。ロック待機は最大5秒で、タイムアウトはエラーとして呼び出し元へ返す。
- claimは実行枠確認、候補選択、Task更新、TaskRun作成、イベント保存を一つのトランザクションにまとめる。途中エラーでは全体をrollbackする。
- 部分UNIQUE indexで、キュー全体の実行中run（claimed/starting/running/validating）を1件に制限する。
- Taskごとの未完了runにはawaiting_integrationも含め、同じTaskの重複runを制限する。統合待ちはキュー全体の実行枠を使わない。
- 部分UNIQUE index `one_integrated_run_per_task`で、Taskごとの`integrated` runを1件に制限する。
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
```

- `<hash>`はcanonicalizeしたGit common directoryのUTF-8 bytesのSHA-256のhex先頭16文字（`repository_hash`）。symlink経由やworktreeからでも同じhashになる。
- `--db PATH`は明示override。run dirは`dirname PATH`/`runs/`で、規則はcwd解決と同じ（`runs_dir`）。`git_common_dir`は持たず、`locate`の`source`は`db_flag`になる。
- `locate`は解決結果（`db`、`queue_dir`、`runs_dir`、`source`、`git_common_dir`、`db_exists`）をDBを開かずに返す。
- repositoryを移動するとhashが変わり新しいqueueに解決される。旧queueは`repository`ファイルで特定し、`--db`で開く。

## Database setup and migrations

`cmux-taskq init`（またはrepository外から`cmux-taskq --db PATH init`）で初期化する。`init`はDBのdirを`create_dir_all`で作り、cwdから解決したqueueなら`repository`ファイルを書いて`queue_repository`を束縛する。通常の操作で存在しないDBを暗黙に作成しない。

SQLiteはrusqliteのbundled機能で同梱する。初期化でWALを有効にし、各接続でforeign_keysを有効にする。migrationは`migrations/0001_queue.sql`から順に適用し、`PRAGMA user_version`とDDL更新を同じトランザクションでcommitする。

`application_id = 0x43545131`でcmux-taskqのDBを識別する。未知の新しいschemaや他アプリのDBは書き換えずに拒否する。`init`の再実行では登録済みデータを保持する。バイナリ更新時は既存DBをopenする際にも未適用migrationを確認する。

SQLiteはCHECK制約を変更できないため、statusの追加はtableの作り直し（`CREATE ... _vN` → `INSERT ... SELECT`（rowidも複写） → `DROP` → `RENAME` → index再作成）で行う。`DROP TABLE`はforeign keyが有効だと参照元の行があるとき失敗するので、migration実行中はトランザクションの外で`foreign_keys=OFF`にし、commit前に`pragma_foreign_key_check`が0件であることを確認してからONに戻す。`0004_integration.sql`がこの形の最初の例。

## Planned runtime persistence

receipt検証結果はイベントとtask_runsの列で足りたため、`run_artifacts`テーブルは追加しなかった。成果物hashが必要になった時点で検討する。一定時間heartbeatが更新されないrunは自動再実行せず、`doctor`で確認して`recover`で明示的に閉じる。`interrupted`は最初からCHECK制約に含まれていたため、復旧のためのmigrationは不要だった。

旧Python版の状態を読み込む移行コマンドは後続の配布段階で用意し、task ID、依存、run履歴、ログpathを保持する。
