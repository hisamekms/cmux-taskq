---
id: design-supervisor-lifecycle-worker-question-answer
type: design
title: "workerの質問への回答の送信"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0022
---

# workerの質問への回答の送信

[ADR-0022](../../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定2。runtimeがsessionのterminalに打ち込むのは`/exit`・resumeの解消依頼と、このworkerへの回答だけ（ADR-0016の決定3の例外）。入力欄に残った文面や`/exit`を送信するためのEnterだけの送り直し（[送信と確認](session-send.md#sessionへの送信と確認)）もその一部として扱う。

- **条件**: `SessionWatch::poll`のたびに、wrapperが生きていて`/exit`をまだ要求していないrunについて、そのrunの`worker_question`のうち回答済みでcloseされていないask（`SqliteQueue::undelivered_answers`）を古い順に見る。`idle.json`のmtime（unix秒）がaskの`created_at`以上なら（backgroundの処理が`running`でもよい。打ち込みは確認画面を出さない）、workerはaskの後に応答を終えて入力を待っているとみなす（秒単位の比較なので、同じ秒の中でaskより前に書かれたmarkerも通るが、askはworkerの作業中のturnで打たれるので、その前の`Stop`が同じ秒に収まることは実際には無い）。markerが無いかaskより古ければ送らず、次のpollで見直す。
- **送信**: [送信と確認](session-send.md#sessionへの送信と確認)の経路で`answer to ask <id>: <answer>`を送り、Enterを押す（cmuxは`cmux send`で改行を空白に畳んだ1行を打ち、`paste_settle`の後に`send-key enter`）。入力欄に残ればEnterだけを送り直し、それでも残れば`submit_unconfirmed`を記録してinbox宛ての`answer_prompt`のaskを開く。送った後に作業の兆候が無ければ、解消依頼と同じく送り直すかaskにする（`StartCheck`。receiptが出た後は見ない）。成功すれば同じトランザクションでaskの`closed_at`を書き、`ask_delivered`（`ask_id`、`workspace_id`）を記録する（`SqliteQueue::ask_delivered`）。その間に誰かがcloseしていれば何も書かない。送った後の記録の失敗はlogに書くだけで、生きているrunを手放さない。
- **失敗**: 送信の失敗はrunを手放さず、`ask_delivery_failed`（`ask_id`、`workspace_id`、`error`）を記録してlogに書き、askはcloseしない。送信は1回だけで、`ask_delivery_failed`のあるaskは（引き継いだsupervisorも）再送しない。送信とcloseの間でsupervisorが死んだときだけ、引き継いだsupervisorがもう一度送りうる。
- **attention**: `answer`は`worker_question`の`ask_answered`のpayloadに`runtime_delivers`（回答した時点でrunが`running`か）を足す。`true`ならattentionイベントにしない（inboxの`watch`を起こさない）、`false`なら`send the answer of ask <id> to the worker and close it`のattentionイベント。`status`は回答済みの`worker_question`を、runが`running`でstaleでないleaseがあり送信に失敗していなければ`next: delivering the answer of ask <id> (runtime)`（何もしなくてよい）、`ask_delivery_failed`があれば`kind: ask_delivery_failed`で、runが`running`でないかleaseが無いかstaleなら（誰も送らない）`kind: ask_answered`で、どちらも`next: send the answer of ask <id> to the worker and close it`として出す。`ask_delivery_failed`はattentionイベント（inbox宛て）。`ask_delivered`はattentionではない。
- **ダイアログ待ちとの関係**: closeされていない`worker_question`を持つrunは画面を読まず（他のkindのaskはダイアログ待ちの検知を止めない）、記録済みの`prompt_waiting`は`prompt_cleared`にする。そのrunの`answer_prompt`のaskも開かない。
- **inbox**: `status`の`asks`に`worker_question`が出る。inboxが人に見せ、人の答えを`answer`で書く（それまで退役した常駐sessionがworktreeの中の判断に自分で答えていたのはやめた。task 100）。
