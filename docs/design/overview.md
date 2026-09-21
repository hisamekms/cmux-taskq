---
id: design-overview
type: design
title: System overview
status: current
created: 2026-09-21
updated: 2026-09-22
last_verified: 2026-09-22
scope: system
related:
  - adr-0001
  - adr-0002
  - adr-0003
  - adr-0004
  - adr-0005
---

# System overview

ステップ2時点でRust CLIとSQLiteキューを実装済み。以下の構成図のsupervisor、cmux adapter、provider、pluginは後続実装であり、CLIからのエージェント起動はまだ行わない。

コードは単一Cargo package内で、`domain`（型と状態遷移）、`application`（キュー操作の契約）、`infrastructure::sqlite`（トランザクションと永続化）、`main`（CLI）に分離している。利用方法は[README](../../README.md)を参照。

cmux-taskqは、依存関係を持つ開発タスクをSQLiteで管理し、着手可能なタスクをcmux workspaceとGit worktreeで実行するRust runtimeである。

```text
CLI / Claude plugin / Codex plugin
                │
                ▼
        SQLite task queue
                │
                ▼
           supervisor
          ┌─────┴─────┐
          ▼           ▼
      cmux adapter  provider
          │       ┌───┴───┐
          ▼       ▼       ▼
       workspace Claude  Codex
          │
          ▼
       Git worktree
```

タスクは`draft | ready | in_progress | completed | canceled`を持つ。`ready`で依存先がすべて`completed`のタスクだけがschedulerの起動候補になる。詳細な実行状態はTaskRunに保存する。

supervisorはagentの完了レシート、コミット、テスト、worktreeのclean状態を確認してからworkspaceを削除する。`integrated`運用では、成果がmainへ取り込まれるまでworktreeを残す。
