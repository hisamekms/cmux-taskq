---
id: adr-0062
type: adr
title: 人の答えを待つrunをslotから外し、leaseを持ったまま軽く見張り、待ちの数に上限を付け、待ちが終わったrunを新しいclaimより先にslotへ戻す
status: accepted
created: 2026-09-26
updated: 2026-09-26
accepted_on: 2026-09-26
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - operations
related:
  - adr-0007
  - adr-0022
  - adr-0025
  - adr-0027
  - adr-0034
  - adr-0039
  - adr-0044
  - adr-0045
  - adr-0047
  - adr-0049
  - design-supervisor-lifecycle
  - design-persistence
---

# ADR-0062: 人の答えを待つrunをslotから外し、leaseを持ったまま軽く見張り、待ちの数に上限を付け、待ちが終わったrunを新しいclaimより先にslotへ戻す

## Context

supervisorは`--parallel N`（既定4）個のslotでrunを動かす（[supervisor-lifecycle](../design/supervisor-lifecycle/supervise.md)の`supervise`）。slotの数は`Supervisor::slots`の長さで、`fill_slots`は次の順に動く: adopt（slotに空きがあるときだけ）→ `recover_dead_runs`（slotを使わない）→ landingの答えの適用（空きがあるときだけ）→ `apply_triage_answers`（空きを見ない）→ `needs_session`のresume（空きがあるときだけ）→ triage（空きがあるときだけ）→ 終わったrunの掃除（slotを使わない）→ claim（空きがあるあいだ）。`drive`は`fill_slots`を、claimを止めておらず停止要求も無いときだけ呼ぶ。slotに居るrunのphaseは`Session` / `Validating` / `Review` / `Revise` / `Exiting` / `Resume` / `AwaitingSlot` / `Landing` / `Triage`で、どれもslotを1つ占める。

人の答えを待つあいだもrunはslotを持ち続ける。

- `Session`（最初のsession。`SessionWatch`）: workerの`worker_question`（[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定2）、ダイアログの`answer_prompt`、receiptの無いidleの`stalled`（[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定29・30）。`SessionWatch`はreceiptの後に`/exit`を送らず（ADR-0027の決定1。idleになれば`validating`へ進む）、このphaseの`stuck_exit`は、wrapperが黙ったsessionに送った`/exit`（`exit_for_silence`）か、ADR-0027より前のsupervisorが送った`/exit`の時間切れだけから出る。
- `Exiting`（verdictの後の`ExitWatch`）: `stuck_exit`（ADR-0047の決定25）と、送信の確認で開く`answer_prompt`。
- `Revise` / `Resume`: 差し戻しや解消依頼の最中のworkerが打つ`worker_question`と、送信の確認（`submit_unconfirmed` / `input_not_ready`）で開く`answer_prompt`。今の実装では答えを配送するのは`SessionWatch`だけで（`Revise` / `Resume`での配送はtask 238が扱う）、どちらのphaseも`resume_timeout`の壁時計とidleの判定で段を終える。

待っているsessionはCPUをほとんど使わないのに、`--parallel`の枠を占める。2026-09-26の夜、task 309と361の`stuck_exit`（asks 79・80）が04:10〜07:07のあいだ4つのうち2つのslotを塞いだ。直近5日で夜間の`stuck_exit`は5件、待ちの合計は18時間だった。

一方、今でもslotを外しているものがある。

- `approve_landing`（ADR-0027）と`decide`（triageとresumeの使い切り）: askを開く前に`/exit`でsessionを終わらせてworkspaceを閉じ、leaseを返す。runは`awaiting_integration` / `failed`のままDBに残り、答えが来たら`apply_landing_answers` / `apply_triage_answers`が適用する。生きているsessionが無いので、待ちのあいだ見張るものが無い。
- `needs_session`のresumeの`stuck_exit`: resumeは`unresolved`でleaseを手放し、`resume_parked_runs`はwrapperが生きているあいだ手を出さない（この文書の「待ち」とは別で、今のコードの`parked`はこのleaseの無い`needs_session`のrunを指す）。
- `queue_hold`（ADR-0047の決定42）: 認証やコストの詰まりで、openなあいだclaimとjobを控える。slotを空けても他のrunも同じ理由で止まるので、slotを外す意味が無い。

goal 41の人の決定（2026-09-26）: 夜間などに人が答えられないあいだslotを使い切る問題を、待ちのrunをslotから外すことで解く（案A）。`stuck_exit`の原因はgoal 34のtask 353・354・355が別に減らす。askの既定の答えを時間切れで適用する案（C）と夜間のclaim順の変更（D）は採らない。待っているsessionのメモリを守るため、待ちの数には上限を付ける。人の判断をruntimeが代わりに下すことはしない。

### 測定（2026-09-26、このhost）

16GBのメモリと8コアのhostで、動いているClaude Codeのプロセスは1つあたり常駐メモリ（RSS）が270〜580MB（6プロセスで合計約2.7GB）、cmux本体は1プロセスで約1.0GBだった。workspaceを1つ足してもcmuxのプロセスは増えず、増えるのはsessionのClaude Codeと、そのterminalのshellとwrapper（数MB〜数十MB）である。

## Decision

この文書で**待ち**（waiting）は、「生きているsessionを持つrunが、人の答えか人の操作だけを待っていて、supervisorにはsessionへ送るものもrunを進める処理も無い」状態を指す。待ちのrunは`--parallel`のslotに数えない。

### (a) 待ちにする条件

1. **待ちにするのは次の表のphaseとaskの組だけにする。** runのsessionが生きていて（wrapperが`exited_at`を記録しておらず、pidが生きている）、そのrunに表のkindのaskが開いている（未回答でcloseされていない）とき、そのrunを待ちにする。askが人に届く経路（今の実装の直接のask、ADR-0047の決定25・29・30・31が置き換える復旧jobの`escalate`の後のask）を問わず、inbox宛てのaskが開いたことを見たtickで待ちに移す（猶予は置かない。askは人に届いた時点で、人が答えるまで数分から数時間かかる）。

   | phase | ask | 待ちが終わる出来事（決定2） |
   | --- | --- | --- |
   | `Session` | `worker_question` | 答えが書かれた（`ask_answered`） |
   | `Session` | `answer_prompt` | ダイアログが消えた（`prompt_cleared`）か、sessionが動いた |
   | `Session` | `stalled` | sessionが動いた（ADR-0047の決定30の「askが開いている間にsessionが自分で動いた」） |
   | `Exiting` | `stuck_exit` | sessionが終わった（`session_exited`か、`ExitWatch`が終わったとみなすwrapperの死） |
   | `Exiting` | `answer_prompt` | sessionが動いたか、終わった |

   次は待ちにせず、今のままslotに数える。
   - `Session`の`stuck_exit`: wrapperが黙ったsessionか、ADR-0027より前の`/exit`の名残で、どちらもまれである。wrapperが黙ったrunはadopt（ADR-0039の決定1(c)）の対象にならず、`SessionWatch`はpidの死をerrorとして扱うので、決定11の引き継ぎと決定2の終わりを定められない。
   - `Revise` / `Resume`の`worker_question`と`answer_prompt`: 今の`ReviseWatch` / `ResumeWatch`はダイアログを検知せず、`resume_timeout`の壁時計とidleの判定で段を終えるので、待ちのあいだに段が時間切れになるか、`worker_question`を打ったworkerがidleになった時点で段が終わる。この組を待ちにするには、task 238の配送に加えて、待ちのあいだ段の計時を止める規則と、`worker_question`のidleで段を終えない規則が要る。それを決める変更は、このADRを置き換える統合ADRで行う（後続）。
   - `queue_hold`に入ったrun（決定6の最後の項）。

2. **待ちの終わりは、askの答えではなく、待っている相手の出来事で決める。** `worker_question`だけは答えそのものをruntimeが配送するので、答えが書かれた時点で待ちが終わる。`answer_prompt` / `stalled` / `stuck_exit`の答えは、人がworkspaceで何をするか（あるいは何もしないか）の指示で、runtimeは答えを受けて何も送らない（ADR-0047の決定29〜32、`dagq-recover` skill）。`stuck_exit`や`stalled`の`wait`の答え、inboxの`ask close`で待ちを終えると、止まったままのsessionがslotに戻って同じ問題が起きるので、これらはsessionの変化（表の右の列）で終える。どのkindでも、runが終わった（`recover`・abandon・taskのcancel）かleaseを失ったら待ちも終わる。
   - 「sessionが動いた」は、待ちに入ったときより新しい送信のmarker（`prompt-submit.json`）・idle marker・receiptのどれかがあることで判定する（ADR-0047の決定31の「処理された印」と同じ）。人がworkspaceに打ち込んだ入力もここに入る。
   - **待ちはaskではなくrunに付く。** 1つの待ちは、それを始めたaskと、待ちのあいだに同じrunで開いた表のaskをすべて持ち、どれか1つの終わりの出来事で終わる。待ちのあいだに開いたaskは`run_waiting_ask_added`（決定5）で足す。`stalled`の`wait`の答えは今の実装（`watch_ask`）でaskをすぐ閉じて計時をやり直すが、待ちは終わらず、sessionが動くまで続く。計時の後に開く新しい`stalled`のaskはその待ちに足す。
   - **同じaskで待ちを2度始めない。** 終わった待ちが持っていたaskは、開いたままでも（送信の確認の`answer_prompt`はsessionが終わるか入力欄が準備できるまで閉じない。`intervene`と答えた`stalled`は答えのままclose待ちに残る）、次の待ちを始める理由にしない。新しい待ちは、前の待ちが終わった後に開いたaskでだけ始める。

3. **今のslotの外の仕組みとは分けたままにする。** `approve_landing`と`decide`はsessionを閉じてleaseを返すので、生きているsessionも見張るものも無く、待ちの上限（決定7）にも数えない。待ちはsessionを閉じない。閉じないのは、閉じると`worker_question`の答えを受ける文脈や、人が答えるダイアログそのものが無くなるからである。`needs_session`のresumeの`stuck_exit`（leaseを手放す）と`queue_hold`は今のままにする（Contextの理由）。`planner_question`と`blocked`はrunに紐づかないので対象外。

4. **人の判断を代わりに下さない。** 待ちに時間の上限は無く、待ちが長くなってもaskに答えを書かず、sessionを閉じず、runを取り消さない。待ちの時間は決定12・13で見えるようにするだけである。

### (b) 表し方

5. **runのstatusは増やさず、supervisorのslotの印とrun_eventsで表す。** 待ちはrunのlifecycleの段とは直交する（`running`でも`awaiting_integration`でも待ちになりうる）。statusを増やすと、adoptの条件（ADR-0039の決定1(a)）、`recover`、`doctor`のblockers、`run_attention`、`stats`の段の集計、`integrate`など、statusで分岐するすべての箇所に同じ意味の分岐が要る。そこで次のようにする。
   - applicationの`Slot`に待ちの印（`waiting: Option<Waiting>`。`Waiting`は待ちが持つaskのIDとkind・待ちに入った時刻、待ちが終わっていればその時刻と理由）を足し、phase（`SessionWatch` / `ExitWatch`）はそのまま持つ。待ちのあいだphaseの状態は変えないので、待ちが終わればphaseの続きからそのまま進む。slotの数は「待ちの印の無いslot」の数にする（`slots.len()`の代わりに数える関数を1つ置き、`fill_slots`・adopt・resume・triageの空きの判定はそれを使う）。待ちのrunも`slots`に居るので、ループの終了判定（`slots.is_empty()`）は待ちのrunが残っていれば続く。
   - run_eventsに次のkindを足す（ADR-0034のdomain event。attentionではない）。run_eventsの表は変えない。
     - `run_waiting_started`: 待ちに入った。payloadは`ask_id`、`ask_kind`、`phase`（`session` / `exit`）、`status`（runのstatus）、`waiting`（入った後の待ちの数）、`limit`（決定7の上限）。
     - `run_waiting_ask_added`: 待ちのあいだに開いた表のaskを待ちに足した。payloadは`ask_id`、`ask_kind`。
     - `run_waiting_ended`: 待ちが終わった。payloadは終わらせた`ask_id`と`ask_kind`（runの終わりとleaseの喪失ではnull）、`cause`（`answered` / `dialog_cleared` / `session_moved` / `session_exited` / `queue_hold` / `run_ended` / `lease_lost`）、`waited_secs`（`run_waiting_started`からの秒）。
     - `run_slot_regained`: 待ちが終わったrunがslotに戻った。payloadは`slot_wait_secs`（`run_waiting_ended`からの秒）、`over_parallel`（戻った後のslotの数が`--parallel`を超えたか。決定10）。`cause`が`run_ended` / `lease_lost`の待ちには書かない。
     - `run_waiting_deferred`: 決定7。
   - 待ちの状態はこれらのイベントから再導出できる（最新の`run_waiting_started`の後に`run_waiting_ended`が無ければ待ち、`run_waiting_ended`の後に`run_slot_regained`が無く`cause`が`run_ended` / `lease_lost`でなければ戻り待ち。待ちが持つaskは`run_waiting_started`と`run_waiting_ask_added`の`ask_id`）。supervisorの再起動・引き継ぎ・`status`・`stats`はこれを読む（決定11〜13）。

### (c) 待ちのrunの見張り方

6. **leaseを持ち続け、同じプロセスが同じtickで、sessionに何も送らない見張りだけを続ける。**
   - **lease**: 待ちのrunはleaseを返さない。heartbeatのthreadは今までどおり自分のtokenのすべてのleaseを更新するので、待ちのあいだもleaseはstaleにならず、他のsupervisorのadopt（ADR-0039の決定1）も`recover_dead_runs`も起きない。leaseを返すと、leaseの無い未完了のrunとして`recover run`のattention（[ADR-0025](0025-leaseless-unfinished-run-is-a-recover-run-attention.md)）になり、生きているsessionの持ち主がいなくなる。tickの先頭のleaseの確認（lease行が自分のtokenか）も今までどおり行い、失っていればslotと同じく退く（`cause: lease_lost`）。
   - **続ける見張り**（待ちの終わりと、runの異常を見逃さないため）: wrapperのheartbeat・`exited_at`・pidの生死（sessionの終了とwrapperの沈黙の検知。[wrapperが黙ったsession](../design/supervisor-lifecycle/silent-wrapper.md)の`wrapper_heartbeat_expired`の記録を含む）、askの状態（答え・close）、idle marker・送信のmarker・receiptのmtime（sessionが動いたか）、`answer_prompt`で待つ`Session`のrunの画面の読み取り（ダイアログが消えたか。今の`watch_prompt`と同じ間隔）、`stalled`の`wait`の後の計時と、新しい`stalled`のaskを開くこと（askを開くことはsessionに何も送らない）。
   - **待ちの終わりで行う後始末**: 今のphaseのpollが出来事を見たときに行うaskの後始末は、待ちの中でも同じtickで行う。sessionが終わったら`stuck_exit`と`answer_prompt`のaskを閉じ（`close_stuck_exit_asks`、`close_answer_prompt_asks`）、`stalled`のaskを`ended`で閉じる。ダイアログが消えたら`prompt_cleared`を記録して`answer_prompt`のaskを閉じる。sessionが動いたら`stalled`のaskを`moved on`で閉じる。こうして、終わったsessionへの`/exit`を人に頼むaskが、slotの空きを待つあいだ開いたままにならないようにする。
   - **止めるもの**: sessionへ送るものと、runを進める処理はすべて止める。答えの配送（`deliver_answers`）、促し（ADR-0047の決定30）、送信の確認とEnterの送り直し（決定31）、`/exit`とその再試行（決定25）、既知のダイアログへのキー（決定29）、復旧job（`RecoveryWatch`。ADR-0047の決定39・40）の新しい起動と、その`send_instruction` / `stop_processes`の適用。待ちに入るときに復旧jobが走っていれば、待ちに入らない（runは人ではなく復旧jobを待っている。jobが`escalate`して開いたaskで待ちに入る）。待ちのあいだに送る必要が生じたら、それを待ちの終わり（`session_moved`など）として扱い、slotに戻ってから送る。runのstatusを変える書き込み（`supervision_finished`、workspaceのclose、`AfterExit`の処理）もslotに戻ってから行う。
   - **認証の切れ**: 画面の読み取りでログインの切れ（ADR-0047の決定42の`auth_required`）を見つけたら、今のとおりrunを`queue_hold`のaskに足し、待ちを`cause: queue_hold`で終えてslotに戻す（決定10の人が動かしたsessionと同じく空きを待たない）。`queue_hold`のあいだはclaimもjobも控えるので、slotに数えても他のrunを止めない。
   - **答えの配送**: `worker_question`の答えは、待ちのあいだに書かれたことを検知し（`cause: answered`）、送信はslotに戻ったtickの`deliver_answers`で今のとおり行う。答えを送るとworkerが作業を再開してCPUを使うので、slotを持つまで送らない。

### (d) 待ちの上限

7. **待ちの数に上限を付ける。`supervise --max-waiting N`（既定4、0で待ちを使わず今の振る舞い）。** `up`も同じ名前の引数を受けてsupervisorに渡し、`supervisors`の登録に`--parallel`と並べて記録する（null可の列を足す互換のmigration。[ADR-0045](0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)の決定6。このADRでschemaを変えるのはこの列だけ）。
   - **根拠**: 待ちのsessionはCPUをほとんど使わないが、メモリを持ち続ける。Claude Codeのsessionは1つあたり約0.3〜0.6GB（Contextの測定）で、cmuxのworkspaceを足しても増えるのはほぼsessionの分だけである。既定の4なら、走っているsession（`--parallel`の4）と合わせて開いているworkerのsessionは最大8、待ちの分は最大で約2.3GBで、16GBのhostにinbox・planner・build（ADR-0049）と並べて収まる。夜間の実績（5日で5件、同時に最大2件）にも余裕がある。上限は`--parallel`と同じ数を既定にし、hostに合わせて人が変える。
   - **上限に達したとき**: 新たに待ちの条件を満たしたrunは、今までどおりslotに居て数えられる（claimは止めない。slotが減るだけで、今より悪くはならない）。そのrunには`run_waiting_deferred`（`ask_id`、`ask_kind`、`waiting`、`limit`）をaskごとに1回だけ記録する。待ちが1つ空いたら、次のtickで、slotに居て待ちの条件を満たすrunのうちaskの古いものから待ちに移す。
   - 上限は待ちに入れる数だけを制限する。決定11で再起動や引き継ぎのときに組み立て直した待ちが上限を超えていても（上限を下げて起動し直したときなど）、slotに戻さずそのまま待ちにし、上限を下回るまで新しい待ちを入れない。

### (e) 再開

8. **待ちが終わったrunは、新しい仕事より先に、待ちが終わった順にslotへ戻す。** 戻す処理（`return_waiting_runs`）は`fill_slots`の外に置き、`drive`のループの毎回、`fill_slots`より前に、claimを止めているときも停止要求の後（drain中）も行う。drainは待ちのrunの終わりを待つので（決定11）、戻す処理がdrain中に止まると、答えが来たrunがslotに戻れずにループが終わらない。戻り待ちのrunを`run_waiting_ended`の古い順に、slotの数が`--parallel`未満のあいだslotへ戻し、`run_slot_regained`を記録する。そのあとで`fill_slots`が今の順で空いたslotを使う。戻り待ちのrunは生きているsessionを持ち（`stuck_exit`ではsessionが終わった直後で）、人がすでに答えているので、その先を他の仕事より先に進める。

9. **runtimeが再開させるrunは、slotが空くまで待つ。** `worker_question`の答え（`cause: answered`）と、sessionが終わった`stuck_exit`（`cause: session_exited`）がこれにあたる。
   - `worker_question`: slotに戻ったtickで`deliver_answers`が答えを送る。以後は今の`SessionWatch`のとおり。
   - `stuck_exit`: 人（inboxが`dagq-recover` skillの`reference/stuck-exit.md`に従う）が`/exit`してsessionが終わると、待ちが終わり、askは決定6のとおりその場で閉じる。slotに戻ったtickで`ExitWatch`の続きをそのまま進める: 画面を保存し、workspaceを閉じて`AfterExit`のとおり（`Land`なら`AwaitingSlot`から着地へ、`Ask`なら`approve_landing`のask、`ReviewFailed`、`Rest`）。

10. **人が直接動かしたsessionは、slotの空きを待たずに戻す。** `answer_prompt`（`dialog_cleared` / `session_moved`）、`stalled`（`session_moved`）、`queue_hold`は、人がworkspaceでダイアログに答えるか入力したことでsessionがすでに作業を再開しているか、claimが控えられている。runtimeはsessionを止められないので、戻り待ちにせず、見つけたtickでslotに戻す（`run_slot_regained`の`over_parallel`が`true`になりうる）。slotの数が`--parallel`以上のあいだは、今の空きの判定のとおり、adopt・landingの答え・resume・triage・claimは行わない。slotを使わない`recover_dead_runs`と掃除は今のまま行い、空きを見ない`apply_triage_answers`は、`retry`（taskを`ready`に戻すだけ）と`cancel`は今のまま適用し、`resume`の答えはrunを`needs_session`にするだけで、そのresumeは空きを見るresumeの段が始めるので、どれもslotを増やさない。超えるのは人が同時に答えたsessionの数だけで、開いているworkerのsessionの数は`--parallel`と待ちの上限の和を超えない。

### (f) supervisorの入れ替えと再起動

11. **待ちの状態は、run_eventsとaskから組み立て直す。**
   - **execの引き継ぎ**（ADR-0045の決定10。まだ実装されていない）: 引き継ぎを実装するtaskは、execしたプロセスが同じtokenのleaseを持つrunのslotをDBから組み立て直すとき、決定5のイベントから待ちの印も組み立てるようにする（待ちに入った時刻と持っていたaskはイベントから）。引き継ぎの要求を受けたsupervisorは、待ちのrunを区切りの判定に入れない（待ちのrunには進行中の短い処理が無い）。待ちのsessionはwrapperの下で動き続ける。
   - **adopt**（ADR-0039）: 待ちになりうるrunは`running`（`Session`）と、verdictの後の`awaiting_integration` / `needs_session` / `failed`（`Exiting`）で、今のadoptは`running` / `validating` / `awaiting_integration`を引き継ぎ、`exit_requested`から`ExitWatch`の終了待ちを組み立てる（`needs_session`と`failed`の`Exiting`は今のadoptの対象外のまま。ADR-0047の決定24のadoptの拡張が入れば同じ規則で待ちも組み立てる）。このADRを実装するtaskは次を足す: 引き継いだrunの待ちの印を決定5のイベントから組み立てる。待ちのrunは空いたslotを要しないので、adoptの段をslotの空きの判定から外し、slotが埋まっていても待ちの上限に空きがあれば、イベントの上で待ちのrunを引き継いで待ちとして持つ。上限に空きが無ければ、今のとおりslotの空きを待って引き継ぎ、slotに居るrunとして扱う（決定7の`run_waiting_deferred`）。イベントの上で待ちが終わっているrunは戻り待ちとして引き継ぎ、決定8の順でslotに戻す。wrapperが黙ったrunを引き継がないこと（ADR-0039の決定1(c)）は変えない。
   - **drain**（SIGINT/SIGTERMの1回目、`down --wait`）: 今のとおり、待ちのrunもactive runとして終わりを待つ。決定8のとおり、drain中も待ちが終わったrunはslotに戻って進む。待ちのrunのleaseを残して終了し、次のsupervisorにadoptさせる案は採らない（Alternatives）。drainが人の答えまで長引くときは、人がaskに答えるか、ADR-0045の引き継ぎ（待たない入れ替え）を使う。

### (g) 見せ方

12. **`status`は待ちのrunと、slotと待ちの数を返す。** どれもDBから組み立て、supervisorのプロセスに問い合わせない。
   - supervisorごとに`slots: {used, parallel}`と`waiting: {count, limit}`を返す。`used`は、そのsupervisorのtokenのleaseを持つ`integrating`でないrunの数（今の`stats`の`idle_slots`が`active_runs`から数えるのと同じ集合）から、決定5のイベントで待ちか戻り待ちのrunを引いた数、`count`は待ちのrunの数（戻り待ちを含まない）、`limit`は登録の`--max-waiting`。戻り待ちのrunは`used`にも`count`にも入らず、下の`waiting`の`state: returning`で見える。決定10のあいだ`used`は`parallel`を超えうる。
   - `waiting`（新しい配列）に待ちと戻り待ちのrunを1件ずつ返す: `run_id`、`task_id`、`asks`（待ちが持つaskのIDとkind）、`phase`、`status`、`state`（`waiting` / `returning`）、`since`（待ちに入った時刻）、`waited_secs`、戻り待ちなら`ended_at`と`cause`。
   - 待ちはattentionを増やさない。人に届くのは今のとおり各askの`ask_opened`である。

13. **`stats`は待ちの回数と時間を返す。** ADR-0049の`stats`の項目に`waiting`を足す: `--since`以降の`run_waiting_started`の件数（始めたaskの`ask_kind`ごと）、`waited_secs`の合計・中央値・最大値（`ask_kind`ごと。夜間かどうかは分けない）、`slot_wait_secs`の中央値と最大値、`over_parallel`の件数、`run_waiting_deferred`の件数（上限に当たった回数。上限の見直しの材料で、observerが読む）。`waited_secs`の合計は、待ちがslotを塞いでいたら失われていたslotの時間である。`stats`の`idle_slots`のalert（observerが読み、taskの無い`blocked`のaskにする閾値。ADR-0044の決定4をADR-0047が引き継ぐ）は、決定12の`used`で空きを数える（待ちのrunを埋まったslotに数えない）。

### (h) goal 39との関係

14. **待ちのrunのbaseが古くなる問題はこのADRで扱わず、goal 39に分ける。** 待ちはrunの時間を延ばすので、待っているあいだにmainが進み、着地のときの衝突が増えうる。これは待ちの無い長いrunにも起きる問題で、今の仕組み（ADR-0027の決定4の`merge-tree`の事前判定、`integrate`の衝突から`needs_session`のresume、ADR-0047の決定24の衝突だけのresumeを数えない上限）が扱う。このADRは待ちから戻るときにrebaseもbaseの読み直しもしない。goal 39が「戻るときにmainへ載せ直すか」を決めるときは、決定5の`run_slot_regained`がその時点になり、決定13の`waited_secs`が効果を測る材料になる。

## Alternatives

- **runのstatusに`waiting`を足す。** 決定5の理由で採らない。statusは段を表し、待ちは段と直交する。
- **待ちのrunのleaseを返し、`approve_landing`と同じくDBに残す。** 生きているsessionの持ち主がいなくなり、ADR-0025の`recover run`のattentionになり、adoptと`recover`の規則にleaseの無い生きたrunの例外を足す必要がある。答えの配送とsessionの終了の見張りも、leaseを持たないプロセスが行うことになる。採らない。
- **待ちのrunのsessionを閉じ（`/exit`）、答えが来たら`claude --resume`で開き直す。** メモリの上限は要らなくなるが、`stuck_exit`はそもそも`/exit`が効かないsessionで、`answer_prompt`はダイアログそのものが待っている相手なので閉じられない。`worker_question`は閉じられるが、開き直しは解消依頼と同じ起動・送信の確認を要し、文脈の読み直しに時間とトークンを使う。採らない。
- **askが答えられた時点で、kindを問わず待ちを終える。** `stuck_exit` / `stalled`の`wait`の答えや`ask close`でsessionが止まったままslotに戻り、同じ詰まりが起きる。決定2のとおりsessionの変化で終える。
- **`Revise` / `Resume`の待ちもこのADRで決める。** 段の計時を止める規則と、`worker_question`のidleで段を終えない規則が、`ReviseWatch` / `ResumeWatch`の終わり方そのものを変える。task 238の配送が入ってから、その実装に合わせて決める（決定1）。
- **待ちに上限を付けない。** 人の決定（goal 41のconstraints）に反する。夜間に待ちが積み重なると、開いているsessionのメモリがhostを圧迫する。
- **上限に達したらclaimを止める。** 待ちの無い今より悪くなる（slotが空いていてもclaimしない）。決定7のとおり、上限を超えた分は今までどおりslotに数える。
- **戻り待ちのrunをclaimと同じ順（優先度）で並べる。** 人がすでに答えたrunは、そのsessionを開いたまま待たせるほど、人の答えから結果までが延び、待ちの上限も空かない。決定8のとおり他の仕事より先にする。
- **drainで待ちのrunを待たずに終了し、leaseをstaleにして次のsupervisorにadoptさせる。** wrapperが生きていればADR-0039でadoptされるが、supervisorの居ないあいだは答えの配送もsessionの終了の見張りも止まり、`down --wait`の「走っているものが終わった」という意味（ADR-0045の決定8の非互換のmigrationの前提など）が崩れる。待たない入れ替えはADR-0045の引き継ぎが担う。採らない。
- **夜間に既定の答えを時間切れで適用する（案C）、夜間のclaim順を変える（案D）。** 人の決定で採らない。

## Consequences

- 人が答えられないあいだも、待ちのrunの分のslotで他のtaskが進む。2026-09-26の夜の例（`Exiting`の`stuck_exit`）では、04:10〜07:07の約3時間、2つのslotが他のtaskに使えた。
- 開いているworkerのsessionは最大で`--parallel`と`--max-waiting`の和になり、その分のメモリを使う。hostに合わせて`--max-waiting`を下げられる。
- supervisorのslotの数え方が「slotの長さ」から「待ちの印の無いslotの数」に変わる。`fill_slots`とその各段の空きの判定、戻す処理、adoptの条件、`status`・`stats`の`idle_slots`がこの数え方を使う。
- 人が直接動かしたsessionのあいだは、slotの数が一時的に`--parallel`を超えうる（決定10）。そのあいだ新しい仕事は始まらない。
- `Revise` / `Resume`の待ちと`Session`の`stuck_exit`は、このADRでは今までどおりslotに数える（決定1）。
- 実装は後続のruntimeのtaskで行う。分け方の目安: (1) slotの待ちの印と数え方、決定1・2の`Session` / `Exiting`の待ちの出入り、決定5のイベント、決定6の見張り・後始末・止めるもの、決定8〜10の戻し方（`fill_slots`の外の`return_waiting_runs`）。(2) `--max-waiting`と登録の列（互換のmigration）、決定7の上限と`run_waiting_deferred`、`up`の引数。(3) 決定11のadoptでの組み立て直し（execの引き継ぎはADR-0045の決定10を実装するtaskが行う）。(4) 決定12・13の`status`と`stats`。(5) plugin の skill（inbox・`dagq-recover`）とdesign文書（supervisor-lifecycleの`supervise`・`ask`・`status`・`stats`、persistence）の更新。
- 待ちの時間と上限に当たった回数が`stats`に出るので、`--max-waiting`の既定値と、goal 39（baseの古さ）の要否を実績で見直せる。
