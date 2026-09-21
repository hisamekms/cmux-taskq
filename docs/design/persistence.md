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

SQLiteはキューの正本であり、プロセス間共有と再起動後の復旧に使う。stdoutは正本にしない。ステップ2では以下の4テーブルを実装した。

```text
tasks
task_dependencies
task_runs
run_events
```

`task_dependencies(task_id, predecessor_id)`は依存関係を保存する。TaskRunは試行ごとに新しい行を作り、Taskに履歴を持たせる。現時点ではworkspace、worktree、receipt、logの参照列をtask_runsに置き、claim時点ではnullにする。process監視や成果物hashは未実装。

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

`run_workspaces`、`run_processes`、`run_artifacts`、`supervisor_leases`はステップ3以降で必要な操作とともに追加する。ログ本体はファイルに置き、pathとhashをDBへ記録する予定。

supervisorはleaseをheartbeat付きでclaimする。一定時間heartbeatが更新されないrunは自動再実行せず、`recover`または`doctor`で確認する。

旧Python版の状態を読み込む移行コマンドは後続の配布段階で用意し、task ID、依存、run履歴、ログpathを保持する。
