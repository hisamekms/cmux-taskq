---
id: journal-017
type: journal
title: Run dependency-free tasks in parallel
status: planned
created: 2026-09-22
updated: 2026-09-22
plan_step: 6
queue_task: null
depends_on_journal: [16]
verify:
  - cargo fmt --all --check
  - cargo test --locked
  - cargo clippy --locked --all-targets -- -D warnings
related:
  - plan-rust-runtime-mvp
  - design-supervisor-lifecycle
  - design-persistence
---

# 017: Run dependency-free tasks in parallel

## Goal

[plans/current.md](../plans/current.md) ステップ6。1つのqueueで、依存が解けたtaskを上限まで同時に実行する。

- queue全体のactive runを1件に絞る部分UNIQUE indexをやめ、Taskごとの未完了run 1件の制約だけ残す。
- leaseをqueue単位からrun単位（`task_runs`のsupervisor tokenとheartbeat）にする。`doctor`と`recover`はrunごとに判定し、1つのrunだけrecoverしても他のrunは走り続ける。
- `supervise --parallel N`（既定4）を常駐ループにする。候補を上限までclaim → 起動 → 各runのheartbeat・idle・exitを監視 → 検証、を繰り返し、`integrate`で依存が解けたtaskも拾う。1件の状態機械は現行のまま並べる。
- 1 runの異常終了・検証失敗・cleanup失敗が他のrunに波及しない。
- `plugins/claude-taskq/`のskill（実行・復旧）を`supervise --parallel`とrun単位の`doctor`/`recover`に追従させる。
- ADRを追加し、`domain-model.md`・`persistence.md`・`supervisor-lifecycle.md`・READMEを更新する。

完了条件: 依存のないtaskが同時に走り、依存のあるtaskは先行taskの`completed`まで待つこと、1 runの失敗とrecoverが他のrunに影響しないことがテストで確認できる。`tests/e2e.rs`に2件同時のハッピーパスがある。

## Log

## Result

## Promoted
