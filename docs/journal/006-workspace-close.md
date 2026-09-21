---
id: journal-006
type: journal
title: Workspace close after validation
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
  - adr-0003
---

# 006: Workspace close after validation

## Goal

[plans/current.md](../plans/current.md) ステップ4。検証成功とセッション終了を確認した後にsupervisorがcmux workspaceを閉じ、worktreeとbranchは統合まで残す。

- `WorkspaceBackend`に`close`を追加し、cmuxの応答を検証する。
- close失敗は`cleanup_failed`として記録し、closeされていないworkspaceをcleaned扱いにしない。
- 失敗・中断・heartbeat切れではworkspaceを閉じない。

完了条件: 成功runでworkspaceが閉じられworktreeが残ること、close失敗が記録され状態が進まないことをテストで確認できる。

## Log

## Result

## Promoted
