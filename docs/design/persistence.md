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
  - design-domain-model
---

# SQLite persistence

SQLiteはキューの正本であり、プロセス間共有と再起動後の復旧に使う。stdoutは正本にしない。ステップ2で4テーブル、ステップ3で3テーブル、ステップ4で`task_runs.workspace_closed_at`列を実装した（schema version 3）。

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
- `queue_repository`は最初の`supervise`でGit common directoryを記録し、以後は同じrepositoryだけを受け付ける。
- `run_processes`は`(run_id, role)`を主キーとし、wrapperとagentの登録は1回限りにする。wrapperの操作は登録したPIDかつ未終了であることを要求する。
- ログ本体とreceiptは`<db>.runs/<run-id>/`のファイルに置き、pathをtask_runsへ記録する。
- receipt検証は`validating`かつ同じsupervisor tokenのrunだけを`awaiting_integration`または`failed`へ進める。検証コマンドの結果は`verification_command`イベント、判定とreceiptの内容は`validation_finished`イベントに置き、専用テーブルは持たない。
- workspaceのcloseは`awaiting_integration`かつ同じtokenで`workspace_closed_at`がnullのrunだけに記録でき、`workspace_closed`イベントと一緒に一度だけ書く。失敗は`cleanup_failed`イベントと`last_error`に残し、列はnullのままにする。イベントから導出せず列に持つのは、`doctor`/`recover`や統合確認が閉じていないworkspaceを1クエリで拾えるようにし、失敗後の再試行で`cleanup_failed`と`workspace_closed`の順序を追わずに済ませるため。
- `recover`だけがtokenなしで未完了runを`interrupted`にし、leaseを削除する。同じトランザクションでheartbeatが30秒以内のleaseがないことと`run_processes`の行数が事前確認と一致することを再検査する。確認したプロセス・leaseの状態は`run_recovered`イベントに残し、`run_processes`の行は変更しない。

## Transactions and constraints

- `BEGIN IMMEDIATE`で状態変更、依存グラフ検証、claimを直列化する。ロック待機は最大5秒で、タイムアウトはエラーとして呼び出し元へ返す。
- claimは実行枠確認、候補選択、Task更新、TaskRun作成、イベント保存を一つのトランザクションにまとめる。途中エラーでは全体をrollbackする。
- 部分UNIQUE indexで、キュー全体の実行中run（claimed/starting/running/validating）を1件に制限する。
- Taskごとの未完了runにはawaiting_integrationも含め、同じTaskの重複runを制限する。統合待ちはキュー全体の実行枠を使わない。
- foreign key、statusのCHECK制約、依存の複合主キーを設ける。自己依存はDBとapplicationの両方で拒否し、循環はトランザクション内の再帰CTEで検証する。
- Task詳細は一つのread transactionで読むため、Task・run・イベント間のスナップショットが揃う。

SQLiteの書き込みトランザクションとIMMEDIATEの挙動は[公式仕様](https://www.sqlite.org/lang_transaction.html)に従う。

## Database setup and migrations

`cmux-taskq --db PATH init`で初期化する。DBの親ディレクトリは既存のものを指定する。CLIはパスを必須とし、通常の操作で存在しないDBを暗黙に作成しない。

SQLiteはrusqliteのbundled機能で同梱する。初期化でWALを有効にし、各接続でforeign_keysを有効にする。migrationは`migrations/0001_queue.sql`から順に適用し、`PRAGMA user_version`とDDL更新を同じトランザクションでcommitする。

`application_id = 0x43545131`でcmux-taskqのDBを識別する。未知の新しいschemaや他アプリのDBは書き換えずに拒否する。`init`の再実行では登録済みデータを保持する。バイナリ更新時は既存DBをopenする際にも未適用migrationを確認する。

## Planned runtime persistence

receipt検証結果はイベントとtask_runsの列で足りたため、`run_artifacts`テーブルは追加しなかった。成果物hashが必要になった時点で検討する。一定時間heartbeatが更新されないrunは自動再実行せず、`doctor`で確認して`recover`で明示的に閉じる。`interrupted`は最初からCHECK制約に含まれていたため、復旧のためのmigrationは不要だった。

旧Python版の状態を読み込む移行コマンドは後続の配布段階で用意し、task ID、依存、run履歴、ログpathを保持する。
