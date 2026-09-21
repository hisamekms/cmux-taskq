---
id: journal-002
type: journal
title: Rust/SQLite minimal queue
status: done
created: 2026-09-22
updated: 2026-09-22
plan_step: 2
queue_task: null
related:
  - plan-rust-runtime-mvp
---

# 002: Rust/SQLite minimal queue

## Goal

[plans/current.md](../plans/current.md) ステップ2。DBを開き直して状態が復元でき、競合するclaimでも同じtaskに二つのactive runができないことをテストで確認する。

## Log

ジャーナル導入前に完了。経過はcommit `f904469` とそのメッセージにある。

## Result

登録・一覧・詳細・ready/draft/cancel・依存追加/削除・候補確認をCLIとして実装。claimはライブラリAPI。CLIとキューの計15テスト、fmt、Clippyを通過。schema version 1。

## Promoted

- [design/persistence.md](../design/persistence.md), [design/domain-model.md](../design/domain-model.md): 実装内容を反映
