---
id: design-supervisor-lifecycle-notification-route
type: design
title: "人への通知経路（ADR-0016で決定、ADR-0022とADR-0024で改めた）"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0022
  - adr-0025
  - design-domain-model
---

# 人への通知経路（ADR-0016で決定、ADR-0022とADR-0024で改めた）

ADR-0016で次を決めた。`status`のattentionとcursor、`events --after`、`watch`は実装済み（[`status`](status.md#status)、[`events` / `watch`](events-watch.md#events--watch)）。`review`は実装済み（下記「`review`」）。`cmux notify`は[ADR-0022](../../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)で送る条件と宛先が改められ、`dagq ask`が新しいaskのときにinboxへ送る形で実装済み（[人への通知](cmux-notify.md#人への通知cmux-notify)）。圧縮出力と`--full`はtask 59、短い初期promptとpluginの`SessionStart` hookはtask 65で実装済み。ADR-0016の受け手だった常駐sessionはADR-0024で退役し、受け手はinboxになった（task 100）。

- inboxとplannerは状態を持たない使い捨てのsessionで、compaction・`/clear`・再起動からの起き直しは`status`の1コマンドで行う。`status`はsupervisorの健全性、未完了run、attention、次のcursorを上限のある大きさで返す。
- `watch --after <cursor>`はcursorより後のattentionイベントかsupervisor健全性の変化までblockし、attentionと新しいcursorを返して終わる。inboxはこれをbackgroundで走らせて終了で起きる。`doctor`は診断専用で、pollingには使わない。
- attentionはrun_eventsのkind（公開契約。既存のkind名とpayloadは変えず追加だけ）からdomainが判定する: runの`awaiting_integration`・`needs_session`・`failed`（のちにtriageに回るものはattentionでなくなった）、`exit_request_timed_out`（task 104からは`stuck_exit`のaskに乗せ替え）、`prompt_waiting`（ADR-0019。task 100からは`answer_prompt`のaskに乗せ替え）、supervisorの停止/stale（のちに[ADR-0025](../../adr/0025-leaseless-unfinished-run-is-a-recover-run-attention.md)でsupervisorが手放した未完了runの`recover run`が加わった）。supervisorの状態はrun_eventsに載せず`supervisors`表から導出し、schemaは変えない。
- runtimeは人のsessionのterminalに`cmux send`で打ち込まない（workerへの`/exit`と回答の送信は従来どおり）。`integrate`は`watch`からもイベントの副作用としても呼ばない。
- 人の経路のコマンドは既定で圧縮し（既存キー名を変えずに省く・切り詰める）、全文は`--full`。`show`・`goal show`・`doctor`は実装済み（`doctor`は上、`show`と`goal show`は[domain-model](../domain-model.md)）。手でのレビューは`review ID`が`<run_dir>/review.md`を書き、subagentにpathを渡す（`review`は実装済み。下記「`review`」）。
