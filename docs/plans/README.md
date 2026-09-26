---
id: plans-index
type: design
title: Implementation plans
status: current
created: 2026-09-22
updated: 2026-09-26
last_verified: 2026-09-26
tags:
  - planning
---

# Implementation plans

計画文書は、実装の順序と完了条件を記録する。現在進行中の計画は `active`、完了した計画は `completed` に更新し、履歴として残す。個々のタスクの経過と状態はdagqのキュー（`dagq show ID`のrun履歴とreceipt）が正である。

- [Current plan](current.md)
- [Milestones](milestones.md)
- [ADR 0001〜0034の棚卸しと統合ADRの組](adr-inventory.md)
- [cargo llvm-cov nextestへの切り替え前後のintegrateのverifyの所要時間と遅いtest](nextest-measurement.md)（ADR-0076決定6の測定）
