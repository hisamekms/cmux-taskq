---
id: journal-012
type: journal
title: Dogfooding: one independent task
status: planned
created: 2026-09-22
updated: 2026-09-22
plan_step: 9
queue_task: null
depends_on_journal: [10]
related:
  - plan-rust-runtime-mvp
---

# 012: Dogfooding: one independent task

## Goal

[plans/current.md](../plans/current.md) ステップ9。ここからドッグフーディングに移行する。cmux-taskq自身のドキュメント改善をタスクとして登録し、固定したビルド済みバイナリで実行して成果をmainへ取り込む。

- runtimeバイナリはrepo外にコピーして使い、作業成果で置き換えない。
- キューDBはステップ5の配置（ユーザーDIR配下、cwdから解決）を使う。
- SVは常駐のClaude Code sessionで、`read-screen`で完了を確認し、差分をレビューして`integrate`を呼ぶ。
- openなジャーナル（011、013、014、019）を`cmux-taskq add`へ登録し、IDを`queue_task`に書き戻す。

完了条件: 登録から実行、receipt検証、workspace終了、差分と証跡のレビュー、`integrate`によるrebase・再検証・squash着地と`completed`まで、DBの手修正なしで通る。手順をこのジャーナルに記録する。

## Log

## Result

## Promoted
