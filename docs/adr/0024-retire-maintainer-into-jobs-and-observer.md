---
id: adr-0024
type: adr
title: maintainerを退役させ、review・triage・observerをheadlessのjobにし、observerの権限をnoteとdraft goalとaskに限り、goalにdraft状態を足す
status: superseded
created: 2026-09-23
updated: 2026-09-23
accepted_on: 2026-09-23
superseded_by: adr-0041
superseded_on: 2026-09-25
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - maintainer
  - plugin
  - operations
related:
  - adr-0003
  - adr-0007
  - adr-0009
  - adr-0010
  - adr-0011
  - adr-0012
  - adr-0016
  - adr-0019
  - adr-0021
  - adr-0022
  - adr-0023
  - design-overview
  - design-supervisor-lifecycle
  - design-plugin-integration
  - design-persistence
---

# ADR-0024: maintainerを退役させ、review・triage・observerをheadlessのjobにし、observerの権限をnoteとdraft goalとaskに限り、goalにdraft状態を足す

> **置き換え済み（2026-09-25）**: このADRの決定は現在有効ではない。現行の決定は[ADR-0041](0041-on-demand-planners-proposals-submitted-and-plan-review-job.md)を読む。

## Context

[ADR-0010](0010-maintainer-and-resident-supervisor.md)は常駐のClaude Code sessionとしてmaintainerを置き、監視・レビュー・着地・復旧を任せた。その後の決定でmaintainerの仕事は順にruntimeと他の役割へ移った。

- [ADR-0016](0016-maintainer-notification-and-compact-output.md)と[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)（goal 6、8）: 通知はpull型の`watch`になり、`needs_session`のresume、承認済みrunの再着地、push、follow_upのdraft登録、evidenceの検証、`prompt_waiting`の検知はsupervisorが行う。
- [ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)（goal 10）: 人への相談はaskとinboxに、taskとgoalの登録とgoalのcloseはplannerに移った。
- [ADR-0023](0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)（goal 11）: 通常のレビューと着地はsupervisorのheadlessのreviewに移った。

goal 10と11が着地すると、maintainerに残るのは次の4つだけになる。

- 失敗runのtriage（`recover`して`ready`に戻すか、cancelするか）
- resumeを3回試しても解消しない`needs_session`への指示
- failedのrunのworkspaceのclose
- in-cmux modeで止まったsupervisorの再起動

どれもrun 1件に対する処理か、判断が要って人に上げるものなので、常駐のsessionを置いて`watch`で待たせる理由が無い。常駐させると、1 runあたり数分の負荷のためにsessionのcontextとcompactionを抱え、maintainerが寝ている間は失敗runが放置される。

一方で、個々の詰まりの解消とは別に、システム全体としてうまくいっていないこと（同じ種類の失敗の繰り返し、着地待ちの伸び、閾値超えの常態化）を見て改善を提案する役割は誰も持っていない。ADR-0023の`stats`は数字を出すが、読んで次の一手にする主体が無い。ユーザーは2026-09-23に、maintainerをobserverに改め、その役割は継続的改善であって個々の詰まりの解消ではないと決めた。observerは定期起動のjobで、draftのgoalの登録を許す。

## Decision

**原則。** 常駐のsessionは、人と対話するもの（inbox、planner）とループそのもの（supervisor）だけにする。run 1件に対する判断はrunごとのheadlessのjobにし、その結果（verdict）で動くのはruntimeにする。継続的改善は定期起動のjobにし、状態を変える権限を持たせない。run_eventsのkindは追加だけで、既存のkind名とpayloadは変えない。schemaを変えるtaskは`user_version`を上げて`migrations/`に追加する。以下の6点を決める。

1. **役割はsupervisor / worker / planner / inbox / observerの5つにし、maintainerは退役する。**
   - **supervisor**: runtimeの`supervise`プロセス。claim、worker起動、validating、resume、jobの起動、着地、後始末を行う。
   - **worker**: runごとにworktreeで作業するClaude session。
   - **planner**: 人と対話してgoalとtaskを登録し、draftのgoalの採否を決め、goalをcloseする常駐session。
   - **inbox**: openなaskを人に見せてanswerを書き戻す常駐session。
   - **observer**: supervisorのtimerで定期起動するheadlessのjob（決定4）。
   - maintainerという役割、`[<repo>]dagq maintainer`のworkspace、`DAGQ_ROLE=maintainer`は無くなる。[ADR-0010](0010-maintainer-and-resident-supervisor.md)以降のADRに残るmaintainerの記述は書き換えず、[overview](../design/overview.md)の用語集で「review job / triage job / observer / inboxのいずれか」に読み替える（レビューと着地はreview job、失敗runの扱いはtriage job、継続的な監視はobserver、人への相談とanswerの実行はinbox。`up` / `down`などの操作は人がplannerかinboxのsessionから打つ）。
2. **jobは、cmux workspaceを持たないheadlessのClaude実行にする。**
   - 起動はADR-0023の決定2で`AgentProvider`に足したheadless実行のport（Claudeでは`claude -p`）を使う。promptに入力のpathと出力のJSON schemaを渡し、stdoutのJSONを読む。
   - jobは次の3種類。
     - **review job**: `awaiting_integration`のrunをレビューし、pass / concernを返す（ADR-0023の決定2、goal 11のtask 95）。
     - **triage job**: `failed` / `interrupted`のrunのreceipt・log・run_eventsを読み、`retry` / `resume` / `ask`のverdictを返す（決定3）。
     - **observer job**: supervisorのtimerで定期起動し、observationとdraft goalとaskを残す（決定4）。
   - jobの起動・終了・失敗はrun_eventsに記録する（kind名は実装taskが決める。ADR-0023の`review_started` / `review_finished` / `review_failed`に倣う）。headless実行が失敗したとき（起動できない、timeout、stdoutがschemaに合わない）は、対象のrunを動かさず、inbox宛てのaskにする。
3. **triage jobのverdictでruntimeが動き、安全に自動化できる後始末はruntimeが行う。**
   - supervisorは`failed`になったrunと、下の自動`recover`で`interrupted`になったrunごとにtriage jobを1回起動する。verdictは`{"verdict": "retry" | "resume" | "ask", "reasons": [...], "summary": "..."}`に固定し、payloadに記録する。
     - **retry**: runtimeがtaskを`in_progress`から`ready`に戻す（これまで人の手で行っていた遷移）。次のclaimで新しいrunが作られる。
     - **resume**: runtimeがrunを`failed` / `interrupted`から`needs_session`にし（新しい遷移）、goal 8の自動resume（[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)の決定1）で同じsessionに続けさせる。ADR-0019の試行3回の上限はrunごとの通算で数え、triageからのresumeも含める。上限を超えたrunは再びtriageに回らず、`decide`のaskになる。
     - **ask**: `kind: decide`のaskをinbox宛てに作り、`reasons`と`summary`をquestionに載せて待つ。answerに従う操作（`ready`、cancel、受け入れ条件の変更）は、inbox自身は判断しないので（ADR-0022の決定4）、人がinboxかplannerのsessionから打つ。
   - triage済みのrunのworkspaceはruntimeが閉じる。retryとaskのrunのworkspaceは用が済んでおり、resumeは同じsessionを開き直す経路（goal 8）なので、元のworkspaceは要らない。
   - wrapperが死んでleaseの無いrunは、supervisorが`recover`まで自動で行う。`recover`の結果は既存どおり`interrupted`で、taskを`ready`にはせず、triage jobに回す。wrapperが死んだ理由（機械の再起動、cmuxの終了、workerの異常）で扱いが変わり、runtimeには判別できないため。[ADR-0012](0012-adopt-stale-lease-of-live-wrapper.md)の「wrapperが死んだrunの`recover`は人の判断で手動」を、`recover`の実行だけ自動に改める。[ADR-0003](0003-supervisor-owns-lifecycle.md)・[ADR-0007](0007-run-level-leases-parallel-execution.md)の「孤児runは自動再実行しない」は、再実行をtriageのverdictに委ねることで維持する（runtimeが理由を見ずに`ready`へ戻すことはしない）。
4. **observerはsupervisorのtimerで1時間ごとに起動するjobにし、状態を変えない。**
   - 起動間隔は既定1時間で、`supervise --observe-interval`で変える。0なら起動しない。supervisorが居ないときは動かない。
   - 入力は`stats --since <前回の起動のcursor>`（ADR-0023の決定5）、直近のobservation、openなask。
   - 出力は次の3つだけ。
     - **observation**: run_eventsのkind `observation`として、task / run / goalのいずれかに紐づけて記録するnote。別の表は作らない。
     - **ask**: 閾値超えの詰まりを`kind: blocked`のaskとしてinbox宛てに上げる。ADR-0022のaskの`kind`に`blocked`を足す。`blocked`は知らせではなく、人に次の一手（放置、inboxかplannerからの操作、goalの登録）を選ばせるaskで、optionsにobserverの見立てを載せる。一意性はADR-0022の（`run_id`または`task_id`、`kind`）に従い、runにもtaskにも紐づかない閾値（空きslotがあるのにcandidatesがゼロ、goal単位の中央値超え）はrun_idもtask_idも持たない`blocked`をopenなもの1件にまとめ、questionに該当箇所を並べる。
     - **draftのgoal**: 改善の提案を決定5のdraft goalとして登録する。証拠として、根拠にしたobservationのidをdescriptionに書く。
   - observerはrun / task / goalの状態を変えない。runtimeは`DAGQ_ROLE=observer`のsessionからは許可したコマンドだけを受ける: 読み取り（`stats` / `status` / `events` / `show` / `asks`など）、`observation`の記録、`ask`、`goal add --draft`とそのdraft goalへの`add`（draftのtask）。それ以外（`ready` / `integrate` / `recover` / cancel / `goal ready` / `goal close` / `answer` / 通常の`add`など）は拒否する。許可の一覧で決めるので、observerが自分のdraftを`goal ready`でplannerを飛ばして採ることもできない。observerは個々の詰まりを解消しない。
5. **goalにdraft状態を足す。**
   - `goal add --draft`でdraftのgoalを作り、`goal ready ID`でdraftを外す。
   - draftのgoalに属するtaskは`candidates`に出ず、supervisorはclaimしない。
   - observerが登録したdraftのgoalは、plannerが人と採否を決め、採るなら`goal ready`にし、採らないなら`goal close`（`abandoned`）にする。
   - [ADR-0009](0009-goal-groups-tasks.md)の「goalは状態機械を持たない」は、draftか否かの1点に限って改める。進捗は従来どおりtaskの状態から導く。
6. **`up`はmaintainerを開かない。常駐はsupervisor / inbox / plannerの3つにする。**
   - `up`が作るworkspaceはsupervisor（in-cmux mode）、inbox、plannerだけになる。
   - `up` / `down` / 固定バイナリの更新は、人がplannerかinboxのsessionから打つ。
   - in-cmux modeのsupervisorが止まったときは、inboxの`watch`が`supervisor_stopped`で拾って人に知らせ、人が`up`を打つ。このため`watch --role inbox`が受けるattentionを、ADR-0022の決定1の`ask_opened`に加えて`supervisor_stopped`と`ask_answered`（answerに従う操作を人が打つため）に広げる。`--role maintainer`は無くなる。in-cmux modeに自動再起動が無いこと（[ADR-0011](0011-cmux-socket-password-and-in-cmux-fallback.md)）は変えない。

実装はgoal 12の後続taskが行う。本ADRの時点では未実装。

## Alternatives

- **maintainerを常駐で残す**: 実装は要らないが、goal 10と11の後にmaintainerに残る負荷は1 runあたり数分で、常駐のsessionを置く理由にならない。常駐させるとcontextとcompactionを抱え、寝ている間は失敗runが放置される。
- **maintainerを複数にする**: 並列のrunに追従できるが、同じattentionを2つのmaintainerが拾わないようにattentionのclaim（誰が処理中か）の仕組みが要る。runごとのjobならsupervisorのrun単位の起動が排他を兼ねるので、job化の方が単純。
- **observerを常駐sessionにする**: `watch`で即座に反応できるが、observerは詰まりを解消しないので即座に反応する必要が無く、待つ理由が無い。polling（timerの定期起動）のjobで足り、contextも起動ごとに入力から作り直せる。
- **observationを`docs/journal/`に書く**: 人が読みやすいが、`docs/journal/`は凍結済みで新しいジャーナルを作らない（AGENTS.md）。repositoryに書くとcommitと着地が要る。queue（run_events）に置けばplannerとobserver自身が`events`や`show`で読め、task / run / goalに紐づけられる。

## Consequences

- goal 8のtask 71の「resumeを3回試しても解消しないrunはattentionで人に返す」（[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)の決定1）は、triage job → `decide`のaskに乗せ替える。task 74の`prompt_waiting`（ADR-0019の決定6）はattentionの代わりに`answer_prompt`のask（[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)）に乗せ替える。どちらもmaintainerが拾う前提をやめ、inboxで人に届ける。
- plugin skillはgoal 10の7本から`dagq-maintain` / `dagq-land` / `dagq-session`の3本を消し、残る手順を`dagq-inbox` / `dagq-planner` / `dagq-recover`に移す。maintainerの初期prompt（`maintainer_prompt`、ADR-0016の決定8）とmaintainer用のSessionStart hookの分岐も退役する。
- [ADR-0010](0010-maintainer-and-resident-supervisor.md)、[ADR-0016](0016-maintainer-notification-and-compact-output.md)、[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)、[ADR-0021](0021-maintainer-and-supervisor-workspace-names-follow-the-run-style.md)、[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)、[ADR-0023](0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)のmaintainerの記述は本文を変えず、[overview](../design/overview.md)の用語集の読み替えで扱う。ADR-0023の`review_failed`（「maintainerが手でレビューする」）は、決定2のとおりinbox宛てのaskになる。ADR-0022の`watch --role maintainer`は決定6のとおり無くなり、その受け手はinboxに移る。
- AGENTS.mdの「役割」節とmaintainerのcold start手順は、supervisor / inbox / plannerの`up`と5役の説明に書き換わる。
- run_eventsのkindに`observation`とtriage / observer jobの起動・終了の記録が加わり、askの`kind`に`blocked`が加わる。goalにdraft状態（列か状態値かは実装taskが決める）が加わり、schemaが変わる。
- runtimeが`DAGQ_ROLE`で操作を拒否するのは、observerが初めての例になる。権限の判定は`DAGQ_ROLE`の申告に依存するので、悪意ある実行を防ぐものではなく、observerのpromptの誤りから状態を守る柵にとどまる。
- triageの判定をjobに任せるので、判定を誤るとretryが繰り返されうる。同じtaskの`failed`が2回を超えたことはADR-0023の`stats`の閾値に入っており、observerが`blocked`のaskで上げる。
- [overview](../design/overview.md)、[supervisor-lifecycle](../design/supervisor-lifecycle.md)、[plugin-integration](../design/plugin-integration.md)、[persistence](../design/persistence.md)は各実装taskで更新する。
