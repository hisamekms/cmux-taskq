---
id: journal-010
type: journal
title: Failure path smoke on a disposable repository
status: planned
created: 2026-09-22
updated: 2026-09-22
plan_step: 4
queue_task: null
depends_on_journal: [6, 9, 15, 18]
related:
  - journal-003
---

# 010: Failure path smoke on a disposable repository

## Goal

[plans/current.md](../plans/current.md) ステップ4。ステップ4の完了条件を、ステップ6の並列実行とステップ7のmerge queueを含めて実機で確認する。使い捨てrepositoryで`supervise --parallel 2`以上を動かし、以下を起こして二重起動や成果の喪失がないことを確認する。

- Claude異常終了（wrapperの子プロセスをkill）
- supervisor再起動（supervisorをkillして`doctor`→`recover`→再実行）
- 検証コマンド失敗
- cleanup失敗（workspaceを先に閉じておく）
- 並列中の1 runの異常終了とrecoverが、他のrunに影響しない
- merge queueの衝突（2 taskで同じ行を変える）を`needs_session`からresumeで解消して着地する

完了条件: 6シナリオそれぞれの観測結果と、DBの状態・保持されたリソースをこのジャーナルに記録し、plans/current.mdのステップ4を完了にできる。

## Log

## Result

## Promoted
