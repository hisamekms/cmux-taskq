---
id: design-supervisor-lifecycle-waiting
type: design
title: "人の答えを待つrun（slotの外の待ち）"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-supervise
  - design-supervisor-lifecycle-status
  - design-supervisor-lifecycle-stats
  - design-supervisor-lifecycle-handoff
  - design-supervisor-lifecycle-review
  - design-supervisor-lifecycle-needs-session
  - adr-0062
  - adr-0071
  - adr-0039
  - adr-0045
  - design-persistence
---

# 人の答えを待つrun（slotの外の待ち）

[ADR-0062](../../adr/0062-runs-waiting-for-a-person-leave-the-slot.md)の実装（決定は[ADR-0071](../../adr/0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)が置き換えて引き継いだ）。生きているsessionを持つrunが人の答えか人の操作だけを待っているあいだ、そのrunを`--parallel`のslotから外し（**待ち**）、空いたslotで他のtaskをclaimする。use caseは`src/application/supervise/waiting.rs`、イベントからの状態の導出と`stats`の集計は`src/domain/waiting.rs`。

## 待ちの出入り

- **印**: `Slot`は`waiting: Option<Waiting>`を持つ。`Waiting`は待ちが持つaskのIDとkind、始めた時刻（queueの時計）、sessionが動いたかを比べる基準の時刻（最初のaskを開いた秒の次の秒と待ちを始めた時刻の早い方。askの時刻は秒なので、askと同じ秒のmarkerは古いとみなす。run filesの時計）、画面を最後に読んだ時刻、終わった時刻と理由（戻り待ち）。runのstatusとphase（`SessionWatch` / `ExitWatch` / `ReviseWatch` / `ResumeWatch`）は変えないので、戻ればphaseの続きから進む（`Revise` / `Resume`の段の計時だけは戻るときにやり直す。下の「段の計時」）。`used_slots()`は印の無いslotの数で、`fill_slots`・adopt・landingの答え・resume・triage・claimの空きの判定はこれを使う。待ちのrunも`slots`に居るので、ループはそれが残っていれば続く（drainも待つ）。
- **始める**（`start_waits`、tickの先頭。`tick(true)`の引き継ぎ待ちでは行わない）: 印の無いslotのうち、phaseが`Session`で`/exit`を送っておらずwrapperが黙っておらず復旧jobが走っていないもの、`Exiting`でsessionを持つもの、`Revise`でwrapperが黙っておらず（`live.silent`）復旧jobが走っていないもの、または`Resume`で`/exit`を送っておらず（`exit_requested`が無い）wrapperが黙っておらず復旧job（`stuck_exit`のものも、ダイアログのものも）が走っていないもので、sessionが生きていて（wrapperが`exited_at`を記録しておらず死んでいない）、queue_holdに入っておらず、ADR-0071の決定1の表のkind（`domain::waiting::waits_for`。`Session`: `worker_question` / `answer_prompt` / `stalled`、`Exiting`: `stuck_exit` / `answer_prompt`、`Revise` / `Resume`: `worker_question` / `answer_prompt`）の未回答でcloseされていないaskを持ち、そのaskが終わった待ちのものでない（`consumed`）run。askのIDの古い順に、slotの外のrun（待ちと戻り待ち。どちらもsessionを開いたまま）の数が`--max-waiting`未満のあいだ`run_waiting_started`を記録して待ちにする。上限に達していれば`run_waiting_deferred`をaskごとに1回だけ記録し、slotに居たまま今のphaseで進む（上限が空けば次のtickで待ちに移る）。`--max-waiting 0`は待ちを使わない。
- **見張り**（`watch_waiting`。待ちのslotでは`step`を呼ばない）: sessionに何も送らず、runのstatusも変えない。
  1. leaseが自分のtokenでなければ、他のslotと同じく退く（DBには書かない。引き継いだ側がイベントから待ちを組み立てる）。
  2. wrapperが終了を記録したか、heartbeatが切れてpidも死んでいれば、`answer_prompt`と`stuck_exit`のaskをその場で閉じ（`Session`なら`stalled`も`ended`で閉じる）、`session_exited`で終える（`Revise` / `Resume`はslotに戻ったtickで段が今のとおりsessionの終わりを扱う）。wrapperが黙っていれば（pidは生きている）`wrapper_heartbeat_expired`を記録し、`Session` / `Revise` / `Resume`ならslotで`/exit`を送るか段を終えるため`wrapper_silent`で終える（`Exiting`は`/exit`を送った後なので待ちを続ける）。`Revise`の沈黙はその後の`ExitWatch`に引き継ぎ、`wrapper_heartbeat_expired`を2度記録しない。slotに戻ったときheartbeatが戻っていれば（`/exit`を送る前）、段は黙った印を消して続き、その後のaskで再び待ちに入れ、次の沈黙は`wrapper_heartbeat_expired`を改めて記録する（task 606。[wrapperが黙ったsession](silent-wrapper.md)）。
  3. runがqueue_holdに入っていれば`queue_hold`で終える。
  4. 待ちのあいだに開いた表のaskを`run_waiting_ask_added`で足す。
  5. `Session`のrunで、待ちのあいだに`SessionWatch`がまだ見ていないreceiptが現れたら`session_moved`で終える（どのkindの待ちでも。askの後始末は戻ったslotの`SessionWatch`が行う）。持っている`worker_question`が回答されたか閉じられたら`answered`で終える（答えの送信はslotに戻ったtickの`deliver_answers`）。`Revise` / `Resume`の待ちが`worker_question`を持つあいだは、以下の6〜8で終えない（receiptやダイアログで戻しても、段は閉じていない`worker_question`のあいだ進まず、そのaskはもう待ちに入れないので、答えまでslotを塞ぐ。ADR-0071の決定2・16）。
  6. `stalled`を持つ`Session`のrunでreceiptが無ければ、`StallWatch::poll_quiet`でそのaskを追う（`wait`の答えでaskを閉じて計時をやり直し、閾値を過ぎたら次の`stalled`のaskを開く。促しも既知のダイアログへのキーも送らない）。
  7. `answer_prompt`か`stalled`を持つrunで、idle marker・`prompt-submit.json`・receiptのどれかが基準の時刻より新しければ`session_moved`で終える（`Revise` / `Resume`で段の依頼の後に書き直されたreceiptもこれにあたる）。
  8. `answer_prompt`を持つ`Session` / `Revise` / `Resume`のrunは、`watch_prompt`と同じ間隔で画面を読み、ログインの切れなら`raise_auth`して`queue_hold`、ダイアログが無ければ`prompt_cleared`を記録してaskを閉じ、`dialog_cleared`で終える（`Revise` / `Resume`ではその段の`live`の`SessionWatch`が記録したダイアログ）。解消依頼を送る前の`Resume`（`input_not_ready`のask）は、入力欄が準備できてダイアログの無い画面で`dialog_cleared`で終え、askは戻ったslotの`ResumeWatch`が依頼を送る前に閉じる。
- **終わり**（`end_wait`）: `run_waiting_ended`（`ask_id`、`ask_kind`、`cause`、`waited_secs`）を記録し、持っていたaskを`consumed`に足す。`answered`と`session_exited`は戻り待ちになり、それ以外（`dialog_cleared` / `session_moved` / `queue_hold` / `wrapper_silent` / `phase_changed`）はその場でslotに戻す（`run_slot_regained`の`over_parallel`は戻った後のslotの数が`--parallel`を超えたか）。
- **戻す**（`return_waiting_runs`）: `drive`のループの毎回、引き継ぎの判定と`fill_slots`より前に（claimを止めていても、drain中も）、戻り待ちのrunを終わった順に、`used_slots()`が`--parallel`未満のあいだslotへ戻し、`run_slot_regained`（`slot_wait_secs`、`over_parallel`）を記録する。戻ったrunは今のphaseのとおり進む（`worker_question`なら答えの配送、`stuck_exit`なら`ExitWatch`がsessionの終了を見て画面を保存し、`AfterExit`のとおり着地・`approve_landing`のask・`review_failed`へ）。

次の3点はADR-0062の文言から外れていたが、[ADR-0071](../../adr/0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)（ADR-0062を置き換え）が実装どおりに決定として取り込んだ（決定2・5・6・7）: `cause`の`wrapper_silent`と`phase_changed`、`lease_lost` / `run_ended`を書かずにlease系のイベントで待ちを終えること、戻り待ちも`--max-waiting`に数えること、`Session`の待ちのあいだのreceiptで待ちを終えること。`status`の`waiting.count`に戻り待ちを含めること（決定12）はtask 545で、`Revise` / `Resume`の待ち（決定1・15〜18）はtask 573で入った。

## 差し戻しと解消依頼の段（`Revise` / `Resume`）

ADR-0071の決定15〜18。

- **段の計時**（決定15）: 待ちのあいだは段のpollを呼ばないので、`ReviseWatch`の`sent`、`ResumeWatch`の`message_sent`の時計、依頼を送る前の`agent_seen`と`ready_since`、古いreceiptの書き直しの依頼（`StaleNudge`）の時間切れは進んでも判定されず、段は時間切れにならない。slotに戻るとき（`regain_slot`と`return_waiting_runs`。`run_slot_regained`を記録するとき）に`Phase::restart_stage_clocks`が段の計時を戻った時点から数え直す: `Revise`は`sent`、`Resume`は`ResumeWatch::restart_clocks`（依頼を送っていれば`message_sent`の時計、送る前なら`agent_seen`と`ready_since`、決着していない古いreceiptの書き直しの依頼の`at`。`at`はその依頼の時間切れとidleを数え始める時刻を兼ねる）。残り時間は持ち越さない。引き継ぎとadoptで組み立て直した段は今のとおり組み立て直した時点（handoffの`Resume`は依頼の送信時刻）から数える。
- **`worker_question`のidle**（決定16）: `ReviseWatch`（task 238）と同じく、`ResumeWatch`もcloseされていない`worker_question`があるあいだは、idleでも段を終えず、`resume_timeout`も数えず、古いreceiptの書き直しも頼まない（`has_unclosed_worker_question`の判定がそれらより前にある）。上限でslotに居るとき（`run_waiting_deferred`）と`--max-waiting 0`でも同じ。答えを送ったときはその時刻を、手で配送されてaskがcloseされたとき（closeの秒が最後の入力の秒より後）はcloseの時刻を最後の入力（`live.input_at`）にし、段の計時をそこから数え直す。以後のidleはそれより新しいmarkerだけを数える。
- **`Resume`の配送とダイアログ**（決定17）: `ResumeWatch`は`live: Box<SessionWatch>`（`SessionWatch::fixing`。`ReviseWatch`と同じ構成）を持ち、解消依頼を送った後、`/exit`を送るまでの毎回のpollで`deliver_answers`と、agentが生きていれば`watch_prompt`を回す。`/exit`を送るとき（解消の判定、idle、時間切れ、wrapperの沈黙、入力欄が準備できないままの時間切れのどれでも）は`request_exit`が`exit_requested`（`workspace_id`、`timeout_secs`、`resume_attempt`）を送る前に記録し、`live`の記録したダイアログを`prompt_cleared`にして復旧jobを止める。`domain::session_takes_answers`は、最新の`resume_started`の後に`resume_finished`も`exit_requested`も無い`needs_session`のrun（`domain::resume_in_progress`）も答えの配送先にするので、leaseがあれば`worker_question`の`ask_answered`の`runtime_delivers`が`true`になり、`status`は`delivering the answer of ask <id> (runtime)`を出す。
- **phase**: `run_waiting_started`の`phase`と`status`の`waiting[].phase`は`revise` / `resume`。`status`はrunの今のstatus（`awaiting_integration` / `needs_session`）。`stats`の`waiting`は`ask_kind`ごとに数え、phaseでは分けない（決定18）。
- **引き継ぎとadopt**: `Revise`はadoptと引き継ぎのどちらでもイベント（`review_anchor`）から組み立て、`Resume`は引き継ぎの`handoff.json`から組み立てる（`Resume`はadoptの対象外のまま）。どちらも組み立てた後の`restore_waiting`で待ちを戻す。

ADR-0062の決定2の`cause`に、この実装は`wrapper_silent`（`Session`でwrapperが黙った。slotで`/exit`を送る）と`phase_changed`（引き継ぎやadoptで待てないphaseに組み立て直した、またはadoptで上限を超えた）を足している。`lease_lost`と`run_ended`は書かない（leaseを失ったプロセスは書かない。adoptした側がイベントから同じ待ちを続けるので、書くとその待ちを消してしまう）。代わりに`WaitState::of`は、`run_waiting_started`の後に`lease_acquired` / `lease_released` / `run_recovered` / `runtime_error`があれば待ちは無いとみなす（待ちを持っていたsupervisorがrunを失った。adoptと引き継ぎはこれらを書かない）。

## 引き継ぎとadopt

- **組み立て直し**（`restore_waiting`）: execの引き継ぎ（[Handoff](handoff.md)）とadopt（[`supervise`](supervise.md)の5）でslotを組み立てた後、runのイベントから`consumed`（終わった待ちのask）、`deferred`、今の待ち（`WaitState::of`: 最新の`run_waiting_started`の後に`run_waiting_ended`が無ければ待ち、`run_waiting_ended`の後に`run_slot_regained`が無ければ戻り待ち）を戻す。待ちに入った時刻はイベントから取り、`run_waiting_started`を記録し直さない。組み立て直したphaseが待てないもの（例えばreviewからやり直すrun）なら`phase_changed`で終えてslotに戻す。引き継ぎは上限を超えていても待ちのまま戻す（上限を下回るまで新しい待ちを入れない）。
- **adopt**: `adopt_stale_runs`は空きslotの判定を先頭の打ち切りではなくrunごとに行い、イベントの上で待っているrunは待ちの数が上限未満ならslotの空きを要さずに待ちとして引き継ぐ。上限に空きが無ければ今のとおりslotの空きを待って引き継ぎ、待ちを`phase_changed`で終えてslotのrunとして扱う（そのaskでまた待ちに入れる）。

## 登録と見せ方

- `supervise --max-waiting N`（既定4、0で待ちを使わない）と`up --max-waiting N`（既定と違うときだけ`supervise`の引数に足す）。supervisorは登録（と引き継ぎの取り戻し）の直後に`supervisors.max_waiting`（schema v37、互換の列。[persistence](../persistence.md)）を書く。
- `status`の各登録に`slots: {used, parallel}`と`waiting: {count, returning, limit}`（`count`は待ちと戻り待ちの合計で`--max-waiting`が数えるものと同じ、`returning`はその内訳の戻り待ち。数え方は`src/domain/waiting.rs`の`WaitCount`の1つで、supervisorの`waiting_runs()`と`status`の両方が使う）、全体に`waiting`の配列（[`status`](status.md)）。`stats`に`waiting`（[`stats`](stats.md)）。

## test

`tests/it/runtime_waiting_stages.rs`: `Revise`の`worker_question`が待ちに入って1つのslotを他のtaskに譲り、`resume_timeout`を過ぎて持っても段が終わらず、答えの後にslotへ戻って配送され、書き直して着地すること、`Resume`の`worker_question`も同じく待ちに入り（`phase: resume`、`status: needs_session`）、`runtime_delivers`が`true`で、戻った後の答えの配送の後のidleで段を終えて`exit_requested`（`resume_attempt`）を記録し着地すること、上限に達した`Resume`の`worker_question`が`run_waiting_deferred`でslotに居たまま計時を止めて待つこと、adoptした`Revise`の待ちがslotの空きを要さずに続くこと。`tests/it/runtime_waiting.rs`: 待ちのrunがslotに数えられずに他のtaskがclaimされ、答えの後に空いたslotへ戻って配送されること（`status`と`stats`の形も）、`--max-waiting`の上限と`run_waiting_deferred`と上限が空いた後の待ち、`stuck_exit`の待ちとsessionの終了で閉じるaskと戻った後の`review_failed`、`stuck_exit`の待ちのあいだにwrapperが終了を記録せずに死んだとき（heartbeatが切れpidも無い）にaskを閉じて`session_exited`で終えること、`worker_question`の待ちのあいだにwrapperが黙ったとき（pidは生きている）に`wrapper_heartbeat_expired`を記録して`wrapper_silent`でその場で戻り、slotで`/exit`を送り、wrapperが死ねば今のとおりrunを手放すこと、待ちのあいだに黙ったwrapperのheartbeatがslotに戻る前に戻れば`/exit`を送らず、答えの配送の後の新しい`worker_question`で再び待ちに入り、2度目の沈黙で`wrapper_heartbeat_expired`を改めて記録して`/exit`を送ること（task 606）、引き継ぎの後に待ちを続けること、adoptで空きslotを要さずに待ちを引き継ぐこと、人がダイアログに答えた2つのrunがその場で戻り1つが`--parallel`を超えること、`--max-waiting 0`で待たないこと、戻り待ちのrunが`status`の`waiting.count`と`returning`に数えられ、上限1をそれが埋めているあいだにslotのrunが聞くと`run_waiting_deferred`になること。`src/domain/waiting.rs`のunit testがイベントからの状態と集計と`WaitCount`の数え方を確かめる。
