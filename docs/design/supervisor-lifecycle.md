---
id: design-supervisor-lifecycle
type: design
title: Supervisor and workspace lifecycle
status: current
created: 2026-09-21
updated: 2026-09-21
last_verified: 2026-09-21
scope: runtime
related:
  - adr-0002
  - adr-0003
  - design-persistence
---

# Supervisor and workspace lifecycle

```text
ready task
  → claim TaskRun
  → create Git worktree
  → create cmux workspace
  → start session wrapper
  → start Claude/Codex
  → running
  → completion receipt
  → validate commit, tests, clean state
  → close cmux workspace
  → awaiting_integration / succeeded
```

workspace削除はsupervisorが行う。成功時はworkspaceだけを削除し、`integrated`ではworktreeとbranchをmainへの反映まで残す。失敗・中断・heartbeat切れの場合は調査のためworkspaceとworktreeを残す。

supervisorの再起動ではleaseとheartbeatを確認し、孤児プロセスを勝手に再実行しない。ユーザーが`recover`で明示的に復旧した後に新しいTaskRunを作る。
