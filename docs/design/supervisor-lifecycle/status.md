---
id: design-supervisor-lifecycle-status
type: design
title: "`status`"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0014
  - adr-0017
  - adr-0044
  - adr-0025
  - adr-0022
  - design-domain-model
---

# `status`

ユースケースはapplication層の`src/application/health.rs`の`status`（`doctor`・`recover`と、`status`と`watch`が使うattentionの導出`attention`も同じファイル）で、queueは`Queue`（`latest_event_id`・`latest_runs_in_progress`・`runs_with_pending_push`・`asks`を含む）、PIDの生死は`ProcessControl`、時刻は`Clock`から得る。入口は`compose::OneShot::status_for`（自由関数の`compose::status_for`はsystemの`Generators`で呼ぶ）。

`dagq status`はrunのprocessを調べずに登録とleaseだけを返す。引き継がれたrunは引き継いだsupervisorのtokenのleaseを持つので、他のrunと同じくそのsupervisorの`run_ids`に並ぶ。`supervisors`は`supervisors`表の登録を`started_at`順に、続けて登録のないtokenのlease保持者（着地中の`integrate`プロセス、または登録表以前のsupervisor）をlease順に並べ、leaseはtokenで登録に結び付ける。各項目は`pid`、`alive`（`kill -0`）、`registered`、`mode`と`workspace_id`、`binary_version`（そのプロセスが動いているdagqのversion。登録がなければnull、列より古いbinaryの登録もnull。[ADR-0014](../../adr/0014-up-replaces-a-supervisor-of-another-binary-version.md)）、`parallel`と`started_at`（登録がなければnull）、`heartbeat_at`、`heartbeat_age_secs`、`stale`（PIDが死んでいるかheartbeatが30秒より古い）、`run_ids`（そのtokenのlease）。runを持たない常駐supervisorは`run_ids: []`で並ぶ。`runs`は未完了run（`claimed`/`starting`/`running`/`validating`/`integrating`）ごとに`run_id`、`task_id`、`status`、`workspace_id`、`worktree_path`（queueの今の`runs/`から解決したpath。[ADR-0017](../../adr/0017-resolve-run-paths-from-the-queue-directory.md)）、`lease`（なければnull。`pid`でどのsupervisorが持つかが分かる）と、`last_error`か中断の理由の分類コードが分かるrunだけ`last_error_code`（[domain-model](../domain-model.md#理由の分類コードcode)）。`awaiting_integration`と`needs_session`はプロセスを持たないので並ばない。

`status`は続けて`attention`と`cursor`を返す（ADR-0016）。`asks`（openなask）の隣の`proposals`は、plan review待ち（`submitted`）と差し戻し中（`revising`）のproposalをsubmitの古い順に並べる（[ADR-0044](../../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定7。形は`proposal list`と同じ）。`cursor`はrun_eventsの最新id（空のqueueは0）で、状態を読む前に取るので、その後の遷移は`watch --after cursor`で必ず拾える（重複はありうるが取りこぼさない）。`attention`は人の判断で止まっているものと、supervisorが動かしているものを今の状態から導出し、supervisorを先、runを後に並べる。各項目は`run_id`、`task_id`、`status`、`kind`、`last_error`（300文字で切り詰め）、`last_error_code`（`last_error`か中断の理由の分類コード。分からなければ項目ごと省く。pushの項目は`push_failed`）、`next`（定型の短い句）で、supervisorの項目は`run_id`/`task_id`がnullで`pid`を持つ。

- supervisor: `supervisors`表の登録のうちstale（PIDが死んでいる、またはheartbeatが`HEARTBEAT_TIMEOUT_SECS`より古い）なものが`kind: supervisor_stale`（`status`は`dead`か`stale`）、登録が1件もなければ`kind: supervisor_stopped`（`status: stopped`、`pid`なし）。`next`は`restart supervisor`。lease保持者だけの`integrate`は数えない。この2つのkindはrun_eventsに書かれない導出値。
- run: `in_progress`のtaskの最新runを`domain::run_attention`で判定する。`awaiting_integration`→leaseがあればsupervisorのreview中なので`reviewing (runtime)`、無ければ`review and integrate`（最後のattentionイベントか`review_failed`が`review_failed`なら`review by hand`。closeされていない`approve_landing`のaskがあるrunはaskがattentionなので出さない）、`needs_session`→どの場合も`resuming (runtime)`（[`needs_session`](needs-session.md#needs_session)の末尾。`kind`はrunを`needs_session`にした最新のイベント）、`failed` / `interrupted`→`domain::triage_state`で、triage前なら`triaging (runtime)`、`triage_failed`の後なら`triage by hand`、`triage_finished`の後は出さない（verdictがtaskかrunを動かしたか、`decide`のaskがattention。[Triage (supervisor)](triage.md#triage-supervisor)）、lease行の無い`claimed` / `starting` / `running` / `validating` / `integrating`→`recover run`（supervisorが手放したrun。[ADR-0025](../../adr/0025-leaseless-unfinished-run-is-a-recover-run-attention.md)）、`exit_request_timed_out`の後に`session_exited`がない`running`と、verdictの後の`/exit`を待つleaseのある`awaiting_integration` / `needs_session` / `failed`→attentionにしない（supervisorの`stuck_exit`のaskがattention。task 104。[Receipt and session exit](receipt-and-session-exit.md#receipt-and-session-exit)）。ダイアログで止まった`running`のrunは出さない（`answer_prompt`のaskがattention。[ダイアログ待ちの検知](prompt-waiting.md#ダイアログ待ちの検知)）。`integrate`はleaseの取得・解放を`integrating`への出入りと同じトランザクションで行うので、leaseの無い`integrating`は正常な経路では生じない。leaseがstaleなrunはここに出さない（supervisorが死んだならその項目、wrapperが生きていればadopt。死んだ`integrate`が残したleaseはどのattentionにも出ない）。leaseの有無はrunのstatusより先に確かめ直す（supervisorはstatusを動かしてからleaseを解放するので、読む間に解放されたrunを取り違えない）。`kind`はそのrunでattentionと判定された最後のイベントのkind（`recover run`は直近の`runtime_error`。無ければstatus名）、`last_error`はrunの`last_error`。taskが再試行・cancel・完了されたrunは出ない。
- push: `push_failed`を持つ`integrated`のrunのうち、その`push_failed`がqueue全体で最後の`push_finished`より後のもの（`runs_with_pending_push`）を`domain::run_attention`で`push main`にする。`kind`は`push_failed`、`last_error`はその`error`。taskは`completed`でも出る。後の`integrate`のpushが成功すればmainはそれまでの着地を含むので消える。`push_skipped`は消さない。手で`git push origin main`してもqueueには記録されないので、次の`push_finished`までは残る。
- ask（[ADR-0022](../../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)、[ask / answer](ask.md#ask--answer--asks)）: closeされていないaskを`asks`表の順に並べる。未回答のものは`kind: ask_opened`、`status: open`、`next: answer ask <id>`、回答済みのものは`kind: ask_answered`、`status: answered`、`next: read the answer of ask <id> and close it`。ただし回答済みの`worker_question`は`delivering the answer of ask <id> (runtime)`か`send the answer of ask <id> to the worker and close it`（[workerの質問への回答の送信](worker-question-answer.md#workerの質問への回答の送信)）。回答済みの`approve_landing`で、runが`awaiting_integration`で回答が`land` / `send_back` / `cancel`のどれかなら`applying the answer of ask <id> (runtime)`（supervisorが適用する。[Review](review.md#review-supervisor)の6）。項目は`ask_id`を持ち、`run_id`はaskのrun（taskだけのaskはnull）、`last_error`はnull。

`status --role <inbox|planner>`は`attention`をそのroleに宛てたものだけにする（省略時は全部）。attentionはすべてinbox宛て（`domain::ATTENTION_ROLE`、ADR-0044の決定17）で、`--role inbox`は全部、`--role planner`は空になる。`asks`はroleに関わらずopenなask（未回答でcloseされていないもの）の一覧で、各項目は`id`、`kind`、`question`（先頭200文字。切ったときは末尾に`…`）、`task_id`、`run_id`、`asked_by`、`age_secs`（登録からの秒数）。
