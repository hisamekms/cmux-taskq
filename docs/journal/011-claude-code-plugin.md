---
id: journal-011
type: journal
title: Claude Code local plugin
status: planned
created: 2026-09-22
updated: 2026-09-22
plan_step: 8
queue_task: null
depends_on_journal: [19]
verify:
  - cargo fmt --all --check
  - cargo test --locked
  - cargo clippy --locked --all-targets -- -D warnings
related:
  - adr-0005
  - design-plugin-integration
---

# 011: Claude Code local plugin

## Goal

[plans/current.md](../plans/current.md) ステップ8。cmux-taskqで流す最初の開発task。Claude Code内の依頼から、ローカルビルドしたバイナリ経由でタスクの登録・状態確認・実行開始・統合確認ができる薄いpluginを作る。

- バイナリの場所とバージョンを確認するskillを用意する。
- task登録では説明、受け入れ条件、依存、検証コマンドをCLIへ渡す。
- 結果はCLIのJSONをエージェントが読める形で返す。pluginはDBを直接変更しない。
- 完了通知は停止hookだけで判定せず、receiptと`show`の状態を使う。

完了条件: Claude Codeのセッションから登録・実行開始・結果確認・統合確認まで操作できる。

## Log

## Result

## Promoted
