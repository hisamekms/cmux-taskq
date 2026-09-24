---
id: adr-0035
type: adr
title: follow_upのdraft taskの採否をsupervisorが起動するheadlessのfollow-up triage jobが決め、判断がつかないものだけinboxで人に聞く
status: accepted
created: 2026-09-24
updated: 2026-09-24
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - planner
  - operations
related:
  - adr-0007
  - adr-0009
  - adr-0019
  - adr-0022
  - adr-0023
  - adr-0024
  - adr-0027
  - adr-0029
  - design-supervisor-lifecycle
  - design-domain-model
  - design-persistence
  - design-plugin-integration
---

# ADR-0035: follow_upのdraft taskの採否をsupervisorが起動するheadlessのfollow-up triage jobが決め、判断がつかないものだけinboxで人に聞く

## Context

[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)の決定4で、`integrate`は着地したrunのreceiptの`follow_ups`を、元のtaskと同じgoal（閉じていればgoalなし）の`draft` taskとして登録し、runに`follow_up_registered`（`task_id`、`title`、`index`、goalが閉じていれば`goal_closed: true`）を記録する。登録されるdraftはtitleとdescriptionだけで、acceptance・`verification_commands`・`paths`・evidence・依存は空。`ready`にするかcancelするかは人が決める。

この運用では次のことが起きている。

- draftが人の判断待ちで溜まる。plannerが人と1件ずつ、acceptanceと検証とpathsを補って`ready`にするか、cancelするかを決めるまで動かない。
- goalに所属するdraftが残っているあいだ`goal close --verdict achieved`は拒否されるので、goalのcloseも止まる。
- 判断の大半は定型的である。「worker自身の範囲外として書かれた、同じgoalの続きの小さな修正」を採り、「既に他のtaskで済んだ」「goalと関係ない思いつき」を落とす判断は、goalとtaskとrepositoryの規則（AGENTS.md）を読めばつく。

一方で、run 1件に対する判断をheadlessのjobにし、そのverdictでruntimeが動く型は[ADR-0024](0024-retire-maintainer-into-jobs-and-observer.md)の決定3（triage job）と[ADR-0023](0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)の決定2（review job）で既にある（[supervisor-lifecycle](../design/supervisor-lifecycle.md)のTriage (supervisor)とReview (supervisor)）。人への相談はinbox宛てのask（[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)）にする。

2026-09-24にplannerのsessionでユーザーと次を決めた: askのoptionsは`adopt` / `cancel` / `keep_draft`、自動adoptの上限は「閉じたgoal」「深さ2以上」「acceptanceが空」の3つ、導入前から残っているdraftも対象にする。

## Decision

**原則。** follow_upのdraftの採否はtriage jobと同じ型で扱う。jobはqueueを読むだけでverdictを返し、状態を変えるのはverdictとanswerを適用するruntimeだけで、適用は1トランザクションにする。runtimeは自動adoptを安全側に倒す上書き規則を持つ。この repository固有のverificationの規則はruntimeに書かず、jobのpromptがrepositoryのAGENTS.md / CLAUDE.mdを読んで反映する。run_eventsのkindは追加だけで、既存のkind名とpayloadは変えない。schemaを変えるtaskは`user_version`を上げて`migrations/`に追加する。以下の9点を決める。

1. **担当はsupervisorが起動するheadlessのfollow-up triage jobにする。**
   - supervisorが対象のdraft（決定2）ごとにjobを1回起動し、stdoutのverdict（決定4）をruntimeが適用する（決定5）。jobの起動には`AgentProvider::headless_command`（Claudeでは`claude -p --allowedTools Read Grep Glob -- <prompt>`）を使い、envは`DAGQ_ROLE=reviewer`と`DAGQ_QUEUE`にする（run triageと同じく、CLIは読むコマンドだけを許す）。cwdはrepositoryのmain checkoutで、jobはAGENTS.md / CLAUDE.md、goalのdoc、関係するsourceを読んで、verification・paths・evidenceを決める。
   - 次の担当は選ばない。
     - **planner**: 人と対話する常駐sessionで、人からgoal / taskへの一方向の窓口である。runtimeから仕事やaskを受けない（ADR-0024の決定1、[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)）。plannerに任せると今と同じく人の対話の順番待ちになり、draftが溜まる問題が解けない。
     - **observer**: 継続的改善のための定期起動のjobで、個々の詰まりを解消しない。状態を変えるコマンドはCLIが拒否し（ADR-0024の決定4）、書けるのはnote、`blocked`のask、draftのgoalだけである。起動も1時間ごとで、draft 1件ごとの判断には向かない。
     - **worker**: follow_upを書いた本人だが、自分のtaskの範囲しか見ておらず、goalの他のtaskの進み具合や着地後のmainを知らない。receiptは着地の前に書かれるので、着地後に既に済んだかを判断できない。receiptの`follow_ups`を完全なtaskの形に広げても、採るか否かの判断は残る。
     - **inbox**: 自分では判断しない（ADR-0022の決定4）。
     - **`integrate`の中の同期の判断**: `integrate`は単一の着地slotで直列に走る。そこでheadlessのClaudeを最大`review_timeout`待つと後ろの着地がすべて止まり、jobの失敗が着地の失敗と混ざる。人が手で打つ`integrate`もClaudeを待つことになる。jobを着地と切り離し、`follow_up_registered`を見て後から起動する。
2. **対象は、statusが`draft`で、そのtaskを`task_id`に持つ`follow_up_registered`があり、まだそのtaskに`follow_up_triage_finished`も`follow_up_triage_failed`も無いtaskにする。**
   - `follow_up_registered`で`task_id`がnull（`skipped`の項目）のものは対象にならない。人が`add`で作ったdraftや、observerのdraft goalのtaskも、`follow_up_registered`が無いので対象にならない。
   - 導入前から残っているdraftも同じ条件で対象になる。migrationは既存のdraftを除外しない。
   - 人がその間に`ready`やcancelにしたtaskは`draft`でなくなるので対象から外れる。
   - 対象の順はtaskのIDの昇順（古いdraftから）にする。
3. **排他・slot・timeout・イベント名はrun triageにそろえる。runを持たないtaskにはtask単位のleaseを取る。**
   - **lease**: 今のleaseはrun単位の`run_leases`（[ADR-0007](0007-run-level-leases-parallel-execution.md)）で、draftのtaskにはrunが無い。そこで新しい表`task_leases`（`task_id`主キー、`supervisor_token`、`reason`、`heartbeat_at`）を足す。鮮度はrun leaseと同じ規則（heartbeatが30秒より古ければstale）で、supervisorがheartbeatで`run_leases`と同時に`heartbeat_at`を更新する。leaseの取得と解放は、draftのtaskに紐づく（`run_id`がnullの）`lease_acquired` / `lease_released`（`reason: follow_up_triage`）として記録する。run_eventsで`run_id`を持たない`lease_*`はこれが初めてなので、`lease_*`を読む側（`status`、`doctor`、`stats`）は`run_id`がnullの行をtaskのleaseとして読む。
   - **開始**: `begin_follow_up_triage`が`BEGIN IMMEDIATE`の中で決定2の条件と下の同時実行の上限を再検査し（staleなleaseは置き換える）、`task_leases`に行を取って、draftのtaskに紐づくイベント（`task_id`とgoalを持ち`run_id`は持たない）`lease_acquired`と`follow_up_triage_started`（`attempt`: そのtaskの何回目の起動か）を記録する。同じdraftを2つのsupervisorが取ろうとしても1つしか通らない。
   - **slot**: run triageと同じく、active runが`--parallel`未満のときだけ始め、jobの間はslotを1つ使う。今のslotはsupervisorが持つrun leaseの数なので、新鮮な`task_leases`の行もslotとして数える: claimの空きslotの判定、`backend_call`の`slots`、`stats`の`idle_slots`はrun leaseと`task_leases`の和で数える。導入時に残っているdraftが多くてもworkerのclaimを塞がないように、follow-up triageの同時実行はqueue全体で1つまでにする（`task_leases`の`reason: follow_up_triage`の新鮮な行の数で数える）。
   - **timeout**: `AgentProvider::review_timeout`（既定600秒）。過ぎればkillしてjobの失敗（決定8）にする。ループはプロセスを`try_wait`で見るだけで止まらない。
   - **イベント**: `follow_up_triage_started`（`attempt`）、`follow_up_triage_finished`（`attempt`、`verdict`、`reason`、`overridden`（上書きの理由、無ければnull）、`task`（提案）、`action`、`new_task_id`、`ask_id`、`depth`、`duration_secs`、適用後のdraftの`status`）、`follow_up_triage_failed`（`attempt`、`error`、`duration_secs`、draftの`status`）。どれもdraftのtaskに紐づける。
   - **ファイル**: runのdirectoryが無いので、`<queue dir>/follow-ups/<task-id>/`に`follow-up-triage-prompt-N.txt`、`follow-up-triage-N.out`、`follow-up-triage-N.err`を書く。
   - **引き継ぎ**: jobの途中でsupervisorが死ぬとleaseがstaleになる。`follow_up_triage_started`の後に`finished` / `failed`が無いdraftは決定2の対象のままなので、他のsupervisorがstaleなleaseを置き換えて次の`attempt`で起動し直す。起動し直しは`follow_up_triage_started`が3件（`attempt` 3）までで、4回目は起動せずに`follow_up_triage_failed`（`error`: 起動の上限）を記録して決定8に回す。verdictを適用する前にleaseをまだ持っているかを確かめ、失っていれば何も書かずに手放す（run triageと同じ）。
   - **prompt**: draftのtitle・description・context、元のtask（`follow_up_registered`を持つrunのtask）のtitle・description・acceptance・`verification_commands`・`paths`・evidence、元のrunのreceiptの`summary`と`follow_ups`、goalのtitle・description・acceptance・constraints・doc、同じgoalの他のtask（ID、title、status）、決定6の上書き規則、verdictのschema、repositoryのAGENTS.md / CLAUDE.mdを読んでverificationの規則を反映せよという指示。
4. **verdictのschemaは`{"verdict": "adopt" | "drop" | "ask", "reason": string, "task": {...}, "question": string}`にする。**
   - `task`は`{"title": string, "description": string, "acceptance": string, "verification_commands": [string], "paths": [string], "evidence": [string], "depends_on": [integer], "context": string}`。`adopt`では必須で、`ask`でも提案できるなら書く（人が`adopt`を選べるようにするため）。`drop`では無視する。
   - `question`は`ask`のとき人への質問で、`adopt` / `drop`では空でよい。
   - stdoutの全体か最外の`{...}`を読み、未知のフィールドは拒否する（review / triageと同じ）。
   - 提案の検査: `title`が空、`paths`のglobが`validate_path_globs`（[ADR-0029](0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)）を通らない、`evidence`に`tests` / `e2e` / `subagent_review`以外がある、`depends_on`に存在しないtaskかcanceledのtaskがある、のどれかなら提案は不正とする。verdictが`adopt`で提案が不正ならaskに置き換え（決定6と同じく`overridden`に理由を書く）、askで提案が不正なら提案を持たないaskとして扱う。
5. **verdictの適用は1トランザクションにする。**
   - **adopt**: `finish_follow_up_triage`が1トランザクションで次をすべて行う。draftのgoalに提案の`task`を登録し（goalはdraftと同じ。決定6によりgoalなしのdraftはここに来ない）、`depends_on`の依存を張り、他のtaskがdraftに依存していればその依存を新しいtaskへ付け替え（付け替えで依存が循環するなら提案は不正として扱い、adoptせずaskにする）、新しいtaskを`ready`にし、draftを`canceled`にし、出自を記録する。出自は、新しいtaskの`context`の冒頭に「follow-up draft task <draft-id>（task <元task-id> の run <run-id> の receipt が提案）をfollow-up triageがadopt」と書くことと、draftと新しいtaskの両方に`follow_up_adopted`（`draft_task_id`、`new_task_id`、`source_task_id`、`source_run_id`、`by: "job" | "person"`、`ask_id`、`depth`）を記録することで残す。あわせて`task_created`、`task_status_changed`、`follow_up_triage_finished`を記録し、`task_leases`の行を消す。
   - **drop**: draftを`canceled`にし、`follow_up_triage_finished`（`action: dropped`）を記録してleaseを消す。これも1トランザクション。
   - **ask**: 新しいaskの`kind` `follow_up`を足し（`asks`のkindのCHECKを広げるmigration）、draftのtaskに紐づけて（`run_id`はnull）inbox宛てに作る。`asked_by: supervisor`、optionsは`adopt` / `cancel` / `keep_draft`（有効な提案が無ければ`cancel` / `keep_draft`だけ）、questionは`question`（無ければ上書きの理由）、`reason`、提案の要約、promptのpath。askの作成、`follow_up_triage_finished`（`action: asked`、`ask_id`）、leaseの削除を1トランザクションで行う。askの一意性はADR-0022の（`task_id`、`kind`）に従う。askを通すのでinboxに`cmux notify`が届く。draftはそのまま。
6. **runtimeは次のどれかに当たるadoptをaskに置き換える。**
   - **閉じたgoalのfollow-up**: draftのgoalがnull（`follow_up_registered`に`goal_closed: true`があり、goalなしで登録された）か、適用時点でdraftのgoalが閉じている。
   - **深さ2以上のfollow-up**: draftの`follow_up_depth`（下記）が2以上。
   - **acceptanceが空の提案**: 提案の`acceptance`が空か空白だけ。
   - 置き換えた理由は`follow_up_triage_finished`の`overridden`とaskのquestionに書く。jobのpromptにも同じ規則を渡し、jobが最初から`ask`を返せるようにする（run triageの規則と同じ二重の持ち方）。
   - **深さの定義**: 深さは「人の判断を経ずに続いたfollow-upの段数」である。`tasks`に列`follow_up_depth`（`INTEGER NOT NULL DEFAULT 0`）を足して記録する。
     - `add`で人やplannerが登録したtaskは0。
     - `integrate`がtask Pのrunの`follow_ups`からdraftを登録するとき、draftの深さはPの`follow_up_depth + 1`。
     - jobのverdictで自動adoptしたtaskは、draftの深さをそのまま持つ。
     - 人がaskに`adopt`と答えて登録したtaskと、人が`ready`でdraftから`ready`にしたtaskは0に戻す（`ready`コマンドの`draft → ready`の遷移が`follow_up_depth`を0に書く。人の判断を経たので、その子のfollow-upは深さ1から数え直す）。
     - したがって、人が登録したtaskのfollow-upは深さ1で自動adoptの対象になり、自動adoptされたtaskのfollow-upは深さ2でaskになる。
     - migrationは既存のtaskを0にし、`follow_up_registered`の`task_id`が指すtaskのうちstatusがまだ`draft`のものを1にする（人が既に`ready`にしたfollow-upは人の判断を経たので0のまま）。導入前には自動adoptされたtaskが無いので、これで上の定義と一致する。
7. **askのanswerはruntimeが適用する。**
   - supervisorはfill passごとに、回答済み・未closeの`follow_up`のaskのうち、draftがまだ`draft`でleaseが無いものを`decide_follow_up`で1トランザクションで適用し、askを閉じて`follow_up_decided`（`ask_id`、`answer`、`new_task_id`、`status`）を記録する。askが回答済みかつ未closeであること、draftが`draft`であることは中で再検査するので、2つのsupervisorが二重に適用しない。
     - `adopt`: 直近の`follow_up_triage_finished`の`task`（jobの提案）を適用時点で決定4の検査と決定5の循環の検査にかけ直し、通れば決定5のadoptと同じ登録・依存・ready・draftのcancel・出自の記録を行う。`follow_up_adopted`は`by: "person"`と`ask_id`を持ち、新しいtaskの深さは0。draftのgoalが閉じているかnullなら、新しいtaskもgoalなしで登録する（人が選んだので登録は妨げない）。有効な提案が無い（askの時点で無かったか、その後に`depends_on`のtaskがcancelされたなどで検査を通らなくなった）ときは適用せず、自由記述のanswerと同じ扱い（下記）にする。
     - `cancel`: draftを`canceled`にする。
     - `keep_draft`: draftは`draft`のまま残す。`follow_up_triage_finished`が既にあるので決定2の対象から外れ、以後jobの対象にならない。plannerが人と扱う（決定9）。
   - 3つのどれでもない自由記述のanswerは適用しない。`ask_answered`はattention（`read the answer of ask N and close it`）のままで、inboxは「plannerのsessionでこのdraftを扱ってほしい」と人に伝え、人の指示でaskを閉じる。inboxは自分では判断しない。runtimeが適用できる`adopt`（`answer`の時点で有効な提案がある）・`cancel`・`keep_draft`のanswerの`ask_answered`には`runtime_delivers: true`を書いてattentionにしない（`status`では`applying the answer of ask N (runtime)`）。
   - draftが既に`draft`でない（人が`ready`やcancelを打った）なら適用するものが無いので、askを閉じるだけにする。
8. **jobの失敗はattentionにする。**
   - 起動できない、非0終了、timeout、stdoutにverdictが無いかschemaに合わない、verdictの適用中のerrorは、`follow_up_triage_failed`を記録して`task_leases`の行を消し、draftはそのままにする。
   - `follow_up_triage_failed`はinbox宛てのattention（`next: triage follow-up by hand`）にする。supervisorは同じdraftをもう一度jobにかけない。人の指示で、inboxが`dagq-recover` skillの手順に従ってdraftと元のrunのreceiptを見せ、人が決めたとおりplannerかinboxのsessionからcancelするか、acceptance・検証・pathsを補って`ready`にする。
9. **ADR-0019の決定4の「`ready`にするか、cancelするかは人の判断」を改める。**
   - follow_upのdraftを`ready`にするか（完全なtaskに置き換えるか）cancelするかは、follow-up triage jobのverdictとruntimeの上書き規則が決める。人が決めるのは、askになったもの、`keep_draft`にしたもの、jobが失敗したものだけになる。`integrate`がdraftを登録すること、その形（titleとdescriptionだけ、`follow_up_registered`の記録）は変えない。
   - **plannerの手順**: plannerはfollow_upのdraftを1件ずつ人と決める作業をやめる。goalの進み具合を見るときは、draftのうち`follow_up_triage_*`がまだ無いものはjobの待ち、`follow_up_decided`が`keep_draft`のものと、`follow_up_triage_finished`の後に`follow_up_decided`が無いまま`follow_up`のaskが閉じられたもの（自由記述のanswerで人がplannerで扱うと決めたもの）を人と扱う対象として読む。plannerはruntimeからaskも報告も受けないことは変わらない。
   - **goal close**: `goal close --verdict achieved`が所属taskに`completed` / `canceled`以外があれば拒否することは変えない。goalのtaskがすべて着地した後も、jobの待ちのdraft、openな`follow_up`のask、`keep_draft`のdraft、自由記述のanswerで閉じたaskのdraft、jobが失敗したdraftが残っていればcloseできない。plannerはcloseの前にこれらを確かめ、jobの待ちなら待ち、askならinboxでの回答を待ち、`keep_draft`・自由記述で閉じたもの・失敗したものは人と決めて片付けてからcloseする。自動adoptされたtaskは同じgoalに入るので、それが着地するまでgoalは閉じない。

実装はgoal 22の後続taskが行う。本ADRの時点では未実装。

## Alternatives

- **plannerに任せたまま、draftの一覧を見やすくする**: 実装は小さいが、判断が人の対話の順番待ちのままで、draftが溜まることとgoal closeが止まることは解けない。判断の大半が定型的なので、人に残す理由が無い。
- **observerに採否を決めさせる**: observerは状態を変えない柵（ADR-0024の決定4）の上に成り立っていて、`ready`やcancelを許すとその柵が崩れる。起動も1時間ごとで、個々の詰まりの解消という役割の外になる。
- **workerがreceiptに完全なtaskを書き、`integrate`がそのまま`ready`で登録する**: 判断のjobが要らないが、workerは自分のtaskの範囲しか見ておらず、採るべきかの判断（既に済んだか、goalに要るか）をしないまま`ready`になる。receiptの形を変えると既存のreceiptの検査とpromptも変わる。
- **`integrate`の中で同期にjobを走らせる**: 別の起動の仕組みが要らないが、単一の着地slotを最大`review_timeout`塞ぎ、jobの失敗が着地の失敗と混ざる。人の手の`integrate`もClaudeを待つことになる。
- **run triageの`decide`のaskを流用する**: kindを足さずに済むが、`decide`はrunに紐づきoptionsが`retry` / `resume` / `cancel`で、supervisorがanswerを適用する条件（runが`failed` / `interrupted`）も違う。runを持たないdraftに流用すると適用の判定が混ざるので、`follow_up`を足す。
- **runを持たないtaskの排他を`run_leases`に仮のrunを作って取る**: 表を足さずに済むが、`task_runs`の行はworktreeやsessionを持つrunの前提で、`candidates`・`status`・`stats`がrunとして数えてしまう。task単位のlease表を足す。
- **深さをイベントから毎回たどって求める**: 列を足さずに済むが、`follow_up_registered`と`follow_up_adopted`を祖先へたどる問い合わせが要り、人の判断で0に戻す規則も表しにくい。列1つで持つ。
- **自動adoptに上限を設けない**: 人の手はさらに減るが、自動adoptされたtaskが次のfollow_upを生み、人の判断を経ずにgoalが膨らみ続けうる。閉じたgoalの続きは新しいgoalにすべきか人が決めることで（ADR-0009）、acceptanceの無いtaskはreviewの基準が無い。

## Consequences

- follow_upのdraftは、上書き規則に当たらなければ人の手を介さずに`ready`の完全なtaskか`canceled`になる。goal closeが止まるのは、askと`keep_draft`とjobの失敗が残っているときだけになる。
- schemaが変わる: `task_leases`表、`tasks.follow_up_depth`列、`asks`のkindの`follow_up`（kindのCHECKを作り直すmigration）。run_eventsのkindに`follow_up_triage_started` / `follow_up_triage_finished` / `follow_up_triage_failed` / `follow_up_adopted` / `follow_up_decided`が加わる。`follow_up_triage_failed`はinbox宛てのattention。
- `ready`コマンドがdraftから`ready`にするときに`follow_up_depth`を0に書くようになる。
- jobがslotを使うので、導入直後に残っているdraftが多いと、同時実行1つの上限の中で1件ずつ（1件あたり最大`review_timeout`）片付く。その間もworkerのclaimは残りのslotで進む。
- 自動adoptの判断を誤ると、要らないtaskが`ready`で走る。深さの上限で連鎖は1段に抑え、誤りはreviewの`concern`、observerの`stats`、人のcancelで拾う。
- `DAGQ_ROLE=reviewer`の読み取りだけの制限は申告に依存する柵で（ADR-0024のConsequences）、jobのpromptの誤りから状態を守るにとどまる。
- [ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)の本文は書き換えず、決定4の「人の判断」を本ADRで読み替える。[supervisor-lifecycle](../design/supervisor-lifecycle.md)（`integrate`の10の「draftなのでsupervisorは拾わず、`ready`にするかは人が決める」、Triage (supervisor)に並ぶ節、`status` / `watch`のattention、`ask` / `answer`）、[domain-model](../design/domain-model.md)（`Receipt`の`follow_ups`の記述、`Task`の`follow_up_depth`）、[persistence](../design/persistence.md)、[plugin-integration](../design/plugin-integration.md)と、plugin skillの`dagq-planner`・`dagq-inbox`・`dagq-recover`・goal closeの手順は各実装taskで更新する。
