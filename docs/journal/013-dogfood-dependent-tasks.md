---
id: journal-013
type: journal
title: Dogfooding: dependent tasks A then B
status: planned
created: 2026-09-22
updated: 2026-09-22
plan_step: 9
queue_task: null
depends_on_journal: [11]
related:
  - plan-rust-runtime-mvp
---

# 013: Dogfooding: dependent tasks A then B

## Goal

[plans/current.md](../plans/current.md) ステップ9。A → Bの依存taskと、依存のないCを登録し、依存解放が着地に紐づくこと、依存のないtask同士は並列に走ることを確認する。

完了条件: AとCが同時に走り、Aの実行成功（`awaiting_integration`）だけではBが始まらず、Aの着地後にBがAの変更を含むmainから作られたworktreeで始まる。

## Log

## Result

## Promoted
