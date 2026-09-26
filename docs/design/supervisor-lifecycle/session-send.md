---
id: design-supervisor-lifecycle-session-send
type: design
title: "sessionへの送信と確認"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
---

# sessionへの送信と確認

task 285。supervisorが生きているsessionに打つもの（resumeの解消依頼、workerへの回答、reviseと衝突の依頼、receiptの書き直しの依頼、`/exit`）は、すべて`application::supervise::deliver`の`submit`を通る。2026-09-24のtask 205のrun 68a96a60では、長い解消依頼が入力欄に貼り付けられたままEnterが送信として扱われず（貼り付けの処理中のEnter）、後の`/exit`も入力欄に追記されただけで`stuck_exit`になった。画面の判定（`input_ready` / `input_pending` / `working`）は`detect_prompt`と同じく`infrastructure::claude`の純粋関数で、`AgentSignals`から呼ぶ。

1. **送信**: `send_text`（文面）または`send_exit`（`/exit`）で打つ。cmux adapterは打った後`paste_settle`（300 msと10文字ごとに1 ms、最大3秒。13 KBで約1.6秒）待ってからEnterを送る。cmuxの時間切れで届いたかが分からない送信は失敗にせず、2の確認に進む（task 326。[backendの呼び出しの失敗](backend-call-failures.md#backendの呼び出しの失敗)のtimeoutのretry）。
2. **確認**: `submit_check_interval`（既定1秒）ごとに画面を読む。ダイアログ（`detect_prompt`）が出ていればそれ以上何も送らない。入力欄に打ったものが残っていれば（`input_pending`: 入力欄の英数字が、打った文面の末尾24文字の英数字を含む、またはClaude Codeが畳んだ`[Pasted text #N ...]`がある）、`send_enter`でEnterだけを送り直す（`SUBMIT_RETRIES`、3回まで）。文面も`/exit`も打ち直さない（`/exit`の再送はダイアログの選択肢を選びうる）。送り直したら`submit_retried`（`workspace_id`、`input`: `text` / `exit`、`what`: 何を送ったか、`retries`、`submitted`）を記録する。3回送り直しても残れば`submit_unconfirmed`（`workspace_id`、`input`、`what`、`retries`、`excerpt`）を記録し、文面ならinbox宛ての`answer_prompt`のask（`ask_unsubmitted`。optionsは無く、人は送るキーか文面を答え、inboxがそのworkspaceで行う。同じrunのopenな`answer_prompt`があれば新たに作らない。sessionが終われば`the session exited; closed by the runtime`で閉じる）を開く。`/exit`は従来どおり`exit_timeout`（120秒）の後の`stuck_exit`のaskに任せる。画面が読めなければ確認をやめ、送れたものとして扱う。Enterの送り直しに失敗すれば入力欄に残ったものとして扱う。`submit`のerrorは打つこと自体の失敗だけで、打った後の記録やaskの失敗はlogに書くだけにする（回答を`ask_delivery_failed`と取り違えないため）。
3. **作業の兆候**（`StartCheck`。解消依頼・回答・revise・衝突の依頼・receiptの書き直しの依頼）: 送ってから`start_wait`（既定60秒）経ったら、idle markerが送信より新しい、画面にagentの作業中の表示（`esc to interrupt`）がある、画面が送信直後に読んだものから変わった、のどれかを兆候とする。どれも無く、ダイアログが出ていればその種類を書いた`answer_prompt`のaskを開く。入力欄が空（準備できていて文面が残っていない）なら依頼が消えたとみなし、`submit_resent`（`workspace_id`、`what`、`waited_secs`）を記録して1回だけ同じ文面を送り直す。送り直しても兆候が無い、または文面が入力欄に残っているなら`submit_not_started`（`workspace_id`、`what`、`waited_secs`、`resent`、`excerpt`）を記録して`answer_prompt`のaskを開き、以後は見ない。`resume_timeout`（1時間）まで黙って待たない。adoptしたsupervisorは引き継いだ依頼の兆候を見ない。画面の変化を兆候とするので、送信直後から変わり続ける表示（時計など）がある画面では依頼の消失を見逃し、従来どおり`resume_timeout`に頼る（保守的な側に倒す）。
4. **待ち時間の既定値**: `resume_prompt_delay` 5秒、`registration_timeout` 45秒、`submit_check_interval` 1秒、`start_wait` 60秒（いずれも`WorkspaceBackend`の既定メソッド）。追加したrun_eventsのkindは`input_not_ready`、`submit_retried`、`submit_unconfirmed`、`submit_resent`、`submit_not_started`で、どれもattentionにはしない（人に届くのは`answer_prompt`のask）。
