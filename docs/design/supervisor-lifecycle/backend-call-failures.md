---
id: design-supervisor-lifecycle-backend-call-failures
type: design
title: "backendの呼び出しの失敗"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0013
  - adr-0012
  - design-domain-model
---

# backendの呼び出しの失敗

`WorkspaceBackend`（cmux adapter）の呼び出しが失敗するかtimeoutすると（cmuxはどの呼び出しも30秒、`WorkspaceBackend::call_timeout`）、runtimeは`backend_call_failed`をrun_eventsに記録する（task 109）。負荷が高いとcmuxが詰まることをqueueに残し、observerが`stats`の`backend_failures`から並列度の見直しを根拠つきで提案できるようにするためで、記録するのはruntime、observerは`stats`を読むだけ。

- **payload**: `code`（`backend_timeout` / `backend_failed`。[domain-model](../domain-model.md#理由の分類コードcode)）、`op`（`create` / `create_named` / `capture` / `close` / `send_exit`（`send`と`send-key`）/ `exists` / `ensure_group`。`ask`の`notify`は記録しない（[人への通知](cmux-notify.md#人への通知cmux-notify)）。起動時の`preflight` / `preflight_detached`はcmuxに繋がるかの確認で、失敗すればコマンド自体が止まるので記録しない）、`workspace_id`（無い呼び出しはnull）、`timeout_secs`、`error`（先頭300文字）、`load_avg`（getloadavg(3)の1分値。取れなければnull）、`slots`、`parallel`、`attempt`（何回目の呼び出しか。1から）、`max_attempts`（その呼び出しに許す回数。retryしない呼び出しは1）、`retry_after_ms`（次の試行までのbackoff。retryしなければnull）。
- **timeoutのretry**（task 326）: timeoutした呼び出しは、もう一度呼んでも害が無いものだけ、backoffを置いて`WorkspaceBackend::call_attempts`（既定3）回までretryする。backoffは`WorkspaceBackend::retry_backoff`（既定2秒）から始めて毎回倍にする。失敗した試行はretryするものも含めて1回ずつ`backend_call_failed`になるので、`backend_failures`の件数はretryの分も数える。もう一度呼んでも害が無いかの判定（画面の読み取り）はbackoffの後、呼び直す直前に行い、backoffの間に遅れて届いた送信を二重に送らない（task 354。最後の試行の後はすぐ判定する）。上限まで失敗したときだけ呼び出し元に今までどおりのエラーを返す。retryするのは読むだけの`capture`と`exists`、送ったtextの跡が画面の最後の30行に無い`send_text`（跡は空でない最初の行の頭24文字（tabは空白、backslashの手前まで）か、Claude Codeが長い貼り付けを畳んだ`[Pasted text`。scrollbackの上の方にある以前の同じ文面は数えない。画面は`capture`で1回だけ読み、retryしない）。textが入力欄か画面に届いていれば打ち直さず、画面が読めなければ届いたかを推測しない。`send_enter`はretryしない（2回目のEnterはダイアログの選択肢を選びうる）。`send_exit`は、supervisorの送信（`submit_input`が`WorkspaceBackend::send_exit_when`で呼ぶ）に限り、画面を`capture`で1回読んで`/exit`が届いていないと分かるときだけ同じ関数でretryする（task 354）: 入力欄が描かれていて（`AgentSignals::input_ready`）ダイアログが無く、最後の30行に`/exit`の跡（`send_text`と同じ判定）が無いこと。跡があるか、ダイアログが出ているか、画面が読めなければ、届いたかもしれないものとして打ち直さず、[sessionへの送信と確認](session-send.md)の画面の確認に進む（二重送信を避ける）。全試行が届かないまま時間切れになった`/exit`はエラーではなく`Submission::Unsent`として返り、[Receipt and session exit](receipt-and-session-exit.md)の2のとおり`ExitWatch`が扱う。
  - `submit`（[sessionへの送信と確認](session-send.md#sessionへの送信と確認)、task 285）は、届いたかが分からないtimeout（`send_exit`のtimeout、跡が画面にあるか画面が読めない`send_text`のtimeout）を失敗にせず、返ってきた送信と同じく画面を読んで判断する。入力欄に残っていればEnterだけを送り直し、無ければ送れたものとする。届いていなかった`/exit`は、閉じて着地する条件がそろわなければ（`exit_unsent`の`action: recover`）exit timeoutと同じ`stuck_exit`の復旧jobとaskに、届いていなかったtextは`StartCheck`の送り直しかaskになる。上限までretryしても跡の無い`send_text`だけが今までどおりの失敗になる。
- **run**: runのための呼び出し（`create`、runの開いたworkspaceへの`capture` / `close` / `send_exit`）はそのrunのイベントとして`task_id`と`run_id`を持つ。workspaceからrunを引けない呼び出し（`up`のinbox / planner / supervisor workspaceの`create_named`と`exists`、queueのworkspace groupの`ensure_group`、`down`のsupervisor workspaceの`close`）は`task_id`も`run_id`も持たない（0012で`run_events`のCHECKがこのkindだけに認める）。
- **slots / parallel**: supervisorの呼び出しはそのsupervisorのtokenのlease数（握っているslot）と`--parallel`。`up` / `down`の呼び出しは全leaseの数と、登録済みsupervisorの`parallel`の合計（登録が無ければnull）。
- **記録する場所**: cmux adapterではなくapplication層の`application::recording::RecordingBackend`（`WorkspaceBackend`を包むdecorator。`runtime::RecordingBackend`として再公開し、DBのpathから作る`new`は`compose`にある。`up`・`down`は`QueueOpener`から`over`で作る）が、`supervise`・`up`・`down`で渡されたbackendを包んで記録する（[ADR-0013](../../adr/0013-layered-architecture-and-type-function-style.md)。`supervise`ではユースケースが自分のtokenで包む）。記録は`QueueOpener`が開く自前の接続で書き、書けなくても呼び出し元へ返すエラーは元のまま。needs_sessionのresume（[`needs_session`](needs-session.md#needs_session)）の`create_resume`（runに記録）、`send_text`・`send_exit`・`close`・`exists`・`capture`も同じ経路を通る。resume workspaceのIDは`task_runs.workspace_id`に無いので、それらの失敗はrunを持たない記録になる（`create_resume`はrunに付く）。
- 既存の記録はそのまま残す: closeの失敗は`cleanup_failed`、wrapper終了後の`read-screen`の失敗は`screen_capture_failed`で、どちらも同じ失敗を`backend_call_failed`としても記録する（呼び出しの直後なので`backend_call_failed`が先）。`/exit`後にsessionが終わらない`exit_request_timed_out`はcmuxの呼び出しの失敗ではないので`backend_call_failed`にならない（`backend_call_failed`になるのは`/exit`の送信（`send_exit`）そのものが失敗かtimeoutしたときだけ）。`create`の失敗（provisioningの失敗）と`/exit`の送信の失敗（timeout以外。timeoutは上のとおりrunを止めない）はrunをabandonするが、`backend_call_failed`はabandonの`runtime_error`より前に入る。

supervisorの再起動ではrunごとのleaseとheartbeatを確認し、孤児プロセスを勝手に再実行しない。wrapperが生きている（heartbeatが30秒以内か`exited_at`記録済み）`running` / `validating`のrunだけは、staleなleaseごと次のsupervisorが引き継いで同じrunを続ける（[ADR-0012](../../adr/0012-adopt-stale-lease-of-live-wrapper.md)、[`supervise`](supervise.md#supervise)の5）。`awaiting_integration`のrunはwrapperを問わず引き継ぐ（task 236）。それ以外（wrapperが死んだ・黙った、`claimed` / `starting`、`integrating`、leaseなし）は、processが止まっていてleaseが無いかleaseのpidが死んでいればsupervisorが自分で`recover`してtriageにかけ、そうでなければユーザーが`recover`で明示的に復旧する。
