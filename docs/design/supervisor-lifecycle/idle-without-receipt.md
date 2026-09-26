---
id: design-supervisor-lifecycle-idle-without-receipt
type: design
title: "receiptの無いidleの検知"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0043
---

# receiptの無いidleの検知

[ADR-0043](../../adr/0043-detect-stalled-worker-sessions-nudge-once-then-ask.md)の決定1（task 288、`application::supervise::stall`の`StallWatch`）。task 182のworkerはbackgroundの`cargo test`の通知を待ってturnを終え（`Stop` hookのidle markerは書かれた）、receiptの無いまま10.5時間eventが0件だった。[ダイアログ待ちの検知](prompt-waiting.md#ダイアログ待ちの検知)はidle markerのあるrunを見ないので、この型は別に検知する。

- **対象**: 最初のsession（`SessionWatch`）だけ。resumeしたsessionは解消依頼の後にreceiptの無いままidleになれば`/exit`を送って試行を終え（`went idle without a resolving receipt`）、reviseと衝突の依頼を送ったsessionは`approve_landing`のaskにする（`went idle without rewriting the receipt`）ので、receiptの無いidleで止まり続けない。backgroundの処理が`running`のままのidleはどちらも`resume_timeout`（1時間）で終わる。`ExitWatch`（verdictの後）とreceiptの後のbackgroundの処理（Bの型）は対象外。
- **条件**（wrapperのheartbeatが有効で`/exit`を要求しておらず、receiptの無いpollごと）: idle markerがあり、そのmtimeがsupervisorが最後にsessionへ打った文（workerへの回答、促し）の時刻より新しく（sessionが入力を処理し終えてturnを閉じている）、記録済みで解消していない`prompt_waiting`も、closeされていない`worker_question` / `answer_prompt`のaskも無いまま、markerのmtime（`wait`の答えの後はその時刻との遅い方）から`[stall].idle_without_receipt_secs`（既定1200秒）以上経った。markerの`background_tasks`に`running`の処理があってもidleに数える。時刻はidle markerと同じくfileの時計（`RunFiles::now`）で比べる。
- **促し**（1回だけ）: `stall_nudged`（`phase: session`、`idle_secs`、`threshold_secs`、`background_running`、`background_tasks`（`id` / `description` / `command`）、`workspace_id`）を記録してから、定型の文（`prompt::stall_nudge`。run id、receiptの無いidleの分数、`running`のbackgroundの処理、「終わったならcommitしてreceiptを書く」「判断が要るなら`dagq ask --run <run-id> --kind worker_question`で聞いて止まる」「backgroundの処理を待っているなら何を待っているか、いつ終わる見込みか、戻らなければどうするかを書いて作業を続ける」）を[送信と確認](session-send.md#sessionへの送信と確認)の`submit`で打ち、回答と同じ`StartCheck`で作業の兆候を見る（送った文の確認の既定の境界。ADR-0043の決定2のEnterの送り直しは後続task）。打てなければlogに書き、次のpollでaskに進む。
- **ask**: 促しの後にsessionが応答して（促しより新しいidle markerを書いて）なお上の条件を`idle_without_receipt_secs`満たしたら、inbox宛ての`kind: stalled`のaskを開く（`asked_by: supervisor`、taskとrunに紐づく、optionsは`wait` / `intervene`）。questionは人に宛てて、run id・task id・workspace・`reason: idle_without_receipt`と`phase`・receiptの無いidleの秒数・促してからの秒数・markerの`running`のbackgroundの処理（`description` / `id` / `command`、無ければ無いこと）・`WorkspaceBackend::capture`で読んだ画面の末尾15行（`screen_excerpt`。読めなければその旨）と、2つの選択肢の意味を持つ。同じ（run、kind）のopenなaskがあれば作らず、通知は新しいaskのときだけ1回inboxへ送る。促しが打たれていない（入力欄に残った）ならmarkerが新しくならないのでaskは開かず、`StartCheck`の`answer_prompt`のaskが扱う。
- **answer**: `wait`は、supervisorがaskをcloseし（attentionは`applying the answer of ask <id> (runtime)`）、計時をその時刻からやり直す。なお`idle_without_receipt_secs`止まっていれば新しい`stalled`のaskを開く（促しは送り直さない）。それ以外の答え（`intervene`）は人がsessionに手を入れる印で、askは回答済みのまま残り（`read the answer of ask <id> and close it`。inboxが`dagq-recover` skillの手順で画面を読み、backgroundの処理を止めるか指示を打つかrunを止めて`recover`する）、sessionが次にturnを閉じるまで新しいaskを作らない。supervisorが適用する前に誰かがaskをcloseしたときは、その答えが`wait`なら`wait`として、それ以外（答えの無いcloseを含む）は`intervene`として扱う。
- **閉じる**: askが開いている間にsessionが自分でturnを閉じた（askより新しいidle marker）、receiptが来た、`worker_question`が開いた、のどれかでsupervisorはそのrunのcloseされていない`stalled`のaskを`the session moved on; closed by the runtime`で閉じ、sessionが終わったら`the session exited; closed by the runtime`で閉じる（未回答なら答えを書いて`ask_answered`（`runtime_closed: true`、attentionではない）、回答済みならcloseだけ。`close_stalled_asks`）。triageも`the run was triaged; closed by the runtime`で閉じる。`recover`はaskを閉じない（`stuck_exit`と同じ）。
- **結末の記録**（ADR-0043の決定3）: 促しとaskのそれぞれに`stall_resolved`を1回記録する。payloadは`phase`、`detection`（`nudge` / `ask`）、`threshold`（`idle_without_receipt_secs`）、`threshold_secs`、`detected_after_secs`（検知したときのidleの秒数）、`outcome`、`resolved_after_secs`（検知から結末まで）、askなら`ask_id`。`outcome`は、促しなら`resolved_by_nudge`（その後receiptか`worker_question`）/ `escalated`（`stalled`のaskに進んだ）/ `run_ended`（sessionが終わった）、askなら`answered_wait` / `answered_intervene`（回答前に人がcloseしたものを含む）/ `resolved_by_itself`（未回答のうちにsessionが動いた）/ `run_ended`。`stall_preempted`（閾値を超える前の人の入力）は`prompt-submit.json`のmarkerを読む送信の確認の後続taskで入る。検知ごとの結末は`stats`の`stall_thresholds`が閾値ごとに集計する（[`stats`](stats.md#stats)、task 297）。
- **限界**: 最後の入力として数えるのはsupervisorが打った文だけで、人が打った入力や、backgroundの処理の完了でClaude Codeが自分で始めたturnは知らない。そのturnが`idle_without_receipt_secs`より長く続くと、作業中のsessionに促しが届きうる。`prompt-submit.json`（`UserPromptSubmit` hook）を入力の印に数える送信の確認の後続taskで解消する。
- **引き継ぎ**: adoptしたsupervisorは、最後の`stalled`のaskがcloseされていてその`stall_resolved`が無ければ（supervisorの居ない間に閉じられた）、答えが`wait`ならそのcloseの時刻を計時の起点、それ以外は人が手を入れた時刻として読み、run_eventsの`phase: session`の最新の`stall_nudged`を促し済み（その時刻を最後に打った文の時刻とする）、そのrunのcloseされていない`stalled`のaskを開いたask（時刻は`created_at`の次の秒から数える）、そのaskの`stall_resolved`があれば答えを適用済み、`answered_wait`の`stall_resolved`の時刻を計時のやり直しの起点、`answered_intervene`の`stall_resolved`の時刻を人が手を入れた時刻、最新の`ask_delivered`の時刻を最後に打った文の時刻として読み、促しもaskも2回出さない。
