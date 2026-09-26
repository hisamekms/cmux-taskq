---
id: design-supervisor-lifecycle-cleanup-and-recovery
type: design
title: "Cleanup and recovery"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0007
  - design-manual-smoke
---

# Cleanup and recovery

workspaceの終了はsupervisorが行う。leaseはrun単位で（[ADR-0007](../../adr/0007-run-level-leases-parallel-execution.md)）、1 runの復旧が他のrunに影響しない。receipt検証を通った`awaiting_integration`のrunだけが対象で、workspaceだけを閉じ、worktreeとbranchは`integrate`が着地するまで残す（着地後に`integrate`が削除する）。`failed`（非0終了、検証拒否）、provisioningや検証処理のエラー、wrapper heartbeat切れの場合はworkspaceもworktreeも調査のため残し、closeを呼ばない（worktreeのビルド成果物だけは消す。[Run worktrees](run-worktrees.md#run-worktrees)）。

cmux 0.64.25の`workspace create --command`はコマンドをログインシェルに打ち込む形で起動し、wrapperが終了してもシェルとworkspaceは残る（2026-09-22の故障経路のスモーク（[manual-smoke](../manual-smoke.md#観測済みの環境依存)）で、cmux 0.64.25 (106)で`--command true` / `sleep 1`のworkspaceが5秒以上残ることを確認した。同じ日に同じ版の別の環境では、コマンド終了の1〜2秒後にworkspaceが自動で閉じ、supervisorのcloseのほうが先に成功していた。cmuxの設定に依存するとみられる）。どちらの場合もsupervisorの手順は同じで、先にworkspaceが消えていれば`cmux workspace close`は`not_found`で失敗して`cleanup_failed`になり、wrapper終了直後の`read-screen`も`screen_capture_failed`になりうる。`failed`・`interrupted`のrunのworkspaceはtriageの後にsupervisorが閉じ（[Triage](triage.md#triage-supervisor)）、triageが失敗したrunだけは調査が済んだら人が`show`の`workspace_id`を`cmux workspace close`に渡して閉じる（`doctor`は未完了runしか列挙しないので`failed`・`interrupted`のrunは出ない）。人が`ready` / cancelした後、そのrunはtriageの対象でなくなり、次の掃除（下）が閉じる。
