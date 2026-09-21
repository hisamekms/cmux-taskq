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

SQLiteはキューの正本であり、プロセス間共有と再起動後の復旧に使う。stdoutは正本にしない。ステップ2で4テーブル、ステップ3で3テーブルを実装した（schema version 2）。

```text
tasks
task_dependencies
task_runs
run_events
queue_repository     -- 0002: キューを束縛するGit common directory（1行）
supervisor_leases    -- 0002: supervisor token、PID、heartbeat（1行）
run_processes        -- 0002: runごとのwrapper/agentのPID、heartbeat、終了コード
```

`task_dependencies(task_id, predecessor_id)`は依存関係を保存する。TaskRunは試行ごとに新しい行を作り、Taskに履歴を持たせる。workspace、worktree、receipt、log、repo、run directory、supervisor token、last errorの参照列をtask_runsに置き、claim時点ではnullにする。成果物hashは未実装。

## Runtime ownership

- supervisorは`supervisor_leases`の唯一行をtokenで所有し、2秒ごとにheartbeatを更新する。run状態を変える操作はtokenと30秒以内のheartbeatを要求する。leaseは自動で奪わない。
- `queue_repository`は最初の`supervise`でGit common directoryを記録し、以後は同じrepositoryだけを受け付ける。
- `run_processes`は`(run_id, role)`を主キーとし、wrapperとagentの登録は1回限りにする。wrapperの操作は登録したPIDかつ未終了であることを要求する。
- ログ本体とreceiptは`<db>.runs/<run-id>/`のファイルに置き、pathをtask_runsへ記録する。

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

成果物のhashやreceipt検証結果を保持する`run_artifacts`はステップ4で追加する。一定時間heartbeatが更新されないrunは自動再実行せず、`recover`または`doctor`で確認する。

旧Python版の状態を読み込む移行コマンドは後続の配布段階で用意し、task ID、依存、run履歴、ログpathを保持する。
