---
id: design-supervisor-lifecycle-events-watch
type: design
title: "`events` / `watch`"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-domain-model
---

# `events` / `watch`

`dagq events --after <id> [--limit N（既定100）] [--all]`はidより後のrun_eventsを古い順に返す純粋なクエリで、既定はattentionイベントだけ、`--all`で全kind。返り値は`{events, cursor}`で、`cursor`はlimitに達したら最後に返したイベントのid、そうでなければ読んだ時点の最新id。各イベントは圧縮形で、`id`、`kind`、`task_id`/`goal_id`/`run_id`（あるものだけ）、`created_at`と、payloadから`status`（なければ`to`）、`exit_code`、`ask_id`、`code`（理由の分類コード。[domain-model](../domain-model.md#理由の分類コードcode)）、`reason`（`reason`/`message`/`error`のどれか、300文字で切り詰め）だけを取り、path・receipt・出力は省く。attentionイベントは`next`も持つ。

attentionイベントの判定は`domain::event_attention(kind, payload)`（候補kindは`ATTENTION_KINDS`）: `validation_finished`は`awaiting_integration`（supervisorがreviewする。ADR-0027）でも`failed`（supervisorがtriageする。ADR-0044の決定3）でもattentionにしない、`review_failed`（`review by hand`。payloadに`ask_id`があれば、同じstepで開いた`approve_landing`のaskがattentionなのでattentionにしない。task 328）、`triage_failed`（`triage by hand`）、`recovery_failed`（`recover by hand`。生きているsessionの復旧jobの失敗）、triageの`decide`の`ask_answered`はpayloadの`runtime_delivers`（`answer`の時点でrunが`failed` / `interrupted`で回答がaskのoptions（`retry` / `resume` / `cancel`とjobが足したもの）のどれか）が`true`ならattentionにしない、`approve_landing`の`ask_answered`はpayloadの`runtime_delivers`（`answer`の時点でrunが`awaiting_integration`で回答が`land` / `send_back` / `cancel`のどれか）が`true`ならattentionにしない、`supervision_finished`と`integration_failed`で`failed`もtriageに回るのでattentionにしない、`integration_deferred`と`integration_error`で`needs_session`もattentionにしない（supervisorがresumeし、試行を使い切れば`decide`のaskにする。task 100）、`push_failed`（`push main`）、`run_env_program_missing`（queueの行。`install tool`。`run_env_program_found`はattentionではない。ADR-0049の決定9）、`runtime_error`でpayloadの`lease_released`が`true`のもの（supervisorのabandon。`recover run`）、`prompt_waiting`と`prompt_cleared`はattentionではない（`answer_prompt`のaskがattention。task 100）、`ask_opened`（payloadの`ask_id`で`answer ask <id>`）、`ask_answered`（`read the answer of ask <id> and close it`。payloadの`runtime_closed`が`true`（sessionが終わってruntimeが閉じた`stuck_exit`）はattentionにしない。payloadの`kind`が`worker_question`なら、`runtime_delivers: true`はattentionにせず、`false`は`send the answer of ask <id> to the worker and close it`）、`ask_delivery_failed`（`send the answer of ask <id> to the worker and close it`）、`resume_finished`で`status`が`awaiting_integration`（未承認のrunが戻った。`review and integrate`。`failed`はtriageに回るのでattentionにしない）。`needs_session`で`exhausted: true`の`resume_finished`もattentionではない（task 100）。leaseを手放さない`runtime_error`（`record_runtime_error`など、`lease_released`が無いかfalse）はattentionにしない。`push_finished`と`push_skipped`もattentionにしない。validation後の後始末（workspaceのclose）の失敗によるabandonは、runが`awaiting_integration`に着いた後でも`lease_released: true`の`runtime_error`を書くので、`events` / `watch`は`recover run`を返すが、`status`はstatusどおり`review and integrate`を出す（`status`の判定を正とする）。`integration_error`で`awaiting_integration`に戻ったものは`integrate`の呼び手がerrorを受け取っているのでattentionにしない。`integration_rebase_aborted`はstatusを変えず着地が続くので、その結果（`integration_deferred`など）の方がattentionになる。既存のkind名とpayloadは変えていない（`integration_deferred`に`resumes_left`を足しただけ）。

task 293（ADR-0044の決定22）で、`events`に`--full`と絞り込みを足した。`--full`は各イベントを`id`、`kind`、`task_id`、`goal_id`、`run_id`（無ければnull）、`payload`（切り詰めない）、`created_at`の全フィールドで返す。絞り込みは`--run RUN`、`--task ID`、`--goal ID`（goalのイベントと、そのgoalのtaskとrunのイベント）、`--kind KIND`（繰り返し可）、`--since` / `--until`（UTCの`YYYY-MM-DD`（その日の0時）か`YYYY-MM-DDTHH:MM:SS[.fff][Z]`。桁と範囲を検査し、合わない値はerror。`since <= created_at < until`）で、どれも組み合わせられ、`--after` / `--limit` / cursorの意味は変わらない。`--kind`を渡すと既定のattentionだけの絞り込みは外れ、そのkindをattentionかどうかに関わらず返す。`--kind`も`--all`も無いときは、`--run` / `--task` / `--goal` / 時刻の絞り込みもattentionのイベントの中で効く（全kindを読むには`--all`を付ける）。条件は`EventFilter`（`src/domain/views.rs`）で、`SqliteQueue::events_between`が1つのSQLで絞る。

`dagq watch [--after <id>] [--timeout SECS（既定600）] [--interval SECS（既定2）] [--role <inbox|planner>]`はqueueをinterval秒ごとに読み、idより後にattentionイベントが1件以上あるか、登録済みsupervisorの健全性（tokenの集合と各`pid`・`alive`・`stale`、`domain::SupervisorPulse`）がwatch開始時のsnapshotと変わるまでblockする。返り値は`{events, supervisors_changed, supervisors, cursor}`で、`supervisors`は`status`と同じ形。timeoutでは`events`が空、`supervisors_changed: false`、`cursor`は渡したままで、exit codeは0。`--after`を省くと開始時の最新idから待つ。`watch`はqueueを読むだけで何も書かず、`integrate`を呼ばない。

`--role`を渡すと、そのroleに宛てたattentionイベント（`domain::ATTENTION_ROLE`、`status --role`と同じ）だけで起き、supervisorの健全性の変化で起きるのも同じrole（`inbox`）だけ（plannerは`events`が常に空で`supervisors_changed`が常にfalse）。inboxは`watch --role inbox`で`ask_opened`、`ask_answered`、ADR-0016のattention、supervisorの停止を受ける（ADR-0044の決定6）。cursorはrun_eventsのidのままで、askの登録と回答も`ask_opened` / `ask_answered`としてrun_eventsに書かれるのでcursorに乗る（roleの違うwatchが同じcursorを使ってよい）。`events`には`--role`は無い。
