---
id: journal-008
type: journal
title: Integration confirmation and task completion
status: planned
created: 2026-09-22
updated: 2026-09-22
plan_step: 4
queue_task: null
depends_on_journal: [5]
verify:
  - cargo fmt --all --check
  - cargo test --locked
  - cargo clippy --locked --all-targets -- -D warnings
related:
  - design-domain-model
  - design-persistence
---

# 008: Integration confirmation and task completion

## Goal

[plans/current.md](../plans/current.md) ステップ4。手動mergeの後、成果commitがmainに含まれることを確認してTaskを`completed`にし、依存taskを解放する。

- `integrate ID`（名称は実装時に決める）コマンドを追加し、`awaiting_integration`のrunのresult commitが`refs/heads/main`の祖先であることをGitで確認する。
- 確認できたらrunを`integrated`、Taskを`completed`にし、イベントを記録する。merge/fast-forwardのみ対象とし、squash/cherry-pickは後回し。
- 確認できない場合は状態を変えない。

完了条件: mergeしたmainで`completed`になり依存taskが`candidates`に現れること、merge前やsquash後は`completed`にならないことをテストで確認できる。

## Log

## Result

## Promoted
