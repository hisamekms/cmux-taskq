---
id: design-supervisor-lifecycle-ask
type: design
title: "`ask` / `answer` / `asks`"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0022
  - adr-0047
  - design-persistence
---

# `ask` / `answer` / `asks`

[ADR-0022](../../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定1。人の判断を要する相談を`asks`表の行にする（[persistence](../persistence.md#asks)）。

- askのkindにはほかに`stuck_exit`（schema v17）があり、CLIからは作れない。`follow_up`（schema v22）は退役したfollow-up triageのaskで、もう作られない（残ったものは人が読む）。`planner_question`（schema v28）はruntimeのplannerが`ask --kind planner_question --task ID`で作り、supervisorがanswerをそのplannerに届ける（[Draft planners (supervisor)](draft-planners.md#draft-planners-supervisor)）。`/exit`のtimeoutでsupervisorが作り、sessionの終了でsupervisorが閉じる（[Receipt and session exit](receipt-and-session-exit.md#receipt-and-session-exit)）。
- `dagq ask --kind <approve_landing|answer_prompt|decide|worker_question|planner_question|blocked> --because <scope|discard|recovery_failed> --question <text> [--option <text>]... [--task ID | --run RUN_ID]`は相談を登録してaskを返す（`Ask`の全列と`created`）。`--run`はそのrunのtaskも決める。`--task` / `--run`のどちらかは必須で、例外は`blocked`（observerが上げる閾値超え。ADR-0044の決定4）だけ: taskにもrunにも紐づかない閾値（`idle_slots`、`backend_failures`）は`task_id` / `run_id`の無いaskにし、その`ask_opened` / `ask_answered`もtaskの無いrun_eventsになる（schema v16）。`--finding ID`（`blocked`だけ、schema v30、ADR-0044の決定23）はaskを根拠のfindingに紐づけ、observerのblocked askには必須にする。同じ（task、run、kind、finding）でopenなaskがあれば登録せずにそれを`created: false`で返す（`--question`と`--option`は捨てる。taskの無い`blocked`はfindingごとに1件）。新しいaskは`ask_opened`（payloadは`ask_id`、`kind`、`asked_by`）を同じトランザクションで書き、commitの後にinboxへ`cmux notify`を1回送る（observerの`blocked`も同じ。出力に`notified`、失敗なら`notify_error`。[人への通知](cmux-notify.md#人への通知cmux-notify)）。`asked_by`はsessionの`DAGQ_ROLE`（無ければnoteの`by`と同じく`human`）。observer（`DAGQ_ROLE=observer`）は読み取りの`asks`と`status`と、`ask --kind blocked`だけを使え、それ以外の`ask`・`answer` / `ask close`は拒否される。
- **人が要る理由**（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定41、task 361、schema v29）: askは`reason_category`（`authentication` / `cost` / `scope` / `discard` / `recovery_failed`）を必ず持つ。CLIの`ask`は`--because <reason>`が必須で、無いか一覧に無い値、`authentication` / `cost`（下のqueue_holdだけのもの）は拒否する。当てはまらない相談はaskにせず、noteかreceiptに書く。runtimeが作るaskはkindごとに決まった理由を書く: `approve_landing` / `approve_plan`は`scope`、`decide`（triageとresumeの使い切り）・`stuck_exit`・`answer_prompt`・`stalled`は`recovery_failed`。`worker_question` / `planner_question` / `blocked`は作る者が`--because`で付ける。v29のmigrationは既存のaskをkindから埋める（`approve_landing` / `approve_plan` / `planner_question` / `worker_question` / `blocked` / `follow_up`は`scope`、ほかは`recovery_failed`）。`ask_opened` / `ask_answered`のpayload、`status`の`asks`とask由来のattention、`events` / `watch`のcompactな行に`reason_category`が載る。
- **認証のaskは1件**（ADR-0047の決定42）: kind `queue_hold`のaskはtaskにもrunにも紐づかず、理由（`authentication` / `cost`）と`subject`ごとにopenなものが1件だけで、止めているrunを`affected`（run IDの配列）に持つ。supervisorはworkerの画面にログインの切れ（最後の行のどれかが、枠と`⎿`を除いて`API Error: 401` / `Invalid API key` / `OAuth token has expired` / `OAuth token revoked`で始まり`/login`を含む。文言を含むだけの作業の出力は数えない。`infrastructure::claude::auth_required`）を見つけると、dialogの確認（`watch_prompt`）でも受領の無いidleの判定（`StallWatch`、促しとstalledのaskの前）でも、そのrunをこのaskに足す（無ければ開く。optionsは`done` / `cancel_affected`、通知は開いたときの1回だけ）。足したrunには`auth_required`（`workspace_id`、`excerpt`、`screen_hash`、`ask_id`）を、2件目以降のrunには`ask_updated`（`ask_id`、`reason_category`、`affected`）をそのrunに書き、questionの末尾の`Affected runs:`を書き直す。askにいるrunは促しもstalledのaskも受けない。ログインの後も画面にはエラーが残るので、直近の`auth_required`と同じ`screen_hash`の画面で、そのaskがもうopenでなければ上げ直さず、stallの判定がそのまま促しで続きを頼む。未実装（後続task）: openな間のclaimとjobの控え、`done` / `cancel_affected`のruntimeによる適用、利用上限とディスクの検知。
- `dagq answer ASK_ID --text <text>`はopenなaskに`answer`と`answered_at`を書き、`ask_opened`と同じtask / runに`ask_answered`（`ask_id`、`kind`）を書く。回答済みかcloseされたaskにはerror。task 325（schema v38）から、回答した者`answered_by`（sessionの`DAGQ_ROLE`、無ければ`person`）と、回答が`options`のどれかと（前後の空白を除いて）一致したときのその番号`option_index`（一致しなければnull）も行に書き、`ask_answered`のpayloadに`answered_by` / `option_index`と選んだ選択肢の文字列`option`を足す。runtimeが自分で閉じる回答（`runtime_closed: true`の`ask_answered`と、supervisorの取り下げ）は`answered_by: runtime`。出力の`Ask`には`answered_by` / `option_index`が常に載り、未回答とv38より前の回答ではnull。集計は[`stats`](stats.md)の`asks`。
- `dagq ask close ASK_ID`は回答済みのaskに`closed_at`を書き、`ask_closed`（`ask_id`、`kind`）を記録する。inboxが回答を読んで従った印（supervisorが答えを適用して閉じるときも同じ`close_ask`）で、`stats`の`asks.times`は回答の適用として読む（task 468）。attentionではない。未回答のaskはcloseできず（error）、取り下げは`answer`で取り下げた旨を書いてからcloseする。run_eventsでaskを終えるのは`ask_answered`だけで（`worker_question`の送信の`ask_delivered` / `ask_delivery_failed`は後から足したkindで、`stats`の対には使わない）、`stats`はこの2つを`ask_id`で対にして未回答のaskを数えるため、closeだけで閉じたaskが`stats`に残り続けないようにする。回答した時点で同じ（task、run、kind）の新しいaskを登録できる。
- `dagq asks [--open] [--role <role>] [--all]`はaskを古い順に`{asks}`で返す。既定はcloseされていないもの、`--all`はcloseされたものも、`--open`は未回答のものだけ、`--role`はそのroleが今動かすもの（`Ask::waits_for`: 未回答も回答済みでcloseされていないものもinbox、plannerは無し）。
