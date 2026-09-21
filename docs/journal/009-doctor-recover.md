---
id: journal-009
type: journal
title: doctor and recover
status: planned
created: 2026-09-22
updated: 2026-09-22
plan_step: 4
queue_task: null
depends_on_journal: [5]
verify:
  - cargo fmt --all --check
  - cargo test --locked
  - cargo clippy --locked --all-targets -- -D warnings
related:
  - design-supervisor-lifecycle
  - design-persistence
---

# 009: doctor and recover

## Goal

[plans/current.md](../plans/current.md) ステップ4。停止したsupervisorや孤児runを人が確認して明示的に復旧できるようにする。

- `doctor`: stale lease、heartbeat切れのrun、終了していないwrapper/agentプロセス、存在しないworktree/workspaceを一覧する。状態は変えない。
- `recover RUN`: 旧プロセスの停止を確認してからrunを`interrupted`（または`failed`）にし、leaseを解放する。リソースは削除しない。
- 再試行は`ready`に戻して新しいTaskRunを作る。孤児runは自動再実行しない。

完了条件: supervisor強制終了後に`doctor`が状態を報告し、`recover`後に同じtaskを新しいrunで再実行でき、生きているプロセスがある間は`recover`が拒否されることをテストで確認できる。

## Log

## Result

## Promoted
