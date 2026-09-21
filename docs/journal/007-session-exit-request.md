---
id: journal-007
type: journal
title: Session exit request after receipt
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
  - journal-001
  - design-supervisor-lifecycle
---

# 007: Session exit request after receipt

## Goal

[plans/current.md](../plans/current.md) ステップ4。receipt受領後、operatorの手動`/exit`に頼らずにセッション終了を要求する。

- receipt受領後にClaudeが応答完了・idleであることを画面文言以外の根拠（hookまたはプロセス状態）で確認する方法を決める。
- 終了要求を送り、wrapperの終了コードで終了を確認する。応答しない場合は強制終了せず、runを保持して人に知らせる。
- 手動`/exit`の経路は残す。

完了条件: 使い捨てrepositoryで、receipt提出から人の操作なしに`session_exited`まで進み、idle判定の根拠がイベントに記録される。

## Log

## Result

## Promoted
