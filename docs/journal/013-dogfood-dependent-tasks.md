---
id: journal-013
type: journal
title: Dogfooding: dependent tasks A then B
status: planned
created: 2026-09-22
updated: 2026-09-22
plan_step: 6
queue_task: null
depends_on_journal: [12]
related:
  - plan-rust-runtime-mvp
---

# 013: Dogfooding: dependent tasks A then B

## Goal

[plans/current.md](../plans/current.md) ステップ6。A → Bの依存taskを登録し、依存解放が統合確認に紐づくことを確認する。

完了条件: Aの実行成功（`awaiting_integration`）だけではBが始まらず、Aの統合確認後にBがAの変更を含むmainから作られたworktreeで始まる。

## Log

## Result

## Promoted
