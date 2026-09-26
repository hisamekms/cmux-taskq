---
id: design-supervisor-lifecycle-cmux-notify
type: design
title: "人への通知（`cmux notify`）"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0022
---

# 人への通知（`cmux notify`）

`WorkspaceBackend::notify(title, body, workspace)`は人への通知の操作で、cmux adapterは`cmux notify --title <title> --body <body> [--workspace <id>]`を実行する（`workspace`が`None`なら`--workspace`を付けない。失敗はcmuxの非0終了をエラーにして返す）。terminalへの打ち込みではないのでinboxやworkerのUI状態に干渉しない。

`cmux notify`を送るのはaskの登録だけで、新しいaskを登録したとき（`ask_opened`を書いたとき）に1回送る（[ADR-0022](../../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定5、task 87）。askを作るのはworker、observer（`blocked`）、手でreviewする人のプロセスと、supervisor（`/exit`のtimeoutの`stuck_exit`（task 104。[Receipt and session exit](receipt-and-session-exit.md#receipt-and-session-exit)）、ダイアログの`answer_prompt`、reviewの`approve_landing`、triageと試行を使い切ったresumeの`decide`）で、どれも`runtime::ask`と同じ経路（supervisorは開いているqueueで`runtime::ask_in`）で送る。宛先は`session_workspaces`に記録されたinboxのworkspace UUID（`up`が記録する）で、記録が無ければ`--workspace`なしで送る。titleは`[<repo>] ask #<id> <kind>`（`<repo>`はqueueが束縛されたrepositoryのmain checkoutのディレクトリ名。束縛の無い`--db` queueは作業ディレクトリの名前）、bodyはquestionの先頭200文字（超えれば`…`）と、改行の後の`task <id>`（runのaskなら` run <run-id>`を続ける。observerの`blocked`でtaskの無いaskはこの行を省きquestionだけ）。同じ（task、run、kind）のopenなaskを返しただけ（`created: false`）なら送らない。`answer`と`ask close`も送らない。cmuxは`ask --cmux <path>`（既定は`cmux`をPATHで解決）。通知の失敗でaskは失敗しない: askは登録済みのまま、出力の`notified`をfalseにして`notify_error`に理由を書く（成功なら`notified: true`）。`backend_call_failed`には記録しない（runにもsupervisorにも属さない呼び出しで、inboxはaskを`watch --role inbox`で受けるので通知は補助）。

supervisorはrunの遷移を通知しない。ADR-0016の契約(3)はattentionのたびに当時の常駐sessionのworkspaceへ送るとしていたが、ADR-0022の決定5でrunの遷移（`awaiting_integration`・`needs_session`・`failed`・`exit_request_timed_out`など）は通知しないことになった。`exit_request_timed_out`で人に届くのは、supervisorが作る`stuck_exit`のaskの通知（上記のaskの経路の1回）で、遷移の通知ではない。`integrate`の`push_failed`（task 70）も通知しない。inboxはattentionを`watch`で受ける。
