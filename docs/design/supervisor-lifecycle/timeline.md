---
id: design-supervisor-lifecycle-timeline
type: design
title: "`timeline`"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
---

# `timeline`

`dagq timeline RUN [--gap SECS（既定300）] [--full]`（task 293、ADR-0044の決定22）は、runのイベントを古い順に並べ、隣り合うイベントの間が`--gap`秒以上の区間（空白）ごとに理由を付けて返す読むだけのコマンド。返り値は`{run_id, task_id, status, gap_secs, events, gaps, gap_total_secs}`で、`events`は`events`と同じ圧縮形（`--full`で全フィールド）。runがin_progressのtaskの最新のrun（triageを待つ`failed` / `interrupted`も含む）なら、最後のイベントから今までも空白にする（`before_event`と`until`がnull）。各空白は`after_event`、`before_event`、`from`、`until`、`secs`、`reason`と、あれば`phase`（supervisorが見ていたsession: `session` / `resume` / `revise` / `conflict`）、`confirmed`（`idle`のとき）、`ask_ids`（`waiting_ask`のとき）。

理由はrunのイベントだけから決まった規則で導く（`domain::timeline::gaps`。LLMもrunのディレクトリも読まない）。空白の始まりのイベントまでの状態で、上から最初に当たるもの:

- `no_supervisor`: 空白が`run_adopted`か`run_recovered`で終わる（別のsupervisorが引き継ぐまで誰もrunを持っていなかった）
- `waiting_ask`: runを止めるaskが開いている（`ask_opened`に対の`ask_answered`が無い。observerの`blocked`とplannerの`planner_question`は数えない）
- `integrating`: `integration_started`の後で、着地の結果（`run_integrated`、`integration_deferred` / `integration_error` / `integration_failed`、`runtime_error`）も引き継ぎ（`run_adopted` / `run_recovered`）も`resume_started`もまだ無い（rebaseと検証の実行中）。ADR-0044の決定22の一覧に無い理由で、着地の検証の長さを`unknown`と分けるために足した
- supervisorが見ているsession（`agent_started`、`resume_started`、`revise_requested`、`requested: true`の`conflict_precheck`から`session_exited`まで）の中: 最後のidle markerの記録（`session_idle_observed`か、markerを読んだ`stall_nudged`）が`background_running: true`なら`background`、そのsessionのreceipt（reviseとconflictでは`revise_finished` / `conflict_resolved`）が観測済みなら`after_receipt`、それ以外は`idle`。`session_idle_observed`はreceiptの後にしか記録されないので、receiptの前の`background`は`stall_nudged`から分かる。`resume_started`は前のreceiptと受理を忘れる（resumeしたsessionが新しいreceiptを書く）。`idle`の`confirmed`は、そのsessionで`session_idle_observed`か`stall_nudged`が記録されていればtrue。イベントだけではidleと作業中を分けられないので、receiptの前の長い空白（task 182の型）は`confirmed: false`の`idle`になる
- `waiting_integration`: sessionが終わっていて、受理された`validation_finished`がある（着地の順番待ち）
- `after_receipt`: sessionが終わっていて、receiptは観測済み（validationやreviewの待ち）
- どれにも当たらなければ`unknown`
