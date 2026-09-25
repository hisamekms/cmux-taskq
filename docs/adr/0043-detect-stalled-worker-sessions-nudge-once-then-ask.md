---
id: adr-0043
type: adr
title: supervisorが止まったworkerのsessionを決まった規則で検知し、一度促すかEnterを一度送り直してからinboxのaskにし、statsが走っているrunのalertと閾値ごとの結果を返す
status: superseded
created: 2026-09-25
updated: 2026-09-25
accepted_on: 2026-09-25
superseded_by: adr-0047
superseded_on: 2026-09-26
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - operations
related:
  - adr-0019
  - adr-0022
  - adr-0027
  - adr-0035
  - adr-0040
  - adr-0041
  - design-supervisor-lifecycle
  - design-domain-model
  - design-persistence
---

# ADR-0043: supervisorが止まったworkerのsessionを決まった規則で検知し、一度促すかEnterを一度送り直してからinboxのaskにし、statsが走っているrunのalertと閾値ごとの結果を返す

> **置き換え済み（2026-09-26）**: このADRの決定は現在有効ではない。現行の決定は[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)を読む。

## Context

運用記録（2026-09-22〜25）で、supervisorが検知できずに止まったworkerのsessionが3つの型で見つかった（goal 30）。

- **A: receiptの前にidleになったまま。** task 182のworkerはbackgroundの`cargo test`の通知を待ってturnを終え（Stop hookのidle markerは書かれた）、テストが戻らないまま10.5時間、eventが0件だった。supervisorの`SessionWatch`（`src/application/supervise/session.rs`）はreceiptを観測してからの時間切れ（`resume_timeout`）は持つが、receiptの無いidleには時間切れを持たない。[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)の決定6の`prompt_waiting`は「receiptもidle markerも無いまま」止まったrunだけを見るので、idle markerのあるAの型は対象外。
- **B: receiptの後にbackgroundの処理が残る。** task 195、約3時間。task 242で扱う。
- **C: supervisorが送った文が入力欄に残り、送信されていない。** task 205では差し戻し（[ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)のrevise）の文が貼られたままEnterされず、後の`/exit`が同じ行につながった。約1時間後の`/exit`の時間切れ（`exit_request_timed_out`）で初めて`stuck_exit`のaskになり、askの見立て（backgroundの処理の確認画面）も誤っていた。supervisorは文を送った後、それが処理されたかを確かめていない。

observer（1時間ごとのheadlessのLLM）もAを見逃した。`stats`（[ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)の決定5）は終わったrunだけを集計し、走っているrunのalertを持たないため。

2026-09-25のplannerとの対話で、人が次を決めた（goal 30のconstraints）。止まったsessionの検知はsupervisorの仕事にし、決まった規則でtickごとに見る。observerは傾向と、supervisorの検知の漏れの確認に絞る（goal 31）。askの前に一度だけworkerに促す。送った文は時間内に処理されなければEnterを一度だけ送り直す。閾値は設定でき、既定値は人が決めた値にする。検知ごとにその後どうなったかと検知までの時間を記録し、閾値の妥当性を継続的に確かめられるようにする。Bの型はtask 242に任せ、重ねない。

既存のADRとの関係。本ADRはADR-0019の6つの決定、[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の5つの決定、ADR-0027の決定、ADR-0040の5つの決定、[ADR-0041](0041-on-demand-planners-proposals-submitted-and-plan-review-job.md)の決定のどれも変えず、新しい検知・送信の確認・stallの表・statsの項目を足す。

- ADR-0019の原則の「runtimeがworkerに送るのは`/exit`と解消依頼の定型文だけ」と、ADR-0022の原則の「例外はworkerへのanswerの送信だけ」は、送り先をruntimeが起動・監視しているworkerのsessionに限る趣旨で、ADR-0022（answer）とADR-0027（reviseの定型文）が既に送る文を足している。本ADRの促しの文とEnterの送り直しも、同じ限定（runtimeが起動・監視しているworkerのsession、idle markerで入力可能と判定したとき）の中で足す。ADR-0022の決定1のaskのkindの列挙も、ADR-0041の`approve_plan`と同じく本ADRが`stalled`を足す（決定3）。この2本の原則の文言とkindの列挙を今の一覧に合わせて書き直すことは、ADR-0035の決定8の棚卸し（ADR-0019・0022・0027を統合ADRで置き換える）に任せる（2026-09-25、plannerへのask 57の答え）。
- ADR-0019の決定2の「`/exit`の再送はしない」と、決定6の「（ダイアログには）キーは送らない」は維持する。本ADRのEnterの送り直しは`/exit`を対象にせず、ダイアログの兆候のある画面には送らない（決定2）。
- ADR-0040の決定3は「`dagq.toml`は当面`[run.env]`だけを持ち、それ以外の設定を足すときは別のADRで決める」とする。本ADRはその別のADRとして`[stall]`の表を足す（決定4）。
- ADR-0040の決定5の`stats`の項目と閾値超えはそのまま残し、走っているrunのalertと検知の結果の集計を足す（決定5・6）。

## Decision

**原則。** 止まったworkerのsessionはsupervisorが決まった規則で検知し、まずworkerに一度だけ直させ、それでも動かなければinboxのaskで人に渡す。人に渡す前にruntimeが試すのは、定型の促しの文を1回と、Enterを1回だけで、文を重ねたりダイアログにキーを送ったりはしない。検知と送り直しとaskは、閾値とその後の結果とともにrun_eventsに残し、`stats`で閾値の妥当性を確かめられるようにする。run_eventsのkindは追加だけで、既存のkind名とpayloadは変えない。以下の7点を決める。

1. **receiptの無いidleを検知し、一度だけ促し、なお止まっていれば`stalled`のaskにする。**
   - **対象**: supervisorがsessionを監視しているworkerのrun。最初のsession（`SessionWatch`）、resumeしたsession（`ResumeWatch`）、reviseを送ったsession（`ReviseWatch`）の3つの段（以下「段」）で見る。verdictの後の`ExitWatch`とreceiptの後のbackgroundの処理（Bの型）は対象外で、task 242が扱う（決定7）。
   - **判定**（tickごと）: 次をすべて満たす状態が`[stall].idle_without_receipt_secs`（既定1200秒=20分）続いたら「receiptの無いidle」とする。
     - その段のidle markerがあり、その段で最後にsessionへ届いた入力（段の開始、supervisorが送った文、人の入力。決定2の送信の印で知る）より新しい。つまりsessionは入力を処理し終えてturnを閉じている。
     - その段で書き直されたreceiptが無い（最初のsessionはreceiptが無い。resume / reviseは段の開始より新しいreceiptが無い）。
     - そのrunに閉じていない`worker_question`のask（ADR-0022の決定2）が無い。answerの送信待ちのworkerは人を待っているので止まっていない。
     - そのrunに閉じていない`answer_prompt`のaskも、その段の`prompt_waiting`（ADR-0019の決定6）も無い。ダイアログはそちらの経路が扱う。
     - 経過は最新のidle markerのmtimeから数える。markerの`background_tasks`に`running`の処理があってもidleに数える（task 182の型。backgroundの処理の有無はaskに載せる）。
   - **促し**: 段ごとに1回だけ、定型の促しの文をworkerのterminalへ送る（送信は決定2の確認の対象）。文は「receiptの無いまま止まっている。作業が終わったならcommitしてreceiptを書く。判断が要るなら`dagq ask --run <run-id> --kind worker_question`で聞く。backgroundの処理を待っているなら、何を待っているか、いつ終わる見込みか、戻らなければどうするかをterminalに書く」の趣旨で、run id・経過時間・markerの`background_tasks`の要約を含める。文面は`src/application/prompt.rs`の他の依頼文と同じ場所に置く。送る前に`stall_nudged`（payload: `phase`（`session` / `resume` / `revise`）、`idle_secs`、`threshold_secs`、`background_running`、`background_tasks`の要約）を記録する。
   - **ask**: 促しの後にsessionが応答し、再びidleになって、上の判定がもう一度`idle_without_receipt_secs`続いたら（促しの送信が処理されなかった場合は決定2のaskが先に開く）、inbox宛ての`kind: stalled`のaskを作る（`asked_by`は`supervisor`、`reason: idle_without_receipt`）。questionは人に宛てて、run id・task id・段・receiptの無いidleの経過時間・促しを送った時刻・markerの`background_tasks`（`running`の処理の`description`と`command`、無ければ無いこと）・`WorkspaceBackend::capture`で読んだ画面の末尾15行（`stuck_exit`と同じ`screen_tail`。読めなければその旨）と、選択肢の意味を持つ。optionsは次の2つ。
     - `wait`: sessionに触らず待つ。supervisorは判定の計時をanswerの時刻からやり直し、なお`idle_without_receipt_secs`止まっていれば新しい`stalled`のaskを作る（促しは送り直さない）。
     - `intervene`: 人がsessionに手を入れる。inboxはanswerを受け、人の指示で`dagq-recover` skillの手順（画面を読み、backgroundの処理を止めるか、指示を打ち込むか、runを止めて`recover`する）に従う。supervisorは計時をやめ、sessionが次に入力を受けるかreceiptを書くまで同じ段で新しいaskを作らない。
   - 同じ（run、kind）のopenなaskがあれば作らない（askの重複抑止。ADR-0022）。askが開いている間にsessionが自分で動いた（新しい入力の印かreceipt）ら、supervisorはそのaskを`stuck_exit`と同じく閉じる（未回答ならruntimeがanswerを書いて`ask_answered`（`runtime_closed: true`）を記録する。これはattentionではない）。runが終わったとき（sessionの終了、`recover`、abandon）も同じく閉じる。`stalled`のaskの`ask_opened`は、他のkindと同じくinboxのattention（`answer ask <id>`）になる。
   - 引き継いだ（adoptした）runは、run_eventsの`stall_nudged`とaskの有無からどこまで進んだかを読み、促しもaskも2回出さない。
2. **supervisorが送った文は、閾値の時間内に処理されたかを確かめ、処理されていなければEnterを一度だけ送り直し、なお動かなければ`stalled`のaskにする。**
   - **対象の送信**: resumeの解消依頼（ADR-0019の決定1）、reviseの差し戻し（ADR-0027）、衝突の解消依頼（ADR-0027の決定4）、workerの質問へのanswerの配送（ADR-0022の決定2）、決定1の促しの文。`/exit`は対象外で、ADR-0019の決定2どおり送り直さず、`exit_request_timed_out`と`stuck_exit`のaskで扱う。
   - **処理された印**: sessionが送った文を入力として受け取ったこと。Claude adapterはrunごとの`claude-settings.json`に`Stop` hookと並べて`UserPromptSubmit` hookを書き、hookは入力を受けるたびに`<run-dir>/prompt-submit.json`を一時ファイル + renameで書く。markerの読み取りはidle markerと同じく`AgentSignals`（`infrastructure::claude`）の後ろに置き、applicationは送信の時刻とmarkerのmtimeを比べる。送信の時刻より新しい送信のmarker、idle marker、receiptのどれかがあれば「処理された」とする。送信のmarkerを持たないprovider（hookの無い古いClaude Codeを含む）では、idle markerとreceiptだけで判定する。
   - **確認**: 送信から`[stall].send_confirm_secs`（既定60秒）以内に処理された印が無ければ、画面を`WorkspaceBackend::capture`で読む。ADR-0019の決定6と同じダイアログの兆候（`detect_prompt`）があれば、Enterは送らずにすぐ下の`stalled`のask（`reason: send_unconfirmed`、questionにダイアログの種類）を作る。ADR-0019の決定6の`prompt_waiting`と`answer_prompt`のaskの条件（receiptもidle markerも無いrun）は広げない。兆候が無ければEnterを1回だけ送り（`WorkspaceBackend`にEnterだけを送るportのmethod（`send_enter`）を足す。今のportは`send_text` / `capture` / `send_exit`だけ）、`send_retried`（payload: `send`（`resume` / `revise` / `rebase` / `answer` / `nudge`）、`threshold_secs`、`screen_tail`）を記録する。送り直してから`send_confirm_secs`以内に処理された印が無ければ、`send_unconfirmed`（payload: `send`、`threshold_secs`、`waited_secs`）を記録し、inbox宛ての`kind: stalled`のask（`reason: send_unconfirmed`）を作る。questionは送った文の種類と先頭、送信とEnterの時刻、画面の末尾15行を持ち、optionsは決定1と同じ`wait` / `intervene`。
   - **送信の順序**: 処理が確かめられていない送信がある間、supervisorは同じsessionに次の文も`/exit`も送らない。task 205のように、処理されていない文の後ろに`/exit`がつながることを防ぐ。例外は[wrapperが黙ったsession](../design/supervisor-lifecycle.md#wrapperが黙ったsession)の`/exit`で、その扱いは今のまま変えない。wrapperが黙っている間は、決定1の促しも決定2の確認とEnterの送り直しも行わない。
   - 文そのものは送り直さない。貼られた文が入力欄に残っているなら、Enterだけで送れる。文を重ねると、処理されていた場合に同じ依頼が2回届く。
3. **`stalled`のaskの答えと、検知のその後を記録する。**
   - askのkindに`stalled`を足す（`asks`のCHECKに加えるので、schemaの`user_version`を上げて`migrations/`に追加する）。`reason`（`idle_without_receipt` / `send_unconfirmed`）はaskの`ask_opened`のpayloadとquestionに持つ。
   - askが答えられ、inboxと人が`dagq-recover` skillに従ってsessionを扱う経路は`stuck_exit`・`answer_prompt`と同じ。`cmux notify`はaskの登録のときに1回だけ送る（ADR-0022の決定5）。
   - 検知（`stall_nudged`、`send_retried`、`stalled`のaskの`ask_opened`）ごとに、その結末を`stall_resolved`（payload: `detection`（`nudge` / `enter_retry` / `ask`）、`threshold`（決定4の設定名）、`threshold_secs`、`detected_after_secs`（receiptの無いidleの検知は、その判定を満たし始めた時刻（最新のidle markerのmtime）から検知までの秒。送信の検知は送信から検知までの秒）、`outcome`、`resolved_after_secs`（検知から結末までの秒））として1回記録する。`outcome`は次のどれか。
     - `resolved_by_nudge`: 促しの後、次の判定までにreceiptかworker_questionのaskが書かれた。
     - `resolved_by_enter`: Enterの送り直しの後、送信が処理された。
     - `resolved_by_itself`: askが開いている間に、supervisorも人も入力を送らずにsessionが動いた（`runtime_closed`でaskを閉じた場合）。
     - `answered_wait`: askの答えが`wait`だった（閾値が早すぎた疑い）。
     - `answered_intervene`: askの答えが`intervene`だった（人が手を入れた）。
     - `escalated`: 促しやEnterの送り直しでは解消せず、次の検知（`stalled`のask）に進んだ。その先の結末はaskの`stall_resolved`が持つ。
     - `run_ended`: 結末の前にrunが終わった（`recover`、abandon、cancel、sessionの終了）。
   - **見逃しの疑い**も記録する。supervisorが送っていない入力（送信のmarkerが、supervisorの送信の記録の無い時刻に更新された）を、receiptの無いidleが続いている段で観測し、そのときの経過がまだその閾値を超えていなければ、閾値を超える前に人が気づいて手を入れたとみなし、`stall_preempted`（payload: `threshold`、`threshold_secs`、`idle_secs`（人の入力までの経過））を記録する。receiptの無いidleのまま人が`recover`したrunも、`recover`のeventから同じく数える。Claude Code自身が差し込む入力（backgroundの処理の完了通知など）が`UserPromptSubmit`を発火させる場合は、hookの入力で人の入力と区別できるものだけを数え、区別できないものは数えない（hookのfieldの確認は実装taskが行う）。
4. **閾値はrepositoryの`dagq.toml`の`[stall]`で設定し、既定値は人の決めた値にする。**

   | 設定名 | 既定値 | 意味 |
   | --- | --- | --- |
   | `idle_without_receipt_secs` | 1200（20分） | receiptの無いidleが促しまで続く時間と、促しの後にaskまで続く時間（同じ値を2回使う） |
   | `send_confirm_secs` | 60 | 送った文が処理されるのを待つ時間と、Enterの送り直しの後に待つ時間 |
   | `background_alert_secs` | 1800（30分） | `stats`がbackgroundの処理を「長く動く」とするまでの時間（決定5） |

   - 値は正の整数（秒）。ADR-0040の決定3の`[run.env]`と同じく、runtimeが読むのはmain checkoutの作業ファイルの`dagq.toml`で、書式の誤り（未知のkey、整数でない値、0以下）はエラーにする。`[stall]`が無いか、keyが無ければ既定値を使う。この repositoryは`dagq.toml`を置かないので既定値で動く。
   - supervisorは起動時に読み、読んだ値をtaskの無いrun_eventsに`stall_config_loaded`（payload: 3つの値）として記録する。値を変えたら`down --wait` → `up`で起動し直す。`stats`は走っているsupervisorが最後に記録した`stall_config_loaded`の値でalertを判定し（ファイルだけを変えて起動し直していないときに、supervisorが使っていない値で検知の漏れを数えないため）、記録が無いときだけファイル（無ければ既定値）を読む。
   - 検知のeventはそのときの`threshold_secs`を持つので、閾値を変えた前後の結果を分けて集計できる。
   - 既定値と書式は[supervisor-lifecycle](../design/supervisor-lifecycle.md)に書く。
5. **`stats`は走っているrunのalertを返す。** ADR-0040の決定5の項目に、`running_alerts`（runごとの配列）を足す。終わったrunの集計と違い、今の状態から導く: run_events・asksに加えて、run dirのidle markerと送信のmarkerとreceiptのmtime、cmuxのworkspaceの一覧を読む。新しい表は持たない。alertは次の4種類。
   - `idle_without_receipt`: 決定1の判定を満たし、経過が`idle_without_receipt_secs`を超えたrun。促し・askの有無を添える（supervisorが検知していれば促しかaskがあるはずなので、無ければ検知の漏れ）。
   - `long_background`: markerの`background_tasks`に`running`の処理があり、その処理が最初に`running`で現れたmarkerから`background_alert_secs`を超えたrun。receiptの前後を問わない（receiptの後のものはtask 242の対処の対象で、alertは観測だけ）。
   - `running_outlier`: `running`の経過（claimから今まで）がそのgoalの作業時間の中央値の2倍を超えたrun（ADR-0040の決定5の閾値超えと同じ基準を走っているrunに当てる）。
   - `workspace_mismatch`: `running`などsessionを持つはずのrunのworkspaceが`cmux workspace list`に居ない、またはqueueの`[dagq]`のworkspace groupに、どの走っているrunにも`session_workspaces`にも対応しないworkerのworkspaceが居る。
   - cmuxに接続できないときは、`workspace_mismatch`だけを`unavailable`（理由付き）にし、他のalertは返す。`stats`自体は失敗させない。
6. **`stats`は閾値ごとの検知の結果を返す。** `stall_thresholds`（設定名ごと）に、`--since`以降の検知の件数（`detection`ごと）、`outcome`の内訳（決定3）、`detected_after_secs`と`resolved_after_secs`の中央値と最大値、`stall_preempted`の件数、使われた`threshold_secs`の値ごとの内訳を返す。集計はrun_eventsから再導出する（ADR-0040の決定5と同じ）。observer（goal 31）はこれを入力に読み、`answered_wait`が多い（早すぎる）、`stall_preempted`が多い（遅すぎる）、`running_alerts`の`idle_without_receipt`に促しもaskも無い（検知の漏れ）などを見て、閾値の見直しが要ると判断したらnoteにする。observerは止まったsessionを自分では検知しない。
7. **Bの型（receiptの後のbackgroundの処理）はtask 242が扱う。** 本ADRはreceiptの後の`ExitWatch`の待ち（`resume_timeout`）と`stuck_exit`のaskを変えない。本ADRがBの型に関わるのは`stats`の`long_background`のalert（観測だけ）と、決定2の送信の順序（処理が確かめられていない送信の後ろに`/exit`を送らない）だけ。

実装はgoal 30の後続taskが行い、[supervisor-lifecycle](../design/supervisor-lifecycle.md)（検知・送信の確認・`[stall]`の既定値）、[domain-model](../design/domain-model.md)（eventとaskのkind）、[persistence](../design/persistence.md)（`asks`のCHECK）と、pluginの`dagq-inbox` / `dagq-recover` skill（`stalled`のaskの見せ方とanswerの実行）、`dagq` skillの`reference/observer.md`（`running_alerts`と`stall_thresholds`の読み方）を更新する。runtime testは、task 182の型（backgroundの処理を残したidle markerのままreceiptが来ない）と、task 205の型（送った文の後に入力の印が来ない）を再現する。本ADRの時点では未実装。

## Alternatives

- **observerに止まったsessionを検知させる**: observerは1時間ごと（と1日1回）に起動するheadlessのLLMで、検知までに最大1時間の遅れがあり、同じ入力に同じ判定を返す保証も無い。止まったかどうかは、idle marker・receipt・ask・送信の印のmtimeとrun_eventsから決まった規則で判定できるので、tickごとに状態を見ているsupervisorが持つ。実際、observerはtask 182を見逃した（`stats`が走っているrunを見ていなかったため）。observerには、決定6の結果から閾値の傾向と検知の漏れを確かめる役を残す。
- **促さずにすぐaskにする**: 実装は単純だが、task 182の型はworkerがbackgroundの処理を待っていることを自分で書けば解ける場合が多く、送った文が入力欄に残ったCの型はEnter 1回で解ける。止まるたびに人を呼ぶと、inboxのaskが増えて本当に判断が要るaskが埋もれ、人の待ちが着地を遅らせる。runtimeが1回だけ試し、それで動かなければ人に渡す。1回に限るのは、繰り返すと同じ文が何度も届き、止まった原因（ダイアログ、壊れた会話）がある場合に状況を悪くするため。
- **送った文そのものを送り直す**: 文が処理されていた（印だけが遅れた）場合に同じ依頼が2回届き、workerが2回作業する。入力欄に残った文はEnterだけで送れるので、Enterだけにする。
- **画面を読まずにEnterを送り直す**: ダイアログが開いていると、Enterが選択肢を押してしまう（ADR-0019の`/exit`の再送を退けた理由と同じ）。送り直す前に画面を読み、ダイアログの兆候があればEnterを送らずに`stalled`のaskにする。
- **送信の確認にidle markerの更新だけを使う**: idle markerはturnの終わりにしか書かれないので、依頼を受けて長く作業している間（数分〜数十分）は「処理されていない」と誤る。`UserPromptSubmit` hookは入力を受けた時点で書かれるので、60秒の閾値で判定できる。
- **閾値をコードに固定する**: 運用で閾値の妥当性を確かめても、変えるにはreleaseが要る。`dagq.toml`で変えられるようにし、eventに使った値を残して前後を比べられるようにする。
- **閾値をsupervisorのCLIの引数にする**（`--observe-interval`と同じ形）: `stats`も同じ閾値でalertを判定するので、supervisorとCLIの両方が同じ値を読める場所に置く。

## Consequences

- Aの型は、receiptの無いidleから最大で`idle_without_receipt_secs`の2倍（既定40分）と送信の確認の時間で、促しかaskになる。task 182の10.5時間はこの範囲に収まる。Cの型は、送信から最大で`send_confirm_secs`の2倍（既定2分）でEnterの送り直しかaskになり、処理されていない文の後ろに`/exit`がつながらない。
- workerへの送信に、促しの文とEnterの送り直しが加わる。どちらもruntimeが起動・監視しているworkerのsessionに限り、inbox・plannerのterminalには打ち込まない。ADR-0019・ADR-0022の原則の送信の一覧は、ADR-0035の決定8の棚卸しで統合ADRを書くときに、本ADRの送信も含めて書き直す。
- 長く正当にbackgroundの処理を待つ作業（長いtestなど）は、20分で促され、さらに20分で`stalled`のaskになる。workerが促しに状況を書けば、askの画面の抜粋から人が`wait`を選べる。`wait`が多ければ`stats`の`stall_thresholds`に表れ、observerが閾値の見直しをnoteにする。
- Claude adapterが書くhookに`UserPromptSubmit`が加わり、run dirに`prompt-submit.json`が増える。markerの形式はClaude Code固有なので`AgentSignals`の後ろに閉じる。
- `asks`の`kind`に`stalled`が加わり、schemaの`user_version`が上がる。run_eventsに`stall_nudged` / `send_retried` / `send_unconfirmed` / `stall_resolved` / `stall_preempted` / `stall_config_loaded`が加わる。`WorkspaceBackend`に`send_enter`が加わる。
- `stats`がcmuxを呼ぶようになる。observerの`claude -p`はsupervisorの子プロセスとしてcmuxに接続できる。接続できない環境でも`workspace_mismatch`以外のalertは返る。
- `dagq.toml`が`[run.env]`に加えて`[stall]`を持つ。表を足すたびにADRで決める規則（ADR-0040の決定3）は変わらない。
