---
id: plan-rust-runtime-mvp
type: plan
title: Rust runtime MVP
status: active
created: 2026-09-22
updated: 2026-09-22
milestone: mvp
target: 2026-10-31
owners:
  - hisamekms
depends_on:
  - adr-0001
  - adr-0002
  - adr-0003
  - adr-0004
  - adr-0005
---

# Rust runtime MVP

## Goal

SQLiteをsource of truthとするRust製のタスクランタイムを作り、依存関係を満たしたタスクをcmuxのworkspaceとGit worktreeで実行できるようにする。Claude Codeを標準プロバイダーとし、Codexを選択またはフォールバック先として追跡する。

## Phases

1. Rust workspaceとドメインモデルを作る。
2. SQLiteのスキーマ、マイグレーション、イベント永続化を実装する。
3. タスク登録、依存関係、claim、状態遷移、手動発火判定のCLIを実装する。
4. cmux workspace、Git worktree、Supervisorのライフサイクルを実装する。
5. Claude Codeを通常モードのセッションとして起動し、完了receiptを受け取る。
6. Codexプロバイダー、選択設定、Claude実行不能時のフォールバックを実装する。
7. heartbeat、再実行、回復、`doctor`による孤児runの検出を実装する。
8. Claude Code/Codex向けプラグインとバイナリリリースを整備する。

## Acceptance criteria

- 依存関係を満たしたタスクだけが一度に一つのrunとしてclaimされる。
- タスク、TaskRun、workspace、worktree、providerの関係がSQLiteから追跡できる。
- エージェントは実装、unit test、E2E、subagent reviewを完了receiptに記録する。
- 成功したrunをSupervisorが検証した後にcmux workspaceを削除する。
- 失敗または中断したrunでは、調査のためworkspaceとworktreeを保持する。
- 既存のPythonキューから移行できる。
- バイナリとプラグインを同じバージョンで配布できる。

## Out of scope

- cmux以外のworkspaceバックエンド
- Web UI
- 本番環境への自動デプロイ
- 外部スケジューラーによる定期実行
