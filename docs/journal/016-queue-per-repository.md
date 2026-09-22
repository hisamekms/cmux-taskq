---
id: journal-016
type: journal
title: One queue per repository under the user directory
status: planned
created: 2026-09-22
updated: 2026-09-22
plan_step: 5
queue_task: null
depends_on_journal: []
verify:
  - cargo fmt --all --check
  - cargo test --locked
  - cargo clippy --locked --all-targets -- -D warnings
related:
  - plan-rust-runtime-mvp
  - design-persistence
---

# 016: One queue per repository under the user directory

## Goal

[plans/current.md](../plans/current.md) ステップ5。1 repositoryに1 queueとし、DBをユーザーDIRに置いてcwdから解決できるようにする。

- DBは`~/.local/share/cmux-taskq/<Git common directoryの正規化パスのhash>/queue.db`（`XDG_DATA_HOME`があれば従う）。run dir・worktree・ログも同じ配下に置く。
- CLIはcwdから`git rev-parse --git-common-dir`でqueueを解決する。`--db`は使い捨てrepositoryとテスト用の明示overrideとして残す。`init`は「このrepositoryのqueueを作る」操作になり、`supervise --repo`と`integrate --repo`は不要になる。
- `queue_repository`の束縛検査は残す。
- `plugins/claude-taskq/`のskillとlauncherを新しい解決方法（`--db`なし、`--repo`なし）に追従させる。
- ADRを追加し、`persistence.md`・`supervisor-lifecycle.md`・READMEを更新する。

完了条件: repository内の任意のworktree（run worktreeを含む）から`--db`なしで同じqueueが使え、別のrepositoryからは別のqueueになることがテストで確認できる。既存のunit testは`--db` overrideでそのまま通る。

## Log

## Result

## Promoted
