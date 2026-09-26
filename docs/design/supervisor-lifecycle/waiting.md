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
  - adr-0062
  - adr-0071
  - adr-0039
  - adr-0045
  - design-persistence
---

# 人の答えを待つrun（slotの外の待ち）

[ADR-0062](../../adr/0062-runs-waiting-for-a-person-leave-the-slot.md)の実装（決定は[ADR-0071](../../adr/0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)が置き換えて引き継いだ）。生きているsessionを持つrunが人の答えか人の操作だけを待っているあいだ、そのrunを`--parallel`のslotから外し（**待ち**）、空いたslotで他のtaskをclaimする。use caseは`src/application/supervise/waiting.rs`、イベントからの状態の導出と`stats`の集計は`src/domain/waiting.rs`。

## 待ちの出入り

- **印**: `Slot`は`waiting: Option<Waiting>`を持つ。`Waiting`は待ちが持つaskのIDとkind、始めた時刻（queueの時計）、sessionが動いたかを比べる基準の時刻（最初のaskを開いた秒の次の秒と待ちを始めた時刻の早い方。askの時刻は秒なので、askと同じ秒のmarkerは古いとみなす。run filesの時計）、画面を最後に読んだ時刻、終わった時刻と理由（戻り待ち）。runのstatusとphase（`SessionWatch` / `ExitWatch`）は変えないので、戻ればphaseの続きから進む。`used_slots()`は印の無いslotの数で、`fill_slots`・adopt・landingの答え・resume・triage・claimの空きの判定はこれを使う。待ちのrunも`slots`に居るので、ループはそれが残っていれば続く（drainも待つ）。
- **始める**（`start_waits`、tickの先頭。`tick(true)`の引き継ぎ待ちでは行わない）: 印の無いslotのうち、phaseが`Session`で`/exit`を送っておらずwrapperが黙っておらず復旧jobが走っていないもの、または`Exiting`でsessionを持つもので、sessionが生きていて（wrapperが`exited_at`を記録しておらず死んでいない）、queue_holdに入っておらず、ADR-0062の表のkind（`Session`: `worker_question` / `answer_prompt` / `stalled`、`Exiting`: `stuck_exit` / `answer_prompt`）の未回答でcloseされていないaskを持ち、そのaskが終わった待ちのものでない（`consumed`）run。askのIDの古い順に、slotの外のrun（待ちと戻り待ち。どちらもsessionを開いたまま）の数が`--max-waiting`未満のあいだ`run_waiting_started`を記録して待ちにする。上限に達していれば`run_waiting_deferred`をaskごとに1回だけ記録し、slotに居たまま今のphaseで進む（上限が空けば次のtickで待ちに移る）。`--max-waiting 0`は待ちを使わない。
- **見張り**（`watch_waiting`。待ちのslotでは`step`を呼ばない）: sessionに何も送らず、runのstatusも変えない。
  1. leaseが自分のtokenでなければ、他のslotと同じく退く（DBには書かない。引き継いだ側がイベントから待ちを組み立てる）。
  2. wrapperが終了を記録したか、heartbeatが切れてpidも死んでいれば、`answer_prompt`と`stuck_exit`のaskをその場で閉じ（`Session`なら`stalled`も`ended`で閉じる）、`session_exited`で終える。wrapperが黙っていれば（pidは生きている）`wrapper_heartbeat_expired`を記録し、`Session`ならslotで`/exit`を送るため`wrapper_silent`で終える（`Exiting`は`/exit`を送った後なので待ちを続ける）。
  3. runがqueue_holdに入っていれば`queue_hold`で終える。
  4. 待ちのあいだに開いた表のaskを`run_waiting_ask_added`で足す。
  5. `Session`のrunで、待ちのあいだに`SessionWatch`がまだ見ていないreceiptが現れたら`session_moved`で終える（どのkindの待ちでも。askの後始末は戻ったslotの`SessionWatch`が行う）。持っている`worker_question`が回答されたか閉じられたら`answered`で終える（答えの送信はslotに戻ったtickの`deliver_answers`）。
  6. `stalled`を持つ`Session`のrunでreceiptが無ければ、`StallWatch::poll_quiet`でそのaskを追う（`wait`の答えでaskを閉じて計時をやり直し、閾値を過ぎたら次の`stalled`のaskを開く。促しも既知のダイアログへのキーも送らない）。
  7. `answer_prompt`か`stalled`を持つrunで、idle marker・`prompt-submit.json`・receiptのどれかが基準の時刻より新しければ`session_moved`で終える。
  8. `answer_prompt`を持つ`Session`のrunは、`watch_prompt`と同じ間隔で画面を読み、ログインの切れなら`raise_auth`して`queue_hold`、ダイアログが無ければ`prompt_cleared`を記録してaskを閉じ、`dialog_cleared`で終える。
- **終わり**（`end_wait`）: `run_waiting_ended`（`ask_id`、`ask_kind`、`cause`、`waited_secs`）を記録し、持っていたaskを`consumed`に足す。`answered`と`session_exited`は戻り待ちになり、それ以外（`dialog_cleared` / `session_moved` / `queue_hold` / `wrapper_silent` / `phase_changed`）はその場でslotに戻す（`run_slot_regained`の`over_parallel`は戻った後のslotの数が`--parallel`を超えたか）。
- **戻す**（`return_waiting_runs`）: `drive`のループの毎回、引き継ぎの判定と`fill_slots`より前に（claimを止めていても、drain中も）、戻り待ちのrunを終わった順に、`used_slots()`が`--parallel`未満のあいだslotへ戻し、`run_slot_regained`（`slot_wait_secs`、`over_parallel`）を記録する。戻ったrunは今のphaseのとおり進む（`worker_question`なら答えの配送、`stuck_exit`なら`ExitWatch`がsessionの終了を見て画面を保存し、`AfterExit`のとおり着地・`approve_landing`のask・`review_failed`へ）。

次の3点はADR-0062の文言から外れていたが、[ADR-0071](../../adr/0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)（ADR-0062を置き換え）が実装どおりに決定として取り込んだ（決定2・5・6・7）: `cause`の`wrapper_silent`と`phase_changed`、`lease_lost` / `run_ended`を書かずにlease系のイベントで待ちを終えること、戻り待ちも`--max-waiting`に数えること、`Session`の待ちのあいだのreceiptで待ちを終えること。ADR-0071の決定のうち、`Revise` / `Resume`の待ち（決定1・15）、`Resume`の段の答えの配送とダイアログの検知と`worker_question`のあいだidleで段を終えないこと（決定16・17。`Revise`の側はtask 238で入っている）はまだ実装されていない。`status`の`waiting.count`に戻り待ちを含めること（決定12）はtask 545で入った。

ADR-0062の決定2の`cause`に、この実装は`wrapper_silent`（`Session`でwrapperが黙った。slotで`/exit`を送る）と`phase_changed`（引き継ぎやadoptで待てないphaseに組み立て直した、またはadoptで上限を超えた）を足している。`lease_lost`と`run_ended`は書かない（leaseを失ったプロセスは書かない。adoptした側がイベントから同じ待ちを続けるので、書くとその待ちを消してしまう）。代わりに`WaitState::of`は、`run_waiting_started`の後に`lease_acquired` / `lease_released` / `run_recovered` / `runtime_error`があれば待ちは無いとみなす（待ちを持っていたsupervisorがrunを失った。adoptと引き継ぎはこれらを書かない）。

## 引き継ぎとadopt

- **組み立て直し**（`restore_waiting`）: execの引き継ぎ（[Handoff](handoff.md)）とadopt（[`supervise`](supervise.md)の5）でslotを組み立てた後、runのイベントから`consumed`（終わった待ちのask）、`deferred`、今の待ち（`WaitState::of`: 最新の`run_waiting_started`の後に`run_waiting_ended`が無ければ待ち、`run_waiting_ended`の後に`run_slot_regained`が無ければ戻り待ち）を戻す。待ちに入った時刻はイベントから取り、`run_waiting_started`を記録し直さない。組み立て直したphaseが待てないもの（例えばreviewからやり直すrun）なら`phase_changed`で終えてslotに戻す。引き継ぎは上限を超えていても待ちのまま戻す（上限を下回るまで新しい待ちを入れない）。
- **adopt**: `adopt_stale_runs`は空きslotの判定を先頭の打ち切りではなくrunごとに行い、イベントの上で待っているrunは待ちの数が上限未満ならslotの空きを要さずに待ちとして引き継ぐ。上限に空きが無ければ今のとおりslotの空きを待って引き継ぎ、待ちを`phase_changed`で終えてslotのrunとして扱う（そのaskでまた待ちに入れる）。

## 登録と見せ方

- `supervise --max-waiting N`（既定4、0で待ちを使わない）と`up --max-waiting N`（既定と違うときだけ`supervise`の引数に足す）。supervisorは登録（と引き継ぎの取り戻し）の直後に`supervisors.max_waiting`（schema v37、互換の列。[persistence](../persistence.md)）を書く。
- `status`の各登録に`slots: {used, parallel}`と`waiting: {count, returning, limit}`（`count`は待ちと戻り待ちの合計で`--max-waiting`が数えるものと同じ、`returning`はその内訳の戻り待ち。数え方は`src/domain/waiting.rs`の`WaitCount`の1つで、supervisorの`waiting_runs()`と`status`の両方が使う）、全体に`waiting`の配列（[`status`](status.md)）。`stats`に`waiting`（[`stats`](stats.md)）。

## test

`tests/it/runtime_waiting.rs`: 待ちのrunがslotに数えられずに他のtaskがclaimされ、答えの後に空いたslotへ戻って配送されること（`status`と`stats`の形も）、`--max-waiting`の上限と`run_waiting_deferred`と上限が空いた後の待ち、`stuck_exit`の待ちとsessionの終了で閉じるaskと戻った後の`review_failed`、`stuck_exit`の待ちのあいだにwrapperが終了を記録せずに死んだとき（heartbeatが切れpidも無い）にaskを閉じて`session_exited`で終えること、`worker_question`の待ちのあいだにwrapperが黙ったとき（pidは生きている）に`wrapper_heartbeat_expired`を記録して`wrapper_silent`でその場で戻り、slotで`/exit`を送り、wrapperが死ねば今のとおりrunを手放すこと、引き継ぎの後に待ちを続けること、adoptで空きslotを要さずに待ちを引き継ぐこと、人がダイアログに答えた2つのrunがその場で戻り1つが`--parallel`を超えること、`--max-waiting 0`で待たないこと、戻り待ちのrunが`status`の`waiting.count`と`returning`に数えられ、上限1をそれが埋めているあいだにslotのrunが聞くと`run_waiting_deferred`になること。`src/domain/waiting.rs`のunit testがイベントからの状態と集計と`WaitCount`の数え方を確かめる。
