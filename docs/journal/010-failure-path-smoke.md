---
id: journal-010
type: journal
title: Failure path smoke on a disposable repository
status: planned
created: 2026-09-22
updated: 2026-09-22
plan_step: 4
queue_task: null
depends_on_journal: [6, 9, 15]
related:
  - journal-003
---

# 010: Failure path smoke on a disposable repository

## Goal

[plans/current.md](../plans/current.md) ステップ4。ステップ4の完了条件を実機で確認する。使い捨てrepositoryで以下を起こし、二重起動や成果の喪失がないことを確認する。

- Claude異常終了（wrapperの子プロセスをkill）
- supervisor再起動（supervisorをkillして`doctor`→`recover`→再実行）
- 検証コマンド失敗
- cleanup失敗（workspaceを先に閉じておく）

完了条件: 4シナリオそれぞれの観測結果と、DBの状態・保持されたリソースをこのジャーナルに記録し、plans/current.mdのステップ4を完了にできる。

## Log

## Result

## Promoted
