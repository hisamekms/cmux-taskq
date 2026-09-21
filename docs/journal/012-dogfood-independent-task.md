---
id: journal-012
type: journal
title: Dogfooding: one independent task
status: planned
created: 2026-09-22
updated: 2026-09-22
plan_step: 6
queue_task: null
depends_on_journal: [11]
related:
  - plan-rust-runtime-mvp
---

# 012: Dogfooding: one independent task

## Goal

[plans/current.md](../plans/current.md) ステップ6。cmux-taskq自身のドキュメント改善をタスクとして登録し、固定したビルド済みバイナリで実行して成果をmainへ取り込む。

- runtimeバイナリはrepo外にコピーして使い、作業成果で置き換えない。
- キューDBはGit common directory配下に置く。

完了条件: 登録から実行、receipt検証、workspace終了、差分と証跡のレビュー、手動merge、`integrate`による`completed`まで、DBの手修正なしで通る。手順をこのジャーナルに記録する。

## Log

## Result

## Promoted
