---
id: design-supervisor-lifecycle-abandon
type: design
title: "1 runの異常（abandon）"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0025
---

# 1 runの異常（abandon）

wrapperのプロセスも死んでいるwrapper heartbeat切れ（プロセスが生きていれば手放さない。[wrapperが黙ったsession](silent-wrapper.md#wrapperが黙ったsession)）、検証処理そのもの（Git呼び出しやDB）の失敗、closeの記録失敗など、監視中のruntime errorは**そのrunだけ**を手放す: `last_error`と`runtime_error`イベント（`lease_released: true`）を書き、そのrunのlease行を削除し、status・`run_processes`・workspace・worktreeは変えない。supervisorは他のrunを続け、結果の`errors`にそのrunを載せる。leaseを消すのは、常駐supervisorが生きている間も`recover`がrunのprocessだけで判定できるようにするため。taskは未完了runで占有されたままなので二重実行にはならず、未登録のwrapperはleaseがなければ登録できずClaudeを起動しない。leaseがないので他のsupervisorも引き継がない。`status` / `watch`はこのrunを`recover run`のattentionとして出す（[ADR-0025](../../adr/0025-leaseless-unfinished-run-is-a-recover-run-attention.md)。`kind`は`runtime_error`）。sessionのprocessが残っていなければ（未登録、`exited_at`が記録済み、PIDが死んでいる）、次のfill passでsupervisor自身がrecoverして`interrupted`にし、triageに回す（ADR-0044の決定3。[Triage (supervisor)](triage.md#triage-supervisor)）。processが生きている間は`recover run`のまま人を待つ。人は`dagq-recover` skillに従い、`show`と`doctor`で確認して[recover](recover.md#recover-run_id)で扱う（supervisorが居ないときだけ。居ればprocessが止まった時点でsupervisorが行う）。recoverすればrunは`interrupted`（`integrating`だったものは`awaiting_integration`）になりattentionから消える。wrapperの登録を待つ時間は`WorkspaceBackend::registration_timeout`（既定45秒）。終了要求のtimeout（`exit_request_timed_out`）ではabandonせず、leaseを持ったままsessionの終了を待つ（[Receipt and session exit](receipt-and-session-exit.md#receipt-and-session-exit)）。
