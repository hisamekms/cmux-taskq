---
id: plan-milestones
type: plan
title: Project milestones
status: active
created: 2026-09-22
updated: 2026-09-22
milestone: roadmap
owners:
  - hisamekms
---

# Project milestones

## M0: Documentation baseline

ADR、現在の設計、MVP計画を確定する。設計変更は影響範囲に応じてADRへ追記する。

## M1: Local Rust runtime

SQLiteのタスク管理、依存関係、状態遷移、イベント、Supervisorの基本ライフサイクルをローカルで動かす。

## M2: Agent providers

Claude Codeの通常セッションを標準経路として実行し、Codexの選択とフォールバック、完了receipt、workspace cleanupを追加する。

## M3: Distribution

バイナリのリリースとClaude Code/Codexプラグインを整備し、既存のPythonキューからの移行手順を公開する。
