---
id: adr-0071
type: adr
title: 人の答えを待つrunを、最初のsessionと/exitに加えて差し戻しと解消依頼の段でもslotから外し、待ちのあいだ段の計時を止め、leaseを持ったまま軽く見張り、戻り待ちも含めて待ちの数に上限を付け、待ちが終わったrunを新しいclaimより先にslotへ戻す（ADR-0062を統合）
status: accepted
created: 2026-09-26
updated: 2026-09-26
accepted_on: 2026-09-26
supersedes:
  - adr-0062
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - operations
related:
  - adr-0022
  - adr-0027
  - adr-0034
  - adr-0039
  - adr-0047
  - adr-0049
  - adr-0054
  - adr-0062
  - adr-0073
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-waiting
  - design-persistence
---

# ADR-0071: 人の答えを待つrunを、最初のsessionと`/exit`に加えて差し戻しと解消依頼の段でもslotから外し、待ちのあいだ段の計時を止め、leaseを持ったまま軽く見張り、戻り待ちも含めて待ちの数に上限を付け、待ちが終わったrunを新しいclaimより先にslotへ戻す（ADR-0062を統合）

## Context

[ADR-0062](0062-runs-waiting-for-a-person-leave-the-slot.md)は、人の答えを待つrunを`--parallel`のslotから外す**待ち**を決めた（goal 41）。対象は`Session`（最初のsession。`SessionWatch`）の`worker_question` / `answer_prompt` / `stalled`と、`Exiting`（verdictの後の`ExitWatch`）の`stuck_exit` / `answer_prompt`だけで、決定1は`Revise` / `Resume`の組（`worker_question`と`answer_prompt`）を外し、「段の計時を止める規則と、`worker_question`のidleで段を終えない規則が要る。task 238の配送が入ってから、このADRを置き換える統合ADRで決める」とした。このADRがその統合ADRである。ADR-0042のとおりADR-0062を丸ごと置き換え、ADR-0062の決定1〜14は同じ番号で引き継ぐ（対応は「ADR-0062からの引き継ぎ」の表）。

### task 238とtask 445の着地後の実装（2026-09-26のmain）

- **`ReviseWatch`**（`src/application/supervise/revise.rs`。`revise`のverdictと、`merge-tree`の事前判定が見つけた衝突の解消依頼。ADR-0027の決定2・4）: task 238で`SessionWatch::revising`を中に持ち、毎回のpollで`deliver_answers`（`worker_question`の答えの送信）と`watch_prompt`（ダイアログの検知と、復旧jobを経た`answer_prompt`のask）を回す。closeされていない`worker_question`があるあいだはidleでも段を終えず`resume_timeout`も数えない（その判定が時間切れの判定より前にある）。答えを送ったとき、または`worker_question`が最後の入力より後の秒にcloseされたとき（手での配送）は、その時刻を入力の時刻（`input_at`）にし、段の計時（`sent`）をその時点からやり直す。idle markerは`input_at`より新しいものだけを数える。`domain::session_takes_answers`は`running`と、revise中（最後の段が`revise_requested`か要求された`conflict_precheck`で、その後に`exit_requested`が無く、leaseがある）の`awaiting_integration`を答えの配送先にする。
  - 残る穴: `answer_prompt`のaskが開いているあいだも`sent`は進み、`resume_timeout`を過ぎれば段は`Ended`（`approve_landing`のaskへ）になる。
- **`ResumeWatch`**（`src/application/supervise/resume.rs`。`needs_session`のrunのresume）: task 238の配送は入っていない。答えを送らず、ダイアログを検知せず（`answer_prompt`は解消依頼の送信の前の`input_not_ready`と、送信の確認の`submit_unconfirmed`だけから開く）、`worker_question`を打ったworkerがidleになると「解消するreceiptの無いidle」として`/exit`を送って段を終える。runは`needs_session`なので`session_takes_answers`も偽で、`status`はその答えを`delivering`にしない。`/exit`の時間切れは`stuck_exit`のaskを開いてleaseを手放す（ADR-0062のContextの`needs_session`のresumeの`stuck_exit`）。
- **待ち**（task 445。`src/application/supervise/waiting.rs`、`src/domain/waiting.rs`、[人の答えを待つrun](../design/supervisor-lifecycle/waiting.md)）: ADR-0062を実装したが、次の3点でADR-0062の文言から外れた。task 445のreviewがこれをconcern（ask 107）にし、inboxが3点とも実装の方が優れていると評価し、人の判断で`land`と答えた（2026-09-26）。このADRは3点を実装どおりに決定として取り込む（決定2・5・6・7・11）。ただし2の`status`の`waiting.count`は実装を決定7に揃えるほうに変える（決定12）。
  1. `lease_lost` / `run_ended`を書かず、`run_waiting_started`の後の`lease_acquired` / `lease_released` / `run_recovered` / `runtime_error`で待ちは無いとみなす。`cause`に`wrapper_silent`と`phase_changed`を足した。
  2. 戻り待ちのrunも`--max-waiting`に数える（`start_waits`はslotの外のrun、つまり待ちと戻り待ちの数で上限を判定する）。ただし`status`の`waiting.count`はまだ戻り待ちを含めない。
  3. `Session`の待ちのあいだに`SessionWatch`がまだ見ていないreceiptが現れたら、待ちのkindを問わず`session_moved`で終える。

goal 41のconstraints（人の決定、2026-09-26）: 夜間などに人が答えられないあいだslotを使い切る問題を、待ちのrunをslotから外すことで解く（案A）。askの既定の答えを時間切れで適用する案（C）と夜間のclaim順の変更（D）は採らない。待っているsessionのメモリを守るため、待ちの数には上限を付ける。人の判断をruntimeが代わりに下すことはしない。

### 測定（2026-09-26、このhost。ADR-0062から）

16GBのメモリと8コアのhostで、動いているClaude Codeのプロセスは1つあたり常駐メモリ（RSS）が270〜580MB（6プロセスで合計約2.7GB）、cmux本体は1プロセスで約1.0GBだった。workspaceを1つ足してもcmuxのプロセスは増えず、増えるのはsessionのClaude Codeと、そのterminalのshellとwrapper（数MB〜数十MB）である。

## Decision

この文書で**待ち**（waiting）は、「生きているsessionを持つrunが、人の答えか人の操作だけを待っていて、supervisorにはsessionへ送るものもrunを進める処理も無い」状態を指す。待ちのrunは`--parallel`のslotに数えない。**戻り待ち**は、待ちが終わり、slotの空きを待っているrunを指す（決定9）。「slotの外のrun」は待ちと戻り待ちのrunを合わせて指す。

### (a) 待ちにする条件

1. **待ちにするのは次の表のphaseとaskの組だけにする。**（変更: `Revise` / `Resume`の行を足した）runのsessionが生きていて（wrapperが`exited_at`を記録しておらず、pidが生きている）、そのrunに表のkindのaskが開いている（未回答でcloseされていない）とき、そのrunを待ちにする。askが人に届く経路（直接のask、ADR-0047の決定25・29・30・31の復旧jobの`escalate`の後のask）を問わず、inbox宛てのaskが開いたことを見たtickで待ちに移す（猶予は置かない）。

   | phase | 待ちにできるphaseの状態 | ask | 待ちが終わる出来事（決定2） |
   | --- | --- | --- | --- |
   | `Session` | `/exit`を送っておらず、wrapperが黙っておらず、復旧jobが走っていない | `worker_question` | 答えが書かれたかcloseされた（`answered`） |
   | `Session` | 同上 | `answer_prompt` | ダイアログが消えた（`dialog_cleared`）か、sessionが動いた（`session_moved`） |
   | `Session` | 同上 | `stalled` | sessionが動いた（ADR-0047の決定30の「askが開いている間にsessionが自分で動いた」） |
   | `Exiting` | sessionを持ち、復旧jobが走っていない | `stuck_exit` | sessionが終わった（`session_exited`か、wrapperの死） |
   | `Exiting` | 同上 | `answer_prompt` | sessionが動いたか、終わった |
   | `Revise` | wrapperが黙っておらず、復旧jobが走っていない | `worker_question` | 答えが書かれたかcloseされた |
   | `Revise` | 同上 | `answer_prompt` | ダイアログが消えたか、sessionが動いた |
   | `Resume` | `/exit`を送っておらず（`exit_requested`が無い）、wrapperが黙っておらず、復旧jobが走っていない | `worker_question` | 答えが書かれたかcloseされた |
   | `Resume` | 同上 | `answer_prompt` | ダイアログが消えた（解消依頼の送信の前の`input_not_ready`では、入力欄が準備できた）か、sessionが動いた |

   どの行でも、`Session`・`Revise`・`Resume`のsessionが終わった（`session_exited`）ら待ちは終わる（決定6の後始末と決定9の戻り待ち）。`Revise` / `Resume`の待ちは、決定15（段の計時）・決定16（`worker_question`のidle）・決定17（`Resume`の配送とダイアログの検知）が入って初めて成り立つ。それまでのruntimeはこの2行を待ちにしない（実装の順は「Consequences」）。

   次は待ちにせず、slotに数える。
   - **`Session`の`stuck_exit`**（(e)）: 引き続きslotに数える。このphaseの`stuck_exit`は、wrapperが黙ったsessionに送った`/exit`（`exit_for_silence`）か、ADR-0027より前のsupervisorが送った`/exit`の時間切れからだけ出て、どちらもまれである。wrapperが黙ったrunはadopt（ADR-0039の決定1(c)）の対象にならず、`SessionWatch`はpidの死をerrorとして扱うので、決定11の引き継ぎと決定2の終わりを定められない。待ちのあいだにwrapperが黙った`Session`のrunは、`/exit`を送るために待ちを`wrapper_silent`で終えてslotに戻す（決定6）ので、その後に開く`stuck_exit`もslotで待つ。
   - **`Resume`の`stuck_exit`**: `ResumeWatch`は`/exit`の時間切れで`stuck_exit`のaskを開き、`unresolved`としてleaseを手放す。`resume_parked_runs`はwrapperが生きているあいだ手を出さないので、すでにslotの外にある（leaseも持たないので待ちではない）。
   - **`Revise`の`stuck_exit`**: `ReviseWatch`は`/exit`を送らない。段が終わった後の`/exit`は`Exiting`で送るので、そこで出る`stuck_exit`は`Exiting`の行で待ちになる。
   - **`queue_hold`に入ったrun**（決定6の最後の項）。
   - **`stalled`の`Revise` / `Resume`**: どちらの段もstallを見ず（`resume_timeout`と決定16のidleで段を終える）、`stalled`のaskを開かない。

2. **待ちの終わりは、askの答えではなく、待っている相手の出来事で決める。**（変更: receiptの規則と、runの終わりとleaseの喪失の扱いを実装どおりにした）`worker_question`だけは答えそのものをruntimeが配送するので、答えが書かれた（またはaskがcloseされた）時点で待ちが終わる。`answer_prompt` / `stalled` / `stuck_exit`の答えは、人がworkspaceで何をするか（あるいは何もしないか）の指示で、runtimeは答えを受けて何も送らない（ADR-0047の決定29〜32、`dagq-recover` skill）。`stuck_exit`や`stalled`の`wait`の答え、inboxの`ask close`で待ちを終えると、止まったままのsessionがslotに戻って同じ問題が起きるので、これらはsessionの変化（表の右の列）で終える。
   - 「sessionが動いた」は、待ちに入ったときより新しい送信のmarker（`prompt-submit.json`）・idle marker・receiptのどれかがあることで判定する（ADR-0047の決定31の「処理された印」と同じ）。人がworkspaceに打ち込んだ入力もここに入る。比べる基準の時刻は、最初のaskを開いた秒の次の秒と待ちを始めた時刻の早い方にする（askの時刻は秒なので、askと同じ秒のmarkerは古いとみなす）。
   - **receiptで待ちを終える**（task 445の実装を取り込む）: `Session`の待ちのあいだに、`SessionWatch`がまだ見ていないreceiptが現れたら、待ちのkindを問わず（`worker_question`の待ちでも）`session_moved`で終え、slotの空きを待たずに戻す。receiptはsessionが動いた印で（上の項）、receiptが出たrunは`validating`へ進むべきだからである。askの後始末（receiptで閉じる`answer_prompt`など）は戻ったslotの`SessionWatch`が行う。`Revise` / `Resume`の待ちでは、`answer_prompt`だけを持つ待ちが、段の依頼の後に書き直されたreceipt（`ReviseWatch`では`sent_at`より新しいもの、`ResumeWatch`では`started_at`より新しいもの）で`session_moved`になって終わる（receiptはmarkerの1つ）。`worker_question`を持つ`Revise` / `Resume`の待ちは、書き直されたreceiptでは終えず、`worker_question`の答えかcloseで終える。決定16のとおり、closeされていない`worker_question`があるあいだ段はreceiptもidleも判定しないので、receiptで戻してもslotで進まず、そのaskは`consumed`になってもう待ちに入れず、人の答えまでslotを塞ぐからである（`Session`とは違う。`SessionWatch`はreceiptを見れば`worker_question`の有無によらず`validating`へ進む）。
   - **runの終わりとleaseの喪失**（task 445の実装を取り込む）: runが終わった（`recover`・abandon・taskのcancel）かleaseを失った待ちには、`run_waiting_ended`を書かない。leaseを失ったプロセスはrunに書かず、adoptした側がイベントから同じ待ちを続けるので、書くとその待ちを消してしまうからである。代わりに、`run_waiting_started`の後に`lease_acquired` / `lease_released` / `run_recovered` / `runtime_error`のどれかがあれば、その待ちは無いとみなす（決定5の再導出）。これらは、待ちを持っていたsupervisorがrunを失ったか、runが別の経路に移った印で、adoptと引き継ぎはこれらを書かない。
   - **待ちはaskではなくrunに付く。** 1つの待ちは、それを始めたaskと、待ちのあいだに同じrunで開いた表のaskをすべて持ち、どれか1つの終わりの出来事で終わる。待ちのあいだに開いたaskは`run_waiting_ask_added`（決定5）で足す。`stalled`の`wait`の答えは（`watch_ask`で）askをすぐ閉じて計時をやり直すが、待ちは終わらず、sessionが動くまで続く。計時の後に開く新しい`stalled`のaskはその待ちに足す。
   - **同じaskで待ちを2度始めない。** 終わった待ちが持っていたaskは、開いたままでも（送信の確認の`answer_prompt`はsessionが終わるか入力欄が準備できるまで閉じない。`intervene`と答えた`stalled`は答えのままclose待ちに残る）、次の待ちを始める理由にしない。新しい待ちは、前の待ちが終わった後に開いたaskでだけ始める。例外は`phase_changed`（決定11）で、上限のためにslotへ戻した待ちのaskは、後で同じaskで待ちに入れる。

3. **今のslotの外の仕組みとは分けたままにする。** `approve_landing`と`decide`はsessionを閉じてleaseを返すので、生きているsessionも見張るものも無く、待ちの上限（決定7）にも数えない。待ちはsessionを閉じない。閉じないのは、閉じると`worker_question`の答えを受ける文脈や、人が答えるダイアログそのものが無くなるからである。`needs_session`のresumeの`stuck_exit`（leaseを手放す）と`queue_hold`は今のままにする（決定1）。`planner_question`と`blocked`はrunに紐づかないので対象外。

4. **人の判断を代わりに下さない。** 待ちに時間の上限は無く、待ちが長くなってもaskに答えを書かず、sessionを閉じず、runを取り消さない。`Revise` / `Resume`の待ちのあいだも段の時間切れで段を終えない（決定15）。待ちの時間は決定12・13で見えるようにするだけである。

### (b) 表し方

5. **runのstatusは増やさず、supervisorのslotの印とrun_eventsで表す。**（変更: `phase`の値と`cause`の値と再導出を実装どおりにした）待ちはrunのlifecycleの段とは直交する（`running`でも`awaiting_integration`でも`needs_session`でも待ちになりうる）。statusを増やすと、adoptの条件（ADR-0039の決定1(a)）、`recover`、`doctor`のblockers、`run_attention`、`stats`の段の集計、`integrate`など、statusで分岐するすべての箇所に同じ意味の分岐が要る。そこで次のようにする。
   - applicationの`Slot`に待ちの印（`waiting: Option<Waiting>`。`Waiting`は待ちが持つaskのIDとkind・待ちに入った時刻・sessionが動いたかを比べる基準の時刻・画面を最後に読んだ時刻、待ちが終わっていればその時刻と理由）を足し、phase（`SessionWatch` / `ExitWatch` / `ReviseWatch` / `ResumeWatch`）はそのまま持つ。待ちのあいだphaseの状態は変えないので、待ちが終わればphaseの続きから進む（段の計時だけは決定15で戻るときにやり直す）。slotの数は「待ちの印の無いslot」の数（`used_slots()`）にし、`fill_slots`・adopt・landingの答え・resume・triage・claimの空きの判定はそれを使う。待ちのrunも`slots`に居るので、ループの終了判定（`slots.is_empty()`）は待ちのrunが残っていれば続く。
   - run_eventsに次のkindを置く（ADR-0034のdomain event。attentionではない）。run_eventsの表は変えない。
     - `run_waiting_started`: 待ちに入った。payloadは`ask_id`、`ask_kind`、`phase`（`session` / `exit` / `revise` / `resume`）、`status`（runのstatus）、`waiting`（入った後のslotの外のrunの数）、`limit`（決定7の上限）。
     - `run_waiting_ask_added`: 待ちのあいだに開いた表のaskを待ちに足した。payloadは`ask_id`、`ask_kind`。
     - `run_waiting_ended`: 待ちが終わった。payloadは終わらせた`ask_id`と`ask_kind`（askによらない終わりではnull）、`cause`、`waited_secs`（`run_waiting_started`からの秒）。`cause`は次のどれか。
       - `answered`: 持っている`worker_question`が回答されたかcloseされた。戻り待ちになる。
       - `session_exited`: sessionが終わった（wrapperが`exited_at`を記録したか、heartbeatが切れてpidも死んだ）。戻り待ちになる。
       - `dialog_cleared`: ダイアログが消えた（`Resume`の`input_not_ready`では入力欄が準備できた）。その場で戻る。
       - `session_moved`: sessionが動いた（決定2のmarkerかreceipt）。その場で戻る。
       - `queue_hold`: runがqueue_holdに入った（決定6）。その場で戻る。
       - `wrapper_silent`: `Session` / `Revise` / `Resume`の待ちのあいだにwrapperが黙った（pidは生きているのにheartbeatが切れた）。そのphaseの見張りが`/exit`を送るか段を終えるので、その場で戻す（決定6）。`Exiting`は`/exit`を送った後なので待ちを続ける。
       - `phase_changed`: 引き継ぎやadoptで組み立て直したphaseが待てないもの（例えばreviewからやり直すrun）だった、またはadoptのときに上限を超えていた（決定11）。その場で戻る。
       - `run_ended` / `lease_lost`は書かない（決定2）。ADR-0062のもとで書かれたものを読んだときは、その待ちは無いとみなす。
     - `run_slot_regained`: 待ちが終わったrunがslotに戻った。payloadは`slot_wait_secs`（`run_waiting_ended`からの秒）、`over_parallel`（戻った後のslotの数が`--parallel`を超えたか。決定10）。
     - `run_waiting_deferred`: 決定7。
   - 待ちの状態はこれらのイベントから再導出できる（`WaitState::of`）: 最新の`run_waiting_started`の後に、`lease_acquired` / `lease_released` / `run_recovered` / `runtime_error`も`run_slot_regained`も無く、`run_waiting_ended`も無ければ待ち、`run_waiting_ended`があれば戻り待ち（その`cause`が`run_ended` / `lease_lost`なら待ちは無い）。待ちが持つaskは`run_waiting_started`と`run_waiting_ask_added`の`ask_id`、終わった待ちのaskは`run_waiting_ended`より前の待ちのask。supervisorの再起動・引き継ぎ・`status`・`stats`はこれを読む（決定11〜13）。

### (c) 待ちのrunの見張り方

6. **leaseを持ち続け、同じプロセスが同じtickで、sessionに何も送らない見張りだけを続ける。**（変更: leaseの喪失とwrapperの沈黙と、`Revise` / `Resume`の見張りを足した）待ちのslotでは、phaseのpoll（`step`）の代わりに待ちの見張り（`watch_waiting`）を呼ぶ。
   - **lease**: 待ちのrunはleaseを返さない。heartbeatのthreadは自分のtokenのすべてのleaseを更新するので、待ちのあいだもleaseはstaleにならず、他のsupervisorのadopt（ADR-0039の決定1）も`recover_dead_runs`も起きない。leaseを返すと、leaseの無い未完了のrunとして`recover run`のattention（ADR-0054）になり、生きているsessionの持ち主がいなくなる。tickの先頭のleaseの確認（lease行が自分のtokenか）も行い、失っていれば他のslotと同じく退く。退くときはDBに書かない（決定2。引き継いだ側がイベントから待ちを組み立てる）。戻り待ちのrunもleaseの確認だけは続ける。
   - **続ける見張り**（待ちの終わりと、runの異常を見逃さないため）:
     - wrapperの`exited_at`・heartbeat・pidの生死。終了を記録したか、heartbeatが切れてpidも死んでいれば`session_exited`で終える。heartbeatが切れてpidが生きていれば`wrapper_heartbeat_expired`を記録し（[wrapperが黙ったsession](../design/supervisor-lifecycle/silent-wrapper.md)）、`Session` / `Revise` / `Resume`では`wrapper_silent`で終える（`Exiting`は待ちを続ける）。
     - runがqueue_holdに入ったか（`queue_hold`で終える）。
     - askの状態: 待ちのあいだに開いた表のaskを足し（`run_waiting_ask_added`）、持っている`worker_question`の回答かcloseを見る（`answered`）。
     - receipt: `Session`では`SessionWatch`がまだ見ていないreceipt、`Revise` / `Resume`では`worker_question`を持たない待ちに限り、段の依頼の後に書き直されたreceiptが現れたら`session_moved`で終える（決定2）。
     - `stalled`を持つ`Session`のrunでreceiptが無ければ、`StallWatch::poll_quiet`でそのaskを追う（`wait`の答えでaskを閉じて計時をやり直し、閾値を過ぎたら次の`stalled`のaskを開く。促しも既知のダイアログへのキーも送らない）。
     - `answer_prompt`か`stalled`を持つrunで、idle marker・`prompt-submit.json`・receiptのどれかが基準の時刻より新しければ`session_moved`で終える。
     - `answer_prompt`を持つ`Session` / `Revise` / `Resume`のrunの画面を、`watch_prompt`と同じ間隔で読む。ログインの切れなら下の「認証の切れ」、ダイアログが無ければ`prompt_cleared`を記録してaskを閉じ`dialog_cleared`で終える。`Resume`の`input_not_ready`のaskは、入力欄が準備できた画面で`dialog_cleared`で終える（askは戻ったslotの`ResumeWatch`が解消依頼を送る前に閉じる）。
   - **待ちの終わりで行う後始末**: phaseのpollが出来事を見たときに行うaskの後始末は、待ちの中でも同じtickで行う。sessionが終わったら`stuck_exit`と`answer_prompt`のaskを閉じ（`close_stuck_exit_asks`、`close_answer_prompt_asks`）、`Session`なら`stalled`のaskを`ended`で閉じる。ダイアログが消えたら`prompt_cleared`を記録して`answer_prompt`のaskを閉じる。sessionが動いたら`stalled`のaskを`moved on`で閉じる。こうして、終わったsessionへの`/exit`を人に頼むaskが、slotの空きを待つあいだ開いたままにならないようにする。
   - **止めるもの**: sessionへ送るものと、runを進める処理はすべて止める。答えの配送（`deliver_answers`）、促し（ADR-0047の決定30）、送信の確認とEnterの送り直し（決定31）、`/exit`とその再試行（決定25）、既知のダイアログへのキー（決定29）、解消依頼・差し戻し・古いreceiptの書き直しの依頼の送信、復旧job（`RecoveryWatch`。ADR-0047の決定39・40）の新しい起動と、その`send_instruction` / `stop_processes`の適用。待ちに入るときに復旧jobが走っていれば、待ちに入らない（runは人ではなく復旧jobを待っている。jobが`escalate`して開いたaskで待ちに入る）。待ちのあいだに送る必要が生じたら、それを待ちの終わり（`session_moved`など）として扱い、slotに戻ってから送る。runのstatusを変える書き込み（`supervision_finished`、workspaceのclose、`AfterExit`の処理、`ReviseOutcome` / `ResumeVerdict`の適用）もslotに戻ってから行う。
   - **認証の切れ**: 画面の読み取りでログインの切れ（ADR-0047の決定42の`auth_required`）を見つけたら、runを`queue_hold`のaskに足し、待ちを`queue_hold`で終えてslotに戻す（決定10の人が動かしたsessionと同じく空きを待たない）。`queue_hold`のあいだはclaimもjobも控えるので、slotに数えても他のrunを止めない。
   - **答えの配送**: `worker_question`の答えは、待ちのあいだに書かれたことを検知し（`answered`）、送信はslotに戻ったtickの`deliver_answers`で行う。答えを送るとworkerが作業を再開してCPUを使うので、slotを持つまで送らない。

### (d) 待ちの上限

7. **待ちの数に上限を付ける。`supervise --max-waiting N`（既定4、0で待ちを使わない）。**（変更: 戻り待ちも数える）`up`も同じ名前の引数を受け、既定と違うときだけsupervisorに渡す。supervisorは登録（と引き継ぎの取り戻し）の直後に`supervisors.max_waiting`（null可の互換の列。[ADR-0073](0073-kind-additions-are-compatible.md)の決定6）を書く。このADRでschemaを変えるのはこの列だけ（ADR-0062のときに足したもので、このADRでは足さない）。
   - **数えるもの**: slotの外のrun、つまり待ちと戻り待ちの両方を数える（task 445の実装を取り込む）。上限の根拠は生きているsessionのメモリで、戻り待ちのrunもslotの外でsessionを開いたまま持つからである。こうすると、開いているworkerのsessionの数は`--parallel`と`--max-waiting`の和を超えない（決定10で`--parallel`を超えて戻る分も、戻る前に上限の中で数えられている）。
   - **根拠**: 待ちのsessionはCPUをほとんど使わないが、メモリを持ち続ける。Claude Codeのsessionは1つあたり約0.3〜0.6GB（Contextの測定）で、cmuxのworkspaceを足しても増えるのはほぼsessionの分だけである。既定の4なら、走っているsession（`--parallel`の4）と合わせて開いているworkerのsessionは最大8、slotの外の分は最大で約2.3GBで、16GBのhostにinbox・planner・build（ADR-0049）と並べて収まる。夜間の実績（5日で5件、同時に最大2件）にも余裕がある。上限は`--parallel`と同じ数を既定にし、hostに合わせて人が変える。
   - **上限に達したとき**: 新たに待ちの条件を満たしたrunは、slotに居て数えられ、今のphaseのとおり進む（claimは止めない。slotが減るだけで、待ちの無いときより悪くはならない）。そのrunには`run_waiting_deferred`（`ask_id`、`ask_kind`、`waiting`、`limit`）をaskごとに1回だけ記録する。slotの外のrunが1つ減ったら、次のtickで、slotに居て待ちの条件を満たすrunのうちaskの古いものから待ちに移す。上限に達してslotで待つ`Revise` / `Resume`のrunにも、決定15・16の規則（段を時間切れで終えず、`worker_question`のidleで終えない）はそのまま効く。
   - 上限は待ちに入れる数だけを制限する。決定11で再起動や引き継ぎのときに組み立て直した待ちが上限を超えていても（上限を下げて起動し直したときなど）、slotに戻さずそのまま待ちにし、上限を下回るまで新しい待ちを入れない。

### (e) 再開

8. **待ちが終わったrunは、新しい仕事より先に、待ちが終わった順にslotへ戻す。** 戻す処理（`return_waiting_runs`）は`fill_slots`の外に置き、`drive`のループの毎回、引き継ぎの判定と`fill_slots`より前に、claimを止めているときも停止要求の後（drain中）も行う。drainは待ちのrunの終わりを待つので（決定11）、戻す処理がdrain中に止まると、答えが来たrunがslotに戻れずにループが終わらない。戻り待ちのrunを`run_waiting_ended`の古い順に、slotの数が`--parallel`未満のあいだslotへ戻し、`run_slot_regained`を記録する。そのあとで`fill_slots`が今の順で空いたslotを使う。戻り待ちのrunは生きているsessionを持ち（`session_exited`ではsessionが終わった直後で）、人がすでに答えているので、その先を他の仕事より先に進める。

9. **runtimeが再開させるrunは、slotが空くまで待つ（戻り待ち）。** `worker_question`の答え（`answered`）と、sessionが終わった待ち（`session_exited`）がこれにあたる。
   - `worker_question`: slotに戻ったtickで`deliver_answers`が答えを送る。以後は`Session`なら`SessionWatch`、`Revise` / `Resume`なら決定15・16のとおり。
   - `stuck_exit`: 人（inboxが`dagq-recover` skillの`reference/stuck-exit.md`に従う）が`/exit`してsessionが終わると、待ちが終わり、askは決定6のとおりその場で閉じる。slotに戻ったtickで`ExitWatch`の続きをそのまま進める: 画面を保存し、workspaceを閉じて`AfterExit`のとおり（`Land`なら`AwaitingSlot`から着地へ、`Ask`なら`approve_landing`のask、`ReviewFailed`、`Rest`）。
   - `Revise` / `Resume`の待ちのあいだにsessionが終わったら、slotに戻ったtickでその段が今のとおり扱う（`ReviseWatch`は書き直されなかった差し戻しとして`Ended`、`ResumeWatch`はreceiptからverdictを出す）。

10. **人が直接動かしたsessionは、slotの空きを待たずに戻す。** `dialog_cleared` / `session_moved`（`answer_prompt`、`stalled`、待ちのあいだのreceipt）、`queue_hold`、`wrapper_silent`、`phase_changed`は、人がworkspaceでダイアログに答えるか入力したことでsessionがすでに作業を再開しているか、claimが控えられているか、slotで送るもの（`/exit`）があるか、待ちを続けられないものである。runtimeはsessionを止められないので、戻り待ちにせず、見つけたtickでslotに戻す（`run_slot_regained`の`over_parallel`が`true`になりうる）。slotの数が`--parallel`以上のあいだは、空きの判定のとおり、adopt・landingの答え・resume・triage・claimは行わない。slotを使わない`recover_dead_runs`と掃除は今のまま行い、空きを見ない`apply_triage_answers`は、`retry`（taskを`ready`に戻すだけ）と`cancel`は今のまま適用し、`resume`の答えはrunを`needs_session`にするだけで、そのresumeは空きを見るresumeの段が始めるので、どれもslotを増やさない。超えるのは人が同時に答えたsessionの数だけで、開いているworkerのsessionの数は`--parallel`と待ちの上限の和を超えない（決定7）。

### (f) supervisorの入れ替えと再起動

11. **待ちの状態は、run_eventsとaskから組み立て直す。**（変更: `phase_changed`と、`Revise` / `Resume`の組み立て直しを足した）組み立て直し（`restore_waiting`）は、runのイベントから、終わった待ちのask、`run_waiting_deferred`のask、今の待ちか戻り待ち（決定5の`WaitState::of`）を戻す。待ちに入った時刻はイベントから取り、`run_waiting_started`を記録し直さない。組み立て直したphaseが待てないもの（例えばreviewからやり直すrun）なら、待ちを`phase_changed`で終えてslotに戻す。
   - **execの引き継ぎ**（ADR-0073の決定10）: execしたプロセスが同じtokenのleaseを持つrunのslotを組み立て直すとき（`Resume`は`handoff.json`のsnapshotから、`Revise`はイベントから）、待ちの印もイベントから組み立てる。上限を超えていても待ちのまま戻す。引き継ぎの要求を受けたsupervisorは、待ちのrunを区切りの判定に入れない（待ちのrunには進行中の短い処理が無い）。待ちのsessionはwrapperの下で動き続ける。
   - **adopt**（ADR-0039）: 待ちになりうるrunは`running`（`Session`）と、revise中の`awaiting_integration`（`Revise`。adoptは`review_anchor`から`ReviseWatch`を組み立てる）と、verdictの後の`awaiting_integration` / `needs_session` / `failed`（`Exiting`）と、resume中の`needs_session`（`Resume`）である。adoptは`running` / `validating` / `awaiting_integration`を引き継ぎ、`exit_requested`から`ExitWatch`の終了待ちを組み立てる（`needs_session`と`failed`の`Exiting`と`Resume`はadoptの対象外のまま。ADR-0047の決定24のadoptの拡張が入れば同じ規則で待ちも組み立てる）。adoptは空きslotの判定を先頭の打ち切りではなくrunごとに行い、イベントの上で待っているrunは、slotの外のrunの数が上限未満ならslotの空きを要さずに待ちとして引き継ぐ。上限に空きが無ければslotの空きを待って引き継ぎ、待ちを`phase_changed`で終えてslotのrunとして扱う（そのaskでまた待ちに入れる。決定2の例外）。イベントの上で戻り待ちのrunは戻り待ちとして引き継ぎ、決定8の順でslotに戻す。wrapperが黙ったrunを引き継がないこと（ADR-0039の決定1(c)）は変えない。
   - **drain**（SIGINT/SIGTERMの1回目、`down --wait`）: 待ちのrunもactive runとして終わりを待つ。決定8のとおり、drain中も待ちが終わったrunはslotに戻って進む。待ちのrunのleaseを残して終了し、次のsupervisorにadoptさせる案は採らない（Alternatives）。drainが人の答えまで長引くときは、人がaskに答えるか、ADR-0073の引き継ぎ（待たない入れ替え）を使う。

### (g) 見せ方

12. **`status`は待ちのrunと、slotと待ちの数を返す。**（変更: `waiting.count`に戻り待ちを含める）どれもDBから組み立て、supervisorのプロセスに問い合わせない。
    - supervisorごとに`slots: {used, parallel}`と`waiting: {count, limit}`を返す。`used`は、そのsupervisorのtokenのleaseを持つ`integrating`でないrunの数（`stats`の`idle_slots`が`active_runs`から数えるのと同じ集合）から、決定5のイベントで待ちか戻り待ちのrunを引いた数。`count`はslotの外のrun（待ちと戻り待ち）の数で、決定7の上限と同じ数え方にする（`count`が`limit`に達していれば新しい待ちは入らない、と読めるようにする）。`limit`は登録の`max_waiting`（列より古いbinaryの登録はnull）。決定10のあいだ`used`は`parallel`を超えうる。
    - `waiting`（配列）に待ちと戻り待ちのrunを1件ずつ返す: `run_id`、`task_id`、`asks`（`[{id, kind}]`）、`phase`（`session` / `exit` / `revise` / `resume`）、`status`（runの今のstatus）、`state`（`waiting` / `returning`）、`since`（待ちに入った時刻）、`waited_secs`（戻り待ちは終わりまで）、戻り待ちなら`ended_at`と`cause`。
    - 待ちはattentionを増やさない。人に届くのは各askの`ask_opened`である。`Resume`の`worker_question`の答えは、決定17が入ってからは`status`でも`delivering the answer of ask <id> (runtime)`になる。

13. **`stats`は待ちの回数と時間を返す。** ADR-0049の`stats`の項目に`waiting`を置く: `--since`以降の`run_waiting_started`の件数（始めたaskの`ask_kind`ごと）、`waited_secs`の合計・中央値・最大値（`ask_kind`ごと。夜間かどうかは分けない）、`slot_wait_secs`の中央値と最大値、`over_parallel`の件数、`run_waiting_deferred`の件数（上限に当たった回数。上限の見直しの材料で、observerが読む）。`waited_secs`の合計は、待ちがslotを塞いでいたら失われていたslotの時間である。`stats`の`idle_slots`のalert（observerが読み、taskの無い`blocked`のaskにする閾値。ADR-0047）は、決定12の`used`で空きを数える（待ちのrunを埋まったslotに数えない）。

### (h) goal 39との関係

14. **待ちのrunのbaseが古くなる問題はこのADRで扱わず、goal 39に分ける。** 待ちはrunの時間を延ばすので、待っているあいだにmainが進み、着地のときの衝突が増えうる。これは待ちの無い長いrunにも起きる問題で、今の仕組み（ADR-0027の決定4の`merge-tree`の事前判定、`integrate`の衝突から`needs_session`のresume、ADR-0047の決定24の衝突だけのresumeを数えない上限、[ADR-0068](0068-recheck-waiting-runs-after-each-landing.md)の着地のたびの先回りの確かめ）が扱う。このADRは待ちから戻るときにrebaseもbaseの読み直しもしない。goal 39が「戻るときにmainへ載せ直すか」を決めるときは、決定5の`run_slot_regained`がその時点になり、決定13の`waited_secs`が効果を測る材料になる。

### (i) 差し戻しと解消依頼の段（`Revise` / `Resume`）

15. **待ちのあいだは段の計時を止め、slotに戻ったら残り時間を持ち越さずに`resume_timeout`を数え直す。**（(b)）
    - 段の計時は、`ReviseWatch`の`sent`（差し戻し・解消依頼、または最後に送った答えからの`resume_timeout`）と、`ResumeWatch`の解消依頼の送信からの`resume_timeout`（`message_sent`）と、解消依頼を送る前の入力欄の準備の待ち（`agent_seen`からの`resume_timeout`）と、古いreceiptの書き直しの依頼の時間切れ（`StaleNudge`の`waited_out`）を指す。待ちのあいだphaseのpollは呼ばれないので段は時間切れにならず、待ちのあいだに段が`Ended`になることも`/exit`を送ることもない（決定4・6）。
    - 待ちが終わってrunがslotに戻ったtick（`run_slot_regained`を記録したとき）に、段の計時をその時点から数え直す（`sent`、`message_sent`の時計、送信前なら`agent_seen`と入力欄の準備の`ready_since`、`ResumeWatch`の古いreceiptの書き直しの依頼（task 357）が決着していなければ、その依頼の時間切れ（`waited_out`）と、idleを数え始める時刻（`answered_from`）も戻った時点から）。`worker_question`の答えを送ったときは、送った時点からさらに数え直す（task 238の実装のとおり）。**残り時間は持ち越さない**。`resume_timeout`は依頼が失われたかsessionが止まったことを見つけるための上限で、作業の持ち時間ではない。人の答えやダイアログへの操作は作業の向きを変えうるので、戻った時点からの1回分を与える。持ち越すと、待ちに入る前に時間を使っていた段が戻ってすぐ時間切れになり、人が答えた直後のsessionに`/exit`を送ることになる。
    - 待ちに入らずslotで待つとき（上限に達したとき、`--max-waiting 0`）も、closeされていない`worker_question`があるあいだは計時を止める（決定16）。`answer_prompt`だけが開いているslotの段は、ADR-0047の決定29〜31のとおり復旧jobと送信の確認が扱い、計時は止めない（slotに居るrunは段が進むので、待ちの外の規則のままにする）。
    - 引き継ぎとadoptで組み立て直した段は、組み立て直した時点から計時を始める（今の実装のとおり）。

16. **`worker_question`を打ったworkerがidleになっても段を終えない。答えを送った（かaskがcloseされた）後のidleだけで段を終える。**（(c)）
    - runにcloseされていない`worker_question`があるあいだ（未回答のもの、回答済みで配送前のものを含む）、`ReviseWatch` / `ResumeWatch`はidleでも段を終えず（`went idle without rewriting the receipt` / `went idle without a resolving receipt`にしない）、`resume_timeout`も数えない。古いreceiptの書き直しの依頼（task 357）もしない。`ReviseWatch`はtask 238でこのとおりになっており、`ResumeWatch`は決定17で揃える。
    - 答えを送ったときは送った時刻を、答えが手で配送されてaskがcloseされたとき（closeの秒が最後の入力の秒より後）はcloseの時刻を、最後の入力の時刻（`input_at`）にする。以後、idleで段を終えるのは、最後の入力より新しいidle markerがあり、段の依頼より新しい（書き直された）receiptが無いか、書き直されたreceiptの後にidleになったとき（今の`Rewritten` / `Mismatch` / `Ended`の判定）だけである。質問で止まったときのidle markerは最後の入力より古いので数えない。
    - 書き直されたreceiptの判定（`ReviseOutcome`、`ResumeVerdict`）は変えない。

17. **`Resume`の段でも答えを配送し、ダイアログを検知する（`Revise`と揃える）。** 決定1の`Resume`の行と決定16は、`ResumeWatch`が`ReviseWatch`と同じく次を行うことを前提にする。
    - 解消依頼を送った後の毎回のpollで`deliver_answers`を回し、`worker_question`の答えを送る（`ask_delivered`、`answer to ask <id>:`の書式、送信の確認はADR-0022の決定2とtask 285のとおり）。
    - agentが生きていれば`watch_prompt`を回し、ダイアログを`prompt_waiting`として記録し、復旧jobを経て`answer_prompt`のaskにする（ADR-0047の決定29・39）。段が終わるときは記録したダイアログを`prompt_cleared`にし、復旧jobを止める（task 238の`ReviseWatch`と同じ）。
    - `ResumeWatch`は`/exit`を送るとき（解消の判定、時間切れ、wrapperの沈黙、入力欄が準備できないままの時間切れのどれでも）、queueから見えるように`exit_requested`（payloadに`resume_attempt`）をrun_eventsに記録する（今はフィールドにしか持たない）。
    - `domain::session_takes_answers`に、resume中（最新の`resume_started`の後に`resume_finished`も`exit_requested`も無く、leaseがある）の`needs_session`のrunを足し、`status`の`delivering`と、`worker_question`の`runtime_delivers`をこれで決める。
    - `/exit`を送った後（`exit_requested`）のpollは今のとおりで、答えの配送も待ちもしない。

18. **`Revise` / `Resume`の待ちでも、決定2〜14はそのまま効く。** 待ちの上限、戻る順序（決定8）、戻り待ちにする`cause`（決定9）、その場で戻す`cause`（決定10）、adoptと引き継ぎ（決定11）、`status` / `stats`（決定12・13）は、phaseを問わず同じ規則で扱う。`stats`の`waiting`は`ask_kind`ごとに数え、phaseでは分けない（phaseは`run_waiting_started`の`phase`で後から数えられる）。

## ADR-0062からの引き継ぎ

ADR-0062の決定1〜14は、このADRでも同じ番号にある。「変更」の印のない決定は内容を変えずに引き継いだ（ADR-0025・ADR-0045の参照を、それを置き換えたADR-0054・ADR-0073に書き直したもの、ADR-0068の参照を足したものを含む）。既存のADRとdesign文書の「ADR-0062 決定N」は「ADR-0071 決定N」と読み替える。

| ADR-0062の決定 | このADR | 変更 |
| --- | --- | --- |
| 決定1（待ちにする条件） | 決定1 | 変更: `Revise` / `Resume`の`worker_question`と`answer_prompt`を待ちにする（(a)）。`Session`の`stuck_exit`はslotに数えたまま（(e)）。除外の理由を実装に合わせて書き足した |
| 決定2（待ちの終わり） | 決定2 | 変更: `Session`の待ちのあいだのreceiptで待ちを終える（(f3)。`Revise` / `Resume`の`worker_question`の待ちはreceiptで終えない）。runの終わりとleaseの喪失では`run_waiting_ended`を書かず、後のlease系のイベントで待ちを無いとみなす（(f1)）。`phase_changed`の例外を足した |
| 決定3（今のslotの外の仕組みとは分ける） | 決定3 | |
| 決定4（人の判断を代わりに下さない） | 決定4 | 変更（説明のみ）: `Revise` / `Resume`の段の時間切れも待ちのあいだは起きないことを足した |
| 決定5（表し方） | 決定5 | 変更: `phase`に`revise` / `resume`、`cause`に`wrapper_silent` / `phase_changed`を足し、`run_ended` / `lease_lost`を書かないことと、lease系のイベントでの再導出を決めた（(f1)） |
| 決定6（見張り方） | 決定6 | 変更: leaseを失ったら書かずに退く（(f1)）。wrapperの沈黙で`wrapper_silent`（(f1)）。`Revise` / `Resume`の見張りとreceiptの見張り（(f3)）を足した |
| 決定7（待ちの上限） | 決定7 | 変更: 戻り待ちも`--max-waiting`に数える（(f2)）。上限でslotに居る`Revise` / `Resume`にも決定15・16が効くことを足した。実装に合わせて、`up`は既定と違うときだけ`supervise`に渡し、登録の列は登録と引き継ぎの取り戻しの直後にsupervisor自身が書くことにした（ADR-0062は「`up`が受けてsupervisorに渡し、登録に記録する」とだけ書いていた） |
| 決定8（戻す順序） | 決定8 | |
| 決定9（runtimeが再開させるrunは空きを待つ） | 決定9 | 変更（説明のみ）: `Revise` / `Resume`の答えと、そのsessionの終わりの扱いを足した |
| 決定10（人が動かしたsessionは空きを待たない） | 決定10 | 変更（説明のみ）: その場で戻す`cause`に`wrapper_silent` / `phase_changed`とreceiptを足した |
| 決定11（組み立て直し） | 決定11 | 変更: `phase_changed`、adoptの上限の扱い、`Revise` / `Resume`の組み立て直しを足した |
| 決定12（`status`） | 決定12 | 変更: `waiting.count`に戻り待ちを含める（(f2)）。`phase`の値を足した |
| 決定13（`stats`） | 決定13 | |
| 決定14（goal 39との関係） | 決定14 | |
| — | 決定15〜18 | 新規: `Revise` / `Resume`の段の計時（(b)）、`worker_question`のidle（(c)）、`Resume`の配送とダイアログの検知、`Revise` / `Resume`にもADR-0062の決定を同じく適用すること（(d)） |

## Alternatives

- **runのstatusに`waiting`を足す。** 決定5の理由で採らない。statusは段を表し、待ちは段と直交する。
- **待ちのrunのleaseを返し、`approve_landing`と同じくDBに残す。** 生きているsessionの持ち主がいなくなり、`recover run`のattentionになり、adoptと`recover`の規則にleaseの無い生きたrunの例外を足す必要がある。答えの配送とsessionの終了の見張りも、leaseを持たないプロセスが行うことになる。採らない。
- **待ちのrunのsessionを閉じ（`/exit`）、答えが来たら`claude --resume`で開き直す。** メモリの上限は要らなくなるが、`stuck_exit`はそもそも`/exit`が効かないsessionで、`answer_prompt`はダイアログそのものが待っている相手なので閉じられない。`worker_question`は閉じられるが、開き直しは解消依頼と同じ起動・送信の確認を要し、文脈の読み直しに時間とトークンを使う。採らない。
- **askが答えられた時点で、kindを問わず待ちを終える。** `stuck_exit` / `stalled`の`wait`の答えや`ask close`でsessionが止まったままslotに戻り、同じ詰まりが起きる。決定2のとおりsessionの変化で終える。
- **leaseを失った待ちとrunの終わりに`run_waiting_ended`（`lease_lost` / `run_ended`）を書く（ADR-0062の文言）。** leaseを失ったプロセスがrunに書くことになり、adoptした側が続けている同じ待ちを消してしまう。後のlease系のイベントで待ちを無いとみなせば、書かなくても再導出が正しくなる（決定2・5）。採らない。
- **戻り待ちを`--max-waiting`に数えない（ADR-0062の文言）。** 上限の根拠は生きているsessionのメモリで、戻り待ちのrunもslotの外でsessionを開いたまま持つ。数えないと、答えが一度に届いたときにslotの外のsessionが上限を超えて積み上がる（決定7）。採らない。
- **`Session`の待ちのあいだのreceiptを見ず、askの出来事だけで終える（ADR-0062の文言）。** receiptを書いたsessionは`validating`へ進むべきで、待ちが`worker_question`の答えを待つあいだもslotの外に留まり、validatingと着地が遅れる。receiptはsessionが動いた印でもある（決定2）。採らない。
- **`Revise` / `Resume`の待ちのあいだの残り時間を持ち越す。** 待ちに入る前に時間を使っていた段が、人が答えた直後に時間切れになり、動き出したsessionに`/exit`を送ることになる。`resume_timeout`は作業の持ち時間ではなく、止まったsessionを見つける上限である（決定15）。採らない。
- **`Revise` / `Resume`の待ちでも段の計時を延ばすだけにする（止めずに、待った分を足す）。** 戻った時点での残りは持ち越しと同じになり、上と同じ理由で採らない。
- **`Revise` / `Resume`の待ちを入れず、段が時間切れになったら`approve_landing` / 次のresumeに回す（ADR-0062の除外のまま）。** 差し戻しや解消依頼の最中に人の答えを待つrunがslotを塞ぎ続けるか、答えを待たずに段を終えて人の答えを捨てる。`Resume`では`worker_question`を打ったworkerが`/exit`され、答えが届く先が無くなる。goal 41の問題がこの段では残る。採らない。
- **`Session`の`stuck_exit`も待ちにする。** wrapperが黙ったsessionはadoptされず、pidの死をerrorとして扱うので、引き継ぎと終わりを定められない。まれでもある（決定1）。採らない。
- **待ちに上限を付けない。** 人の決定（goal 41のconstraints）に反する。夜間に待ちが積み重なると、開いているsessionのメモリがhostを圧迫する。
- **上限に達したらclaimを止める。** 待ちの無いときより悪くなる（slotが空いていてもclaimしない）。決定7のとおり、上限を超えた分はslotに数える。
- **戻り待ちのrunをclaimと同じ順（優先度）で並べる。** 人がすでに答えたrunは、そのsessionを開いたまま待たせるほど、人の答えから結果までが延び、待ちの上限も空かない。決定8のとおり他の仕事より先にする。
- **drainで待ちのrunを待たずに終了し、leaseをstaleにして次のsupervisorにadoptさせる。** wrapperが生きていればADR-0039でadoptされるが、supervisorの居ないあいだは答えの配送もsessionの終了の見張りも止まり、`down --wait`の「走っているものが終わった」という意味（ADR-0073の決定8の非互換のmigrationの前提など）が崩れる。待たない入れ替えはADR-0073の引き継ぎが担う。採らない。
- **夜間に既定の答えを時間切れで適用する（案C）、夜間のclaim順を変える（案D）。** 人の決定で採らない。

## Consequences

- 人が答えられないあいだも、待ちのrunの分のslotで他のtaskが進む。差し戻しと解消依頼の最中に打たれた`worker_question`と、その段のダイアログも、決定15〜17が入ればslotの外で待つ。
- 開いているworkerのsessionは最大で`--parallel`と`--max-waiting`の和になり、その分のメモリを使う。戻り待ちも上限に数えるので、この和は答えが一度に届いても超えない。hostに合わせて`--max-waiting`を下げられる。
- `Revise` / `Resume`の段は、人の答えを待ったぶん壁時計では長くなる。待ちから戻ると`resume_timeout`の1回分が改めて与えられる。
- `Resume`の`worker_question`は、答えがworkerに届くようになる（今は届かず、workerがidleになると`/exit`される）。
- `Session`の`stuck_exit`と`Resume`の`stuck_exit`は、今までどおり待ちにしない（前者はslotに数え、後者はleaseを手放す）。
- ADR-0062に対してtask 445の実装が外れていた3点（lease系のイベントでの終わりと`wrapper_silent` / `phase_changed`、戻り待ちを上限に数えること、待ちのあいだのreceipt）は、このADRの決定になり、実装の変更は要らない。ただし`status`の`waiting.count`は今の実装では戻り待ちを含まないので、決定12に合わせる変更が要る。
- 実装は後続のruntimeのtaskで行う。分け方の目安:
  1. 決定12の`status`の`waiting.count`に戻り待ちを含める（design文書の[`status`](../design/supervisor-lifecycle/status.md)も直す）。
  2. 決定16・17の`ResumeWatch`の答えの配送・ダイアログの検知・`worker_question`のあいだidleで段を終えないこと、`exit_requested`の記録、`session_takes_answers`の拡張。待ちとは独立に入れられ、slotに居るまま`Resume`の`worker_question`が正しく扱われるようになる。
  3. 決定1の`Revise` / `Resume`の行と決定6の見張り（`WaitPhase`の`revise` / `resume`、書き直されたreceipt、`input_not_ready`の入力欄の準備、`wrapper_silent`）、決定15の戻るときの計時のやり直し、決定11の`Revise` / `Resume`の組み立て直し。2の後に行う。
  4. plugin の skill（inbox・`dagq-recover`）とdesign文書（supervisor-lifecycleの[人の答えを待つrun](../design/supervisor-lifecycle/waiting.md)・[Review](../design/supervisor-lifecycle/review.md)・`needs_session`・`status`・`stats`）の更新は、それぞれの実装のtaskで行う。
- 待ちの時間と上限に当たった回数が`stats`に出るので、`--max-waiting`の既定値と、goal 39（baseの古さ）の要否を実績で見直せる。
- ADR-0062の決定を番号で引く既存のADRとdesign文書は書き換えない（ADRは書き換えない規則）。番号は変わらないので、ADR-0071の同じ番号の決定として読む。
