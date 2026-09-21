---
id: design-persistence
type: design
title: SQLite persistence
status: current
created: 2026-09-21
updated: 2026-09-21
last_verified: 2026-09-21
scope: persistence
related:
  - adr-0003
  - design-domain-model
---

# SQLite persistence

SQLiteはキューの正本であり、プロセス間共有と再起動後の復旧に使う。stdoutは正本にせず、ログファイルのpathとhashだけをDBに記録する。

```text
tasks
task_dependencies
task_runs
run_workspaces
run_processes
run_events
run_artifacts
supervisor_leases
```

`task_dependencies(task_id, predecessor_id)`は依存関係を保存する。TaskRunは再試行ごとに新しい行を作り、Taskに実行履歴を持たせる。workspace、process、eventはTaskRunに紐づける。

supervisorはleaseをheartbeat付きでclaimする。一定時間heartbeatが更新されないrunは自動再実行せず、`recover`または`doctor`で確認する。

schema変更はmigrationとして管理する。旧Python版の状態を読み込む移行コマンドを用意し、task ID、依存、run履歴、ログpathを保持する。
