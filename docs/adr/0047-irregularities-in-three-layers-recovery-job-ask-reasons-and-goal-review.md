---
id: adr-0047
type: adr
title: イレギュラーをruntimeの自動修正・復旧job・inboxの3層で扱い、askに人が要る理由の分類を必須にし、自動修正を数え、goalの達成をgoal review jobが判断する（ADR-0019・ADR-0043・ADR-0044を統合）
status: accepted
created: 2026-09-26
updated: 2026-09-26
accepted_on: 2026-09-26
supersedes:
  - adr-0019
  - adr-0043
  - adr-0044
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - planner
  - observer
  - plugin
  - operations
related:
  - adr-0003
  - adr-0007
  - adr-0008
  - adr-0009
  - adr-0010
  - adr-0011
  - adr-0012
  - adr-0013
  - adr-0016
  - adr-0019
  - adr-0022
  - adr-0024
  - adr-0026
  - adr-0027
  - adr-0028
  - adr-0029
  - adr-0034
  - adr-0036
  - adr-0037
  - adr-0038
  - adr-0039
  - adr-0040
  - adr-0041
  - adr-0042
  - adr-0043
  - adr-0044
  - adr-0045
  - adr-0046
  - design-overview
  - design-supervisor-lifecycle
  - design-domain-model
  - design-plugin-integration
  - design-persistence
  - adr-inventory
---

# ADR-0047: イレギュラーをruntimeの自動修正・復旧job・inboxの3層で扱い、askに人が要る理由の分類を必須にし、自動修正を数え、goalの達成をgoal review jobが判断する（ADR-0019・ADR-0043・ADR-0044を統合）

## Context

2026-09-24〜25に、人の判断が要らないイレギュラーがinboxに上がり、人の手を待った（goal 34）。

- 依頼や`/exit`が入力欄に残って送信されない（task 205、221、182、259）。task 285で送信の確認とEnterの送り直しを入れた。
- cmuxの時間切れ（`backend_timeout`）で`/exit`が送れず、`runtime_error`でrunを手放した（task 259。reviewはpassで、inboxが手で着地させた）。
- 既知のダイアログで止まる（Claude Codeの「Background work is running」の確認画面、Settings / Usageのパネルが開いたまま）。[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)の決定6は「ダイアログにキーを送らない」としていた。
- supervisorの入れ替えの後に、`needs_session`のresumeが拾われない（task 205、221）。旧supervisorがresumeのleaseを持ったまま止まった。新supervisorのadoptの対象は`running` / `validating` / `awaiting_integration`だけで、`resume_parked_runs`はsessionが生きている間は手を出さない。
- rebaseの後にreceiptが書き直されない（task 205）。HEADはcleanでmainの上にあるのに、receiptは古いcommitを指したままworkerがturnを終えた。
- よく触られるファイルの衝突だけで、review pass済みのrunがresumeを3回使い切る（task 221、276）。2026-09-25にinboxが手で、前のbranchのcommitを今のmainに載せ直すretryを行った。
- backgroundの処理が終わらない。孤児のtest fixtureがパイプを握り、task 182は約10.5時間、task 285は1時間止まった（根本修正はtask 317で着地済み）。
- ログインが切れて止まった（task 266）。認証は人にしかできないが、複数のsessionで同時に起きるとaskも複数になる。

ユーザーは2026-09-25に、inboxをユーザーの判断が要るものだけに限ると決め、inboxが出した案を承認した（goal 34のconstraints）。

- 3つの層で扱う: (1) runtimeが決まった規則で直す。安全だと確かめられる条件がそろうときだけ自動で直し、eventに残す。(2) headlessの復旧job（今のtriage jobを広げたもの）が状況を読み、許された操作の範囲で直す。直せないか自信がないときだけinboxに上げる。(3) inboxには人の判断が要るものだけを出す。
- inboxに残すもの: 受け入れ条件・ADR・goalの決定と食い違う着地、成果を捨てるかどうか（cancel、引き継がないretry）、認証（ログイン）、コスト（利用上限への対処）、復旧jobが直せなかった・自信がないと返したもの。
- askの作成に「人が要る理由」の分類を必須にし、当てはまらないaskは作る時点で拒むかnoteに回す。認証のaskは複数のrunで同時に起きても1件にする。
- 自動で直した件数をeventとして数え、`stats`でtask 325（askの集計）と並べて見られるようにする。
- 衝突だけのresumeは上限に数えない。draftのtask 158（衝突の解消依頼も上限に数える）は採らない。
- ディスクの容量（人の決定 2026-09-25）: 終わったrunのビルド成果物（worktreeの`target/`、`llvm-cov-target`など）はrunが終わったらすぐ消し、ソースとcommitは残す。worktree自体はtaskが`completed` / `canceled`になった時点で消す（retry待ちのtaskのworktreeは残す）。claimの前とintegrateの検証の前に空き容量を確かめる。閾値の既定は、claimの前が「直近のrunの`target`の最大値 × 2」、integrateの前が「その × 1.5」で、設定で変えられる。足りなければclaim / 検証を控えて掃除を自動で走らせ、それでも足りないときだけinboxに知らせる（理由の分類はコスト・資源）。
- goalの達成の判断（人の決定 2026-09-25、plannerとの対話）: plannerではなく、専用のheadlessのgoal review jobが行う。supervisorは、taskがすべて`completed` / `canceled`でdraftも残っていないopenなgoalを見つけたら起動する（同じgoalのjobは同時に1つ）。verdictは`achieved`（runtimeがgoalを`achieved`で閉じ、acceptanceの項目ごとの根拠を記録する）/ `gaps`（足りないものを、そのgoalのdraftのtaskとして出どころ`goal_gap`で登録する。以後はgoal 29のtask 282の経路でplannerが補う。goalは開けたまま）/ `ask`（acceptanceの変更や`abandoned`など人の判断が要るときだけinbox）。goal 13のtask 115の案はこれに置き換える。
- 案の表のうち、observerが解消済みの件をaskにする行はgoal 31のtask 329・294、固定バイナリの入れ替えの行はgoal 32、孤児のtest fixtureの根本修正はtask 317（着地済み）が扱う。本ADRはそれらを決めない。

これらは次の既存のADRの決定を変える。

- [ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md): 決定1（resumeの試行の上限と、衝突だけのresume、adopt）、決定2（`/exit`の再送はしない、timeoutは知らせるだけ）、決定6（ダイアログにはキーを送らない）。
- [ADR-0043](0043-detect-stalled-worker-sessions-nudge-once-then-ask.md): 決定1・2の「inboxのaskにする」（復旧jobを先に通す）と、決定2の「`/exit`は送信の確認の対象外」（task 285の実装は`/exit`も確認している）。ADR-0043のContextは「ADR-0019の決定2・6は維持する」と書いていた。
- [ADR-0044](0044-findings-proposals-from-findings-and-quiet-observer.md)（[ADR-0041](0041-on-demand-planners-proposals-submitted-and-plan-review-job.md)を置き換え）: 決定1（役割の説明）・決定2（jobの種類）・決定3（triage job。`decide`のanswerはsupervisorが適用する今の実装に合わせた）、決定8（readyの例外に引き継ぐretryを足す）、決定16（goal closeの前にplannerが確かめるもの）、決定17（inboxに届くもの）、決定21（observerの入力に自動修正の件数を足す）。

[ADR-0042](0042-adr-is-superseded-whole-and-deprecation-date-is-deprecated-on.md)の決定2により、この3本のまだ生きている決定を書き直して引き継ぎ、3本を丸ごと置き換える統合ADRにする。既存の文書とsourceの「ADR-00XXの決定N」を読み替えられるよう、決定の番号を次のように固定する。

| 置き換えたADR | 本ADRの決定 |
| --- | --- |
| ADR-0044の決定N（1〜23） | 決定N（1〜23。同じ番号） |
| ADR-0019の決定N（1〜6） | 決定23+N（24〜29） |
| ADR-0043の決定N（1〜7） | 決定29+N（30〜36） |
| 新しい決定 | 決定37〜45 |

ADR-0044はADR-0041を、ADR-0041はADR-0024とADR-0037を置き換えていたので、本ADRはそれらから続く決定もまとめて持つ。書き直しでは、着地済みの実装に合わせて形を具体にした（task 100の`decide`のask、task 104の`stuck_exit`のask、task 285の送信の確認、task 288の`StallWatch`）。goal 28で決めたtaskの優先度（[ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)の決定4）と、[ADR-0046](0046-full-text-search-related-and-duplicate-of.md)の検索・関連・重複の記録は変えない。

[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)は置き換えない。askに列（人が要る理由）と一意性の鍵を足すことは、ADR-0041・ADR-0043がkindを足したのと同じく、ADR-0022の決定1のaskの表と一意性に足すものとして扱い、kindと列の一覧を今の値で書き直すことは、[ADRの棚卸し](../plans/adr-inventory.md)でADR-0022を置き換える統合ADRに任せる（ADR-0043の扱いと同じ）。[ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)の決定5の`stats`の項目も変えず、本ADRは項目を足す（決定45）。`dagq.toml`に表を足すことはADR-0040の決定3の「別のADRで決める」に当たり、本ADRが`[resume]`・`[exit]`・`[disk]`を足す（決定24、25、44）。

## Decision

**原則。** 常駐のsessionは、人に届くものの窓口（inbox）とループそのもの（supervisor）だけにする。人と対話する計画は、計画ごとにオンデマンドのworkspace（planner）を開いて行う。run 1件、計画1件、goal 1件に対する判断はheadlessのjobにし、jobはqueueを読むだけでverdictを返し、状態を変えるのはverdictとanswerを適用するruntimeだけにする。適用は1トランザクションにする。イレギュラーは3つの層で扱う: runtimeが決まった規則で、安全の条件がそろうときだけ自動で直してeventに残し（決定37、38）、それ以外は復旧jobが許された操作の範囲で直し（決定39、40）、inboxには人の判断が要るものだけを、人が要る理由の分類とともに出す（決定41、42）。継続的改善は定期起動のjob（observer）にし、run / task / goalの状態を変える権限を持たせず、見つけたものを構造のある記録（finding）に積み上げる。findingを計画に変えるのはruntimeが立てるplannerで、人に聞くかどうかはplannerとplan review（AI）が判断する。人とAIは同じCLIで記録を読む。repository固有の規則（AGENTS.mdのverificationの規則、ADR番号など）はruntimeに埋め込まず、jobとplannerのpromptがrepositoryの文書から読む。run_eventsのkindは追加だけで、既存のkind名とpayloadは変えない。schemaを変えるtaskは`user_version`を上げて`migrations/`に追加する。以下の45点を決める。

### I. 役割、job、計画、finding（ADR-0044の決定1〜23）

決定1〜23はADR-0044の決定1〜23を同じ番号で書き直したもの。見出しの後の括弧の注記のうち「ADR-0041の決定Nを…」はADR-0044がADR-0041から引き継いだときの注記で、本ADRで改めた決定には「ADR-0044の決定Nを…」と書いた。「本ADRより前」はADR-0044より前と読む。

1. **役割はsupervisor / worker / planner / inbox / observerの5つにする。maintainerは退役したまま。**（ADR-0044の決定1を引き継ぎ、jobに復旧jobとgoal review jobを足した）
   - **supervisor**: runtimeの`supervise`プロセス。claim、worker起動、validating、resume、jobの起動（review、復旧（旧triage）、plan review、goal review、observer）、verdictとanswerの適用、着地、runtimeが立てるplannerの起動と終了、後始末を行う。
   - **worker**: runごとにsupervisorが開くオンデマンドのworkspaceで、worktreeで作業するClaude session。
   - **planner**: proposal（決定7）を書くオンデマンドのworkspace。人が`dagq plan`で開くもの（`origin: person`）と、runtimeが立てるもの（`origin: runtime`。決定12、14、16、19）がある。goalとtaskを書き、proposalをsubmitする（決定8）。`ready`にはしない。
   - **inbox**: 唯一の常駐session。openなask（人が要る理由の分類を持つもの。決定41）を人に見せてanswerを書き戻し、それ以外のattentionを人に知らせる。自分では判断しない（[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定4）。
   - **observer**: supervisorのtimerで定期起動するheadlessのjob（決定4）。findingを記録・更新し、`blocked`のaskを上げる。
   - 検査はsupervisorが起動するheadlessのplan review jobが行う（決定10、11）。plan review jobは同時に1つで、runのreview jobと対になる。goalの達成の判断はheadlessのgoal review jobが行う（決定43）。
   - maintainerという役割、`[<repo>]dagq maintainer`のworkspace、`DAGQ_ROLE=maintainer`は無い。[ADR-0010](0010-maintainer-and-resident-supervisor.md)以降のADRに残るmaintainerの記述は書き換えず、[overview](../design/overview.md)の用語集で「review job / 復旧job（旧triage job） / observer / inboxのいずれか」に読み替える（レビューと着地はreview job、失敗runと止まったsessionの扱いは復旧job、継続的な監視はobserver、人への相談とanswerの実行はinbox。`up` / `down`などの操作は人がinboxかplannerのsessionから打つ）。
2. **jobは、cmux workspaceを持たないheadlessのClaude実行にする。**（ADR-0044の決定2を引き継ぎ、triage jobを復旧jobに広げ、goal review jobを足した）
   - 起動は`AgentProvider`のheadless実行のport（`headless_command`。Claudeでは`claude -p`）を使う。promptに入力のpathと出力のJSON schemaを渡し、stdoutのJSONを読む。未知のフィールドは拒否する。
   - jobは次の5種類。
     - **review job**: `awaiting_integration`のrunをレビューし、`pass` / `revise` / `concern`を返す（ADR-0040の決定2、[ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)）。
     - **復旧job**（旧triage job）: runtimeが直さなかったイレギュラーのalert（`failed` / `interrupted`のrunを含む）ごとに状況を読み、許された操作か`escalate`のverdictを返す（決定3、39、40）。
     - **goal review job**: 達成の判断の条件がそろったgoalを検査し、`achieved` / `gaps` / `ask`を返す（決定43）。
     - **plan review job**: submitされたproposalを検査し、`pass` / `revise` / `concern`を返す（決定10、11）。
     - **observer job**: supervisorのtimerで定期起動し、findingと`blocked`のaskを残す（決定4）。verdictは返さず、許されたCLIだけで書く。
   - review / 復旧 / plan review / goal reviewのjobのenvは`DAGQ_ROLE=reviewer`と`DAGQ_QUEUE`で、CLIは読むコマンドだけを許す。cwdはrepositoryのmain checkoutで、jobはrepositoryの文書とsourceを読める。observerのenvは`DAGQ_ROLE=observer`（決定4）。
   - jobの起動・終了・失敗はrun_eventsに記録する（`review_started` / `review_finished` / `review_failed`、`plan_review_started` / `plan_review_finished` / `plan_review_failed`、`observe_started` / `observe_finished`）。headless実行が失敗したとき（起動できない、非0終了、timeout、stdoutがschemaに合わない）は、対象を動かさず、inbox宛てのattentionかaskにする。review / 復旧 / plan review / goal reviewのtimeoutは`AgentProvider::review_timeout`（既定600秒）で、ループはプロセスを`try_wait`で見るだけで止まらない。
3. **失敗・中断したrunは復旧job（triage jobを広げたもの）にかけ、そのverdictでruntimeが動く。安全に自動化できる後始末はruntimeが行う。**（ADR-0044の決定3を引き継ぎ、triage jobを決定39・40の復旧jobに広げた。triageの部分は実装済み）
   - supervisorは`failed`になったrunと、下の自動`recover`で`interrupted`になったrunごとに、復旧jobを`failed` / `interrupted`のalertで起動する（決定39）。今の実装のtriage jobのverdict（`{"verdict": "retry" | "resume" | "ask", "reason": "...", "instruction": "..."}`）は、決定40のverdictの`retry` / `resume` / `escalate`に当たり、復旧jobの実装が入るまでこの形で動く。
     - **retry**: runtimeがtaskを`in_progress`から`ready`に戻す。次のclaimで新しいrunが作られる。taskの中身は変わらないので、plan reviewにはかけない（決定8のreadyの権限の例外）。前のrunのbranchにcommitがあるときの引き継がない`retry`は成果の破棄なので、復旧jobは選べない（決定40）。
     - **resume**: runtimeがrunを`failed` / `interrupted`から`needs_session`にし、自動resume（決定24）で同じsessionに続けさせる。試行3回の上限はrunごとの通算で数え、復旧jobからのresumeも含める（衝突だけの試行の扱いは決定24）。上限を超えたrunは決定24の「使い切ったとき」に従う。
     - **escalate**（今の実装の`ask`）: inbox宛てのaskを作り、jobの見立てをquestionに載せて待つ（決定40）。askは人が要る理由の分類を持つ（決定41）。`decide`のanswer（`retry` / `resume` / `cancel`）はsupervisorが適用する。
   - 復旧済みのrunのworkspaceのうち、sessionの要らなくなったものはruntimeが閉じる。
   - wrapperが死んでleaseの無いrunは、supervisorが`recover`まで自動で行う。`recover`の結果は`interrupted`で、taskを`ready`にはせず、復旧jobに回す。[ADR-0039](0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)（ADR-0012を置き換え）の引き継ぎはそのまま（`needs_session`のresumeのsessionへの広げ方は決定24）。[ADR-0003](0003-supervisor-owns-lifecycle.md)・[ADR-0007](0007-run-level-leases-parallel-execution.md)の「孤児runは自動再実行しない」は、再実行を復旧jobのverdictに委ねることで維持する。
4. **observerはsupervisorのtimerで定期起動するjobにし、run / task / goalの状態を変えず、findingと`blocked`のaskだけを書く。**（ADR-0041の決定4を改める。noteとdraft goalを書かせず、起動の間隔と条件は決定21）
   - 起動は`supervise --observe-interval`（既定を3時間に改める。決定21）と1日1回のdaily（`--observe-daily`）。0なら起動しない。supervisorが居ないときは動かない。同時に走るobserverは1つで、run slotを使わない。
   - 入力は`stats --since <前回の起動のcursor>`（決定21の閾値ごとの結果と`conflict_hotspots`を含む）、openなfindingと前回以降に更新されたfinding（決定18）、openなask、`graph`の`candidates`と`critical`。noteは読んでよいが、書かない。
   - 出力は次の2つだけ。
     - **finding**: 見つけた問題を決定18の記録として登録し、同じ問題なら既存のfindingを更新する。proposalにすべきと判断したfindingには、その理由を付けてproposalを求める印を付ける（決定19）。
     - **ask**: 今すぐ人の判断が要る詰まり（閾値超えで、待っても解けないもの）を`kind: blocked`のaskとしてinbox宛てに上げる。askは根拠のfindingに紐づけ（決定23）、optionsにobserverの見立てと「提案にする」（決定19）を載せる。同じfindingにopenなaskがあれば新しく作らない。
   - observerはnoteを書かず、draftのgoalもtaskも書かない。改善の提案は決定19の経路でruntimeが立てるplannerが作る。
   - runtimeは`DAGQ_ROLE=observer`のsessionからは許可したコマンドだけを受ける: 読み取り（決定22のCLIを含む）、findingの登録・更新・proposalを求める印・解消（決定18、19）、`ask --kind blocked`。それ以外（`note`、`add`、`goal add`、`ready`、`submit`、`integrate`、`recover`、cancel、`goal ready`、`goal close`、`answer`、`observe`、`supervise`など）は拒否する。observerは個々の詰まりを解消せず、proposalを作ることもplan reviewに出すこともできない。
5. **goalにdraft状態を持たせる。**（ADR-0041の決定5を引き継ぎ、observerのdraft goalをやめた）
   - `goal add --draft`でdraftのgoalを作り、`goal ready ID`でdraftを外す。draftのgoalに属するtaskは`candidates`に出ず、supervisorはclaimしない。
   - draftのgoalを書くのはplannerで、新しいgoalをproposalに入れるときに使う。plan reviewがそのproposalをpassにしたとき、runtimeがgoalのdraftも外す（決定11）。`goal ready`は引き続き使えるが、goalのdraftを外すだけで、所属taskを`ready`にはしない（taskは決定8のとおりplan reviewかbypassでだけ`ready`になる）。
   - ADR-0044より前にobserverが登録したdraftのgoalは、人が開いたplannerで人と採否を決める。採るなら、plannerがそのgoalとtaskを自分のproposalにしてsubmitし、採らないなら`goal close`（`abandoned`）にする。
   - [ADR-0009](0009-goal-groups-tasks.md)の「goalは状態機械を持たない」は、draftか否かの1点に限って改めたまま。進捗は従来どおりtaskの状態から導く。
6. **`up`はsupervisorとinboxだけを開き、plannerは`dagq plan`で開く。**（ADR-0041の決定6を変えずに引き継ぐ。実装済み）
   - `up`が作るworkspaceはsupervisor（in-cmux mode）とinboxだけ。常駐のplannerはやめ、`up`は`session_workspaces`のsupervisor・inbox以外の行（常駐plannerの`planner`行とmaintainerの行）を忘れる。残ったworkspaceは人が閉じる。
   - 人は`dagq plan`で人が開くplannerのworkspaceを開く。何度打っても新しいworkspaceを開き、複数同時に開ける。plannerは`planners`表の1行で、workspaceは[ADR-0026](0026-identify-workspaces-by-uuid-env-and-queue-group.md)のとおりUUIDで識別し、`--env`に`DAGQ_ROLE=planner`、`DAGQ_QUEUE`、`DAGQ_PLANNER_ORIGIN`、`DAGQ_PLANNER_ID`を持たせ、queueのworkspace groupに入れる。titleは表示専用で、[ADR-0028](0028-workspace-titles-are-repo-and-role.md)の`[<repo>]planner`にplannerのIDを足した`[<repo>]planner#<planner-id>`（runtimeが立てたものは後ろに対象のproposalかdraft taskを足す）。
   - `up` / `down` / 固定バイナリの更新は、人がinboxかplannerのsessionから打つ。
   - in-cmux modeのsupervisorが止まったときは、inboxの`watch`が`supervisor_stopped`で拾って人に知らせ、人が`up`を打つ。`watch --role inbox`は`ask_opened`・`ask_answered`・`supervisor_stopped`とそれ以外のattentionを受ける。in-cmux modeに自動再起動が無いこと（[ADR-0011](0011-cmux-socket-password-and-in-cmux-fallback.md)）は変えない。
7. **proposalを、plan reviewと差し戻しの単位にする。**（ADR-0041の決定7を変えずに引き継ぐ。実装済み）
   - proposalは、goal（0個以上）とtaskの束に、持ち主のplanner（workspaceのUUIDと、人が開いたかruntimeが立てたか）を結び付けたもの。plan reviewはproposal単位で検査し、reviseはproposalの持ち主に返す。
   - 1つのtaskは同時に1つのproposalにだけ属する。plannerは`add`や`goal add`で書いたgoal / taskを自分のproposalに入れる。既存のdraftのtask（保留・退避したtask、ADR-0044より前のobserverのdraft goalのtask）も、plannerが自分のproposalに入れてsubmitできる。
   - 持ち主のplannerが閉じた後に差し戻すときは、runtimeが立てた新しいplannerがproposalの持ち主になる（決定12）。
8. **taskの状態に`submitted`を足し、`ready`にするのはplan review jobだけにする。**（ADR-0041の決定8を変えずに引き継ぐ。ADR-0044の決定8を引き継ぎ、例外に復旧jobの`retry_inherit`と決定24の引き継ぐretryを足した。それ以外は実装済み）
   - `submitted`はplan review待ち。plannerが`dagq submit`でproposalを出すと、proposalのtaskが`draft`から`submitted`になる。supervisorは`submitted`のtaskをclaimしない。
   - `draft`は「まだ出していない」の意味だけになる。runtimeやjobが作ったdraft、保留、退避などは、submitしない限りreadyにならない。
   - `ready`にするのは、plan review jobのverdictを適用するruntime（決定11）と、`concern`のaskに人が`ready`と答えたときのruntimeだけ。plannerとjobはsubmitまでを行う。
   - 例外は2つの種類。人が明示したbypass（`ready --bypass-review`。bypassしたことをeventに記録する）と、復旧jobとtriageの`retry` / `retry_inherit`・決定24の引き継ぐretry（決定3、24、40。中身の変わらない同じtaskを戻すだけ）。bypassの無い`ready`は、どのroleから打たれても拒否する。
   - `goal close --verdict achieved`は所属taskに`completed` / `canceled`以外があれば拒否することを変えない。`submitted`のtaskもcloseを止める。
9. **taskの中身を編集できるのは`draft`と`submitted`のあいだだけにする。**（ADR-0041の決定9を引き継ぎ、編集のコマンド`edit`と、goal 29で足したtitleの編集を書いた）
   - `draft`と`submitted`のtaskは、`edit`でtitle・description・acceptance・`verification_commands`・`paths`・evidence・contextを編集できる。`submitted`のtaskを編集したら、そのproposalはplan reviewを受け直す（検査中なら、そのverdictは適用しない）。
   - `ready`のtaskは編集しない。変える必要があるときは、決定14の手順で`submitted`に戻してから直す。`in_progress`以降のtaskは編集しない。
10. **plan reviewの検査を、CLIの決まった規則とLLMに分ける。**（ADR-0041の決定10を引き継ぎ、入力に先例とADR-0046の手段を足した。ADR-0046の手段を使うこと以外は実装済み）
    - **機械的な検査**はCLIの`dagq lint`が決まった規則で判定する: 依存の循環、完了・cancel済みのtaskへの依存、存在しないtask / goalへの依存、宣言`paths`のglobの妥当性（[ADR-0029](0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)の`validate_path_globs`）、evidenceの値、宣言`paths`とverification・evidenceの整合、titleとacceptanceが空でないこと。plannerはsubmitの前に同じ検査を自分で打てる。`submit`もこの検査を行い、通らないproposalはsubmitできない。
    - **意味の検査**はplan review jobのLLMが行う: 他のtaskとの重複、既に実装済み、ADR・goalのconstraintsとの矛盾、acceptanceとdescription・兄弟taskのacceptanceの食い違い、同じファイルを触るtaskの間の依存の提案、他のsubmittedのproposalとの食い違い（決定15）、既にreadyのtaskとの食い違い（決定14）。重複と実装済みの候補はADR-0046の`search` / `related`で集め、候補だけをLLMで判断する（今のplan review jobは`Read` / `Grep` / `Glob`だけで`dagq`を打てず、`related`もまだ無い。jobにdagqの読み取りのコマンドを許すことと合わせて後続のtaskが実装する）。
    - repository固有の規則（AGENTS.mdのverificationの規則、ADR番号の割り当てなど）はruntimeに埋め込まない。plan review jobのpromptが、repositoryのAGENTS.md / CLAUDE.md、`docs/adr/`、goalのdocを読んで反映せよと指示する。
    - promptには、proposalのgoalとtask（description・acceptance・verification・paths・evidence・context・依存・優先度）、`lint`の結果、他のsubmittedのproposalと既にreadyのtaskの一覧、関係するgoalのdescription・acceptance・constraints（constraintsがtaskのdescriptionより優先）、人が答えたaskのうち先例の候補、verdictのschemaを渡す。
11. **plan reviewのverdictは`pass` / `revise` / `concern`にし、jobが自分でしてよい修正を4つに限る。**（ADR-0041の決定11を引き継ぎ、実装したverdictの形を書いた。実装済み）
    - verdictは`{"verdict": "pass" | "revise" | "concern", "reasons": [...], "summary": "...", "actions": [...], "reopen": [...], "precedents": [...]}`。`actions`はjobが自分でしてよい修正だけを持つ: readyにしてよいこと（passそのもの）、依存を足す（`add_dependency`）、優先度を下げる（`lower_priority`）、明らかな重複をcancelする（`cancel_duplicate`。重複先のtaskを示し、ADR-0046の重複の記録で残す）。これら以外の修正（description・acceptance・verification・pathsの書き換え、taskの分割、依存の削除、優先度を上げる）はjobにさせず、reviseでplannerに直させる。`reopen`は決定14、`precedents`は判断の根拠にした先例のaskで、reviseの指摘とconcernのaskのquestionに載る。
    - **pass**: runtimeが1トランザクションで`actions`を検査して適用し（1つでも適用できなければverdict全体をjobの失敗として扱う）、proposalのtaskを`submitted`から`ready`にし、proposalのdraftのgoalのdraftを外す。
    - **revise**: runtimeがproposalのtaskを`draft`に戻し、`reasons`をproposalの持ち主のplannerに返す（決定12）。同じproposalのreviseが2回（runのreviewの`MAX_REVISE_ATTEMPTS`と同じ）に達したら、3回目のplan reviewがpassでなければconcernとして扱う。
    - **concern**: 人の判断が要るもの。`approve_plan`のaskをinbox宛てに作り、`reasons`と`summary`をquestionに載せて待つ。optionsは`ready`（そのまま通す）/ `send_back`（人の理由を付けてplannerに差し戻す）/ `cancel`（proposalのtaskをcancelする）。answerはsupervisorが適用する（runの`approve_landing`と同じ形）。`ready`・`send_back`・`cancel`のanswerは`runtime_delivers`でattentionにしない（`send_back`は決定12のreviseと同じくplannerに理由を返す）。
    - jobが自分でcancelするのは明らかな重複だけにする。重複か疑わしいもの、既に実装済みに見えるもの、ADR・constraintsと矛盾するものはconcernにする。
    - plan review jobはqueue全体で同時に1つ。workerのslotには数えない。
12. **reviseはproposalの持ち主のplannerに返し、閉じていればruntimeが新しいplannerを立てる。**（ADR-0041の決定12を変えずに引き継ぐ。実装済み）
    - 持ち主のplannerのworkspaceが生きていれば、runtimeはplannerがidleになるのを待って、`reasons`と直す手順（直してsubmitし直す、自分の判断で直せないものは決定13の規則で人に聞く）を送る。
    - 閉じていれば、runtimeが新しいplannerのworkspaceを立て、proposalと`reasons`を初期promptに載せて続けさせる（runのresumeと同じ考え方）。新しいplannerは「runtimeが立てたplanner」で、proposalの持ち主になる。
    - runtimeが立てるplannerの同時の数には上限を置く。`supervise --runtime-planners`（既定1）で設定し、workerのslotとは別に数える。reviseのplanner、決定16のdraftのplanner、決定19のfindingのplannerはこの上限を共有する。人が開いたplannerは数えない。上限に達していれば、空くまで立てるのを待つ。
13. **人が開いたplannerとruntimeが立てたplannerで、人の判断の届け先を変える。**（ADR-0041の決定13を変えずに引き継ぐ。実装済み）
    - **人が開いたplanner**: 計画の意図が変わる修正（受け入れ条件、範囲、goalとの関係）は、そのworkspaceで人に聞く。reviseを渡してから`supervise --planner-timeout`（既定はrunの`resume_timeout`と同じ1時間）を過ぎても再submitが無ければ、inboxにattention（`planner_unresponsive`）で知らせる。この期限は、runtimeが立てたplannerに渡したreviseと、まだ誰にも渡せていないreviseにも同じく当てる。
    - **runtimeが立てたplanner**（人がいない）: 人の判断が要るものは`planner_question`のaskをinbox宛てに作る。answerはそのplannerのworkspaceにruntimeが`answer to ask <id>: ...`として送り、plannerが適用する（`worker_question`と同じ形。ADR-0022の決定2）。
    - plannerのworkspaceは、期限を過ぎても閉じない。inboxに知らせるだけにする。
    - runtimeが立てたplannerは、proposalをsubmitしたか、cancelしたか、対象のdraftを`keep_draft`で残したか、findingを決定19のとおり片付けたら、idleになったところで`/exit`され、終わったworkspaceはruntimeが閉じる（workerの`/exit`とcloseと同じ手順）。`planner_question`のanswerを待つ間は開いたままにする。answerを送る時点やreviseを返す時点でworkspaceが閉じていれば、決定12のとおり新しいplannerを立てて渡す。
14. **readyのtaskを変える必要があるときは、`submitted`に戻して新しいplannerに直させる。**（ADR-0041の決定14を変えずに引き継ぐ。実装済み）
    - plan reviewが、既に`ready`のtaskを変える必要がある（新しいproposalと食い違う、前提が崩れた）と判断したら、verdictの`reopen`にそのtaskと理由を書く。runtimeはそのtaskを`ready`から`submitted`に戻してclaimされないようにし、そのtaskだけの新しいproposalを作って、runtimeが立てたplannerに理由を渡して直させる。直したproposalは再びplan reviewを通る。
    - `in_progress`のtaskは直さない。plan reviewは、着地の後に直すtask（そのtaskに依存する）を足すようproposalの持ち主にreviseで求める。
15. **同時に出されたproposalは出された順に1件ずつ検査し、interruptだけ先にする。**（ADR-0041の決定15を変えずに引き継ぐ。実装済み）
    - plan review jobはsubmitされた順（submitの時刻の古い順）に1件ずつ起動する。検査のときは、まだreadyになっていない他のproposal（submitted、reviseで持ち主が直しているもの）と、既にreadyのtaskとも照らし合わせる。
    - 食い違えば後から出した方を差し戻す。検査中のproposalが先に出された他のproposalと食い違えば、検査中のものをreviseにする。後から出されたものと食い違うだけなら検査中のものは通し、後から出されたものはその検査のときにreadyのtaskとの食い違いとして差し戻される。
    - interruptの優先度（ADR-0040の決定4）を持つtaskを含むproposalは、他より先に検査する。gateは飛ばさない。
16. **runtimeやjobが作ったdraftは、1件ごとにruntimeが立てるplannerに採否を決めさせる。**（ADR-0041の決定16を引き継ぎ、task 282の人の決定で対象をfollow_upのdraftからruntimeやjobが作ったdraft全般に広げた。ADR-0037のfollow-up triage jobを置き換えたまま。ADR-0044の決定16を引き継ぎ、最後の項のgoal closeを決定43に合わせて改めた。最後の項を除き実装済み）
    - runtimeやjobがdraftを作るときは、出どころ（`follow_up`: `integrate`がreceiptの`follow_ups`から登録したもの、`goal_gap`: goalを判断するjobがgapから作ったもの）と、plannerに見せる材料を記録する。`integrate`がreceiptの`follow_ups`からdraft taskを登録し、`follow_up_registered`を記録することと、その形（titleとdescriptionだけ）は変えない（決定27）。`goal_gap`のdraftは決定43のgoal review jobの`gaps`から作る。
    - **対象**は、statusが`draft`でproposalに入っておらず、出どころの記録があり、閉じていないruntimeのplannerも閉じていない`planner_question`も無く、`keep_draft`で残されておらず、3回の上限に達していないdraft。人が`add`で作ったdraftは対象にならない。導入前から残っているfollow_upのdraftも対象にする。対象の順はtaskのIDの昇順。
    - supervisorは対象のdraft 1件ごとにplannerを1つ立てる（決定12の上限の中で）。立てることは`BEGIN IMMEDIATE`の中で対象の条件を再検査してから記録するので、2つのsupervisorが同じdraftにplannerを立てることはない。plannerがどれも選ばずに終わったdraftは次のplannerを立てて続けさせる。立てるのはdraft 1件あたり3回までで、超えたら`draft_planner_exhausted`を記録してinbox宛てのattentionにする。初期promptには、draftのtitle・description・context、出どころ（follow_upなら元のtaskとそのrunのreceiptの`summary`と`follow_ups`、goal_gapなら材料）、goal（title・description・acceptance・constraints・doc）と同じgoalの他のtaskを載せる。plannerは採否の前にADR-0046の`search`（`related`が入ればそれも）で既存のtaskを確かめる。
    - plannerは3つから選ぶ。
      - **採用**: acceptance・verification・paths・evidence・依存を補い、draftを自分のproposalとしてsubmitする。plan reviewを通ってreadyになる。submitが出自として、plannerがtaskの`context`の冒頭に「follow-up draft（task <元task-id> の run <run-id> の receipt が提案）」を書き、submitが`follow_up_adopted`（goal_gapは`draft_adopted`。`task_id`、`origin`、`source_task_id`、`source_run_id`、`by: "planner" | "person"`、`ask_id`、`depth`）で記録する。
      - **不採用**: draftをcancelし（重複ならADR-0046の`cancel --duplicate-of`）、理由をnoteに残す。
      - **判断できない**: `planner_question`のask（決定13）をinbox宛てに作る。optionsは`adopt` / `cancel` / `keep_draft`。answerはplannerのworkspaceに送られ、plannerが適用する。plannerが居なければruntimeが新しいplannerを立ててanswerを渡す。`keep_draft`ならdraftのまま残し、人が開いたplannerが後で扱う。
    - **自動で採用しない上限**（ADR-0037の決定6から続く）: 次のdraftは、runtimeが立てたplannerからは、人の判断（`planner_question`のanswerの`adopt`）を経ずにsubmitできない。CLIの`submit`がこれを拒否する。人が開いたplannerからのsubmitは人の判断を経たものとして扱い、拒否しない（`keep_draft`で残したdraftもこの経路で出せる）。
      - 閉じたgoalのfollow_up（draftのgoalがnullか、そのgoalが閉じている）
      - 深さ2以上のfollow_up
    - **深さ**は「人の判断を経ずに続いたfollow_upの段数」で、`tasks`の列`follow_up_depth`に持つ。`add`で人やplannerが登録したtaskは0。`integrate`がtask Pのrunの`follow_ups`からdraftを登録するとき、draftの深さはPの深さ + 1。runtimeのplannerが人に聞かずにsubmitしたtaskは深さをそのまま持つ。人がanswerで`adopt`を選んだtask、人が開いたplannerがsubmitしたtask、bypassで`ready`にしたtaskは0に戻す。migrationは既存のtaskを0にし、`follow_up_registered`の`task_id`が指すtaskのうちstatusがまだ`draft`のものを1にした。acceptanceが空のtaskは決定10の`lint`がsubmitを拒否する。
    - goal 22のfollow-up triage job、そのverdictの適用、`follow_up`のaskのanswerの適用、`task_leases`はこの決定で要らなくなり、task 282が消した（`follow_up`のkindは古い行を読むためだけに残る）。
    - goalのcloseはplannerではなく決定43のgoal review jobが判断する。所属goalのdraftのうち、plannerの待ち、openな`planner_question`、`keep_draft`で残ったものがあるgoalは、goal reviewの起動の条件を満たさない。`keep_draft`は人が開いたplannerで人と決めて片付ける。
17. **人に届くものはすべてinboxにし、plannerに返すのはそのplanner自身のproposalへのreviseと、そのplannerが作ったaskのanswerだけにする。**（ADR-0041の決定17を引き継ぎ、実装で足したattentionとfindingのaskを書いた。ADR-0044の決定17を引き継ぎ、inboxに届くものを人が要る理由を持つものに絞った）
    - inboxに届くもの: 決定41の人が要る理由の分類を持つask（`approve_landing`・`approve_plan`・`planner_question`・`blocked`（決定4、23）・`approve_goal`（決定43）・認証とコストのask（決定42）と、復旧jobが`escalate`にした`decide`・`stuck_exit`・`answer_prompt`・`stalled`）、`plan_review_failed`、`goal_review_failed`、復旧jobの失敗、`planner_unresponsive`、`draft_planner_exhausted`、決定19のfindingのplannerの上限超え、`push_failed`、`supervisor_stopped`など人の操作が要るattention。runtimeと復旧jobが直すもの（決定37の表）はinboxに届けず、`auto_repaired`に記録するだけにする。
    - **`plan_review_failed`**: plan review jobが失敗したら（決定2の失敗）、proposalを`submitted`のまま動かさず、`plan_review_failed`をinbox宛てのattentionにする。supervisorは同じproposalを自動ではもう一度かけない。人の指示で、inboxがbypassでreadyにするか、plannerかinboxが`submit --proposal`で出し直す。
    - plannerはruntimeからaskも報告も受けない。受けるのは、自分のproposalへのreviseと、自分が作った`planner_question`のanswerだけ（runtimeが立てたplannerは、初期promptで対象のdraft、proposal、findingを受け取る）。
18. **observerの検出を、構造のある記録findingにする。**
    - findingは1行の記録で、次を持つ: ID、種類（`kind`。例: `stall`、`failure`、`wait`、`capacity`、`threshold`（閾値の見直し）、`conflict_hotspot`。値は実装taskが決め、観測の都合で足してよい）、対象（`target`: `run` / `task` / `goal` / `queue`と、そのID）、対象の中で問題を見分ける`subject`（ファイルのpath、alertの種類、閾値の名前など。無くてよい）、1行の要約（`summary`）と見立て（`detail`）、影響（`impact`: `high` / `normal` / `low`）、最初と最後に見た時刻、発生回数、根拠のevent IDの一覧、状態（`status`: `open` / `proposed` / `resolved` / `dismissed`）、紐づいたproposal、proposalを求める印とその理由（決定19）。
    - **同じ問題は1件にまとめる。** 種類・対象・`subject`が同じで`resolved` / `dismissed`でないfindingがあれば、observerは新しい行を作らず、その行の最後に見た時刻・発生回数・根拠を更新する（CLIがこの一致を判定し、同じ3つを持つ閉じていないfindingは1件に限る）。状態も内容も変わらないfindingは書き直さない（決定21）。
    - **状態の遷移**: `open`は見つけて手当てがまだのもの。決定19のplannerがfindingに紐づけてproposalをsubmitしたら`proposed`になる。紐づいたproposalのtaskがすべて終わり（`completed`か`canceled`）、1つ以上が`completed`になったらruntimeが`resolved`にし、すべて`canceled`になったときとproposalが`canceled`になったときは`open`に戻す。observerは、対象の問題がもう起きていないと根拠を付けて判断したら`resolved`にできる。`resolved`の問題が再び起きたら、observerは同じfindingを`open`に戻して回数と根拠を足す（手当てが効かなかったことが残る）。人（askのanswer）とplannerは、手当てしないと決めたfindingを理由付きで`dismissed`にする。`dismissed`のfindingは再び起きても回数と根拠を足すだけで、自動では`open`に戻さず、proposalも求めない。
    - 遷移とその理由はrun_eventsに記録する（`finding_recorded` / `finding_updated`など。kindの名前は実装taskが決める）。根拠のevent IDは記録のフィールドに持ち、本文の文章に埋めない。
    - **note**は人とplannerの自由文のメモとして残す。observerはnoteを書かない（決定4）。findingを読んだ人やplannerが補足を書くときはnoteをfindingの対象に付けてよい。
19. **proposalにすべきfindingと、askの「提案にする」のanswerから、runtimeが立てるplannerがproposalを作る。**
    - **経路(a) observerの判断**: observerは、再発の回数と影響からproposalにすべきと判断したfindingに、理由を付けてproposalを求める印を付ける（`queue`が対象の`conflict_hotspot`のように、リファクタリングなどのtaskで手当てするもの）。
    - **経路(b) askのanswer**: findingに紐づいた`blocked`のask（決定23）のoptionsに「提案にする」（`propose`）を載せる。人が`propose`（または`propose: <理由>`）と答えたら、runtimeがそのfindingにproposalを求める印を付け、answerはruntimeが運ぶので（`runtime_delivers`）attentionにしない。`dismiss`（または`dismiss: <理由>`）の答えはfindingを`dismissed`にする。それ以外の答えは従来どおりinboxが人の指示で扱う。
    - supervisorは、印が付いて`open`で、閉じていないplannerも閉じていない`planner_question`も持たないfindingごとに、runtimeのplannerを1つ立てる（決定12の上限の中で、印の古い順）。立てることは`BEGIN IMMEDIATE`で条件を再検査してから記録する。どれも選ばずに終わったらもう一度立て、1件あたり3回を超えたらinbox宛てのattentionにする（決定16と同じ）。
    - 初期promptには、finding（種類、対象、要約と見立て、影響、回数、最初と最後に見た時刻、根拠のevent）、根拠を読むCLI（決定22）、askから来たならそのquestionと人のanswer、関係するgoal（対象がtask / run / goalならそのgoal）を載せる。
    - plannerは、proposalを作る前にADR-0046の`search` / `related`で既存のtask（同じ手当てを持つtask、実装済みのtask）を確かめる。そして次から選ぶ。
      - **既存のgoalへのtask**: 手当てが既存のopenなgoalの範囲に入るなら、そのgoalにtaskを書いて自分のproposalにする。
      - **新しいgoal**: 範囲に入るgoalが無ければ、draftのgoalとそのtaskを書いて自分のproposalにする（plan reviewのpassでdraftが外れる。決定5）。
      - どちらでも、submitのときにfindingをproposalに紐づけ、findingは`proposed`になる。以後は決定10〜15のplan reviewを通る。
      - **手当てしない**: 既存のtaskで足りる、もう起きていない、手当てが割に合わないと判断したら、理由を付けてfindingを`dismissed`にする（既存のtaskで足りるなら、そのtaskをfindingの理由に書く）。
      - **判断できない**: `planner_question`のask（決定13）をinbox宛てに作る。
20. **人へのエスカレーションは、plannerとplan review（AI）が判断する。人の承認を一律には求めない。**
    - 決定19のplannerが作ったproposalは、新しいgoalを含んでいても、plan reviewのpassで`ready`になる。人のanswerを待つのは、plannerが決定13の`planner_question`にしたときと、plan reviewが決定11の`concern`にしたときだけ。
    - plannerとplan reviewのpromptは、人に聞く基準を示す: 計画の意図や範囲を人が決めるもの（受け入れ条件、既存のgoalのconstraintsとの矛盾、ADRの決定を変えること、人が以前に答えた先例と食い違うもの）、影響が大きく取り消しにくいもの。それ以外は自分で決める。
    - 決定16の自動で採用しない上限（閉じたgoalのfollow_up、深さ2以上）はfollow_upの連鎖を止める柵で、そのまま残す。findingから作るproposalは連鎖しない（元がtaskのreceiptではなくobserverの観測）ので、この上限の対象にしない。
21. **observerは、変化の無いときに何もせずに終わり、MCPを読み込まずに起動し、3時間ごとに起きる。**
    - **変化の無いときは起動しない**: `observe`は入力を集める前に、前回のcursorより後のrun_eventsのうちobserver自身のもの（`observe_*`、observerが書いたfindingとaskのevent）以外が1件でもあるかを見る。無ければagentを起動せず、何も読まずに、起動しなかったことだけを`observe_finished`の`outcome: skipped`（kindの形は実装taskが決める）で記録して終わる。記録はtimerの期日の判定（run_eventsで数える）と決定22の履歴のために残す。dailyも同じ判定を24時間の窓に当てる。
    - **書き直さない**: 状態も回数も根拠も変わらないfindingは更新しない。同じalertが続いているだけなら、前回から増えた根拠のeventがあるときだけ回数と根拠を足す。
    - **MCPを読み込まない**: observerのheadless実行は、ユーザーやrepositoryの設定にあるMCPサーバーを読み込まずに起動する（Claudeでは`--strict-mcp-config`で空の設定を渡すなど。形は`AgentProvider`の実装taskが決める）。observerが要るのは`dagq`のCLIだけで、MCPサーバーの起動は時間とtokenを使い、observerの判断に要らない入力を増やす。
    - **間隔**: `supervise --observe-interval`の既定を3時間（10800秒）にする（人の決定 2026-09-25）。1日1回のdaily（24時間の傾向）は残す。
    - **入力に足すもの**: goal 30の閾値の妥当性の`stats`（決定35の閾値ごとの結果`stall_thresholds`と、決定32の結末の内訳。決定45の`auto_repairs`も読む）を読み、閾値の見直しが要ると判断したら種類`threshold`のfindingにする。`stats`の`conflict_hotspots`を読み、衝突の割合が閾値を超えて再発するファイルを種類`conflict_hotspot`、対象`queue`、`subject`にそのファイルのpathを持つfindingにする。これは決定19の経路でリファクタリング（ファイルの分割など）のproposalになる。
22. **記録を見るCLIを、人とobserver・planner・plan reviewが同じものとして使う。**
    - **`dagq findings`**: 閉じていない（`open` / `proposed`）findingを影響の大きい順（`impact`、次に発生回数、次に最後に見た時刻の新しい順）に返す。各行に紐づいたproposalとその状態、proposalを求める印、openなaskを付ける。`--all`と`--status`・`--kind`・対象で絞り込め、`findings ID`（形は実装taskが決める）で1件の根拠のeventまで読める。
    - **`dagq events --full`と絞り込み**: `--full`はrun_idを含む全フィールドとpayloadを切り詰めずに出す。`--run RUN`・`--task ID`・`--goal ID`・`--kind KIND`（繰り返し可）・時刻の範囲（`--since` / `--until`）で絞り込める。既定（attentionだけ、圧縮形）と`--after` / `--all`の意味は変えない。
    - **`dagq timeline RUN`**: runのeventを時系列に並べ、決まった長さを超える空白の区間ごとに理由を付ける。理由はrun_events、ask、runのディレクトリの記録（idle marker、backgroundの処理、receipt）から決まった規則で導く: `idle`（receiptの無いidle）、`waiting_ask`（openなaskの待ち）、`background`（backgroundの処理の実行中）、`after_receipt`（receiptの後の終了・validating・reviewの待ち）、`waiting_integration`（着地のslotの待ち）、`no_supervisor`（supervisorが居なかった）、どれにも当たらなければ`unknown`。LLMは使わない。
    - **`dagq observe --history`**: observationを1回ごとに、mode、outcome（`skipped`を含む）、入力の範囲（`since`とcursor）、作った・更新した・閉じたfinding、proposalを求めた印、作ったask、所要時間、dirで返す。`observe_finished`のpayloadにこれらを持たせる。
    - これらは読み取りのコマンドで、observerとreviewerも打てる（決定4がobserverに拒否する`observe`のうち、`observe --history`だけは読み取りとして許す）。reviewerのjobが`dagq`を打てるよう、jobに許すtoolも実装taskが広げる。observer・planner・plan reviewのpromptは、自由文を読ませる代わりにこれらのコマンドを示す。
    - **`dagq report`**（Markdown / HTMLのまとめ）は作らない。CLIの出力が揃ってから、別のgoalで決める。
23. **taskの無い`blocked`のaskは、findingごとにopenなもの1件にする。**
    - `blocked`のaskは根拠のfindingを持つ。openなaskの一意性は、taskの無い`blocked`ではfindingごとにする（今の「taskの無い`blocked`はqueueでopenなもの1件」を改める）。別々のqueue全体の問題を、それぞれのfindingのaskとして同時に人に聞ける。
    - この決定で、task 244（taskの無い`blocked`のaskが1件しか開けない）の問題はfindingの実装が解く。task 244は別のtaskとしては要らない。askとfindingの結び付けと一意性の変更は、findingの記録を実装するtaskが持つ。

### II. 判断を含まない手順をruntimeが行う（ADR-0019の決定1〜6）

ADR-0019の原則「判断を含まない手順はruntime（supervisorと`integrate`）が引き受け、人には判断だけを残す」は引き継ぐ。ADR-0019の原則のうち、[ADR-0016](0016-maintainer-notification-and-compact-output.md)の決定5（`integrate`は自動で呼ばれない）を維持するという部分は、review jobのpassでsupervisorが着地させる（ADR-0040の決定2）ことで既に改まっており、引き継がない。runtimeがsessionに送るもの（workerへの`/exit`・解消依頼・revise・衝突の依頼・receiptの書き直しの依頼・促し・answer・既知のダイアログへのキー、plannerへのrevise・answer）は、どれもruntimeが起動・監視しているsessionに限り、決定31の送信の確認を通す。inboxのterminalには打ち込まない。ADR-0019が足したrun_eventsのkind（`resume_started` / `resume_finished` / `push_finished` / `push_failed` / `push_skipped` / `follow_up_registered` / `evidence_missing` / `prompt_waiting`）はそのまま残す。

24. **`needs_session`のrunはsupervisorがresumeする。衝突だけのresumeは上限に数えず、使い切ったrunは前のbranchを引き継ぐretryにする。**（ADR-0019の決定1を引き継ぎ、上限の数え方・引き継ぎ・adoptを改めた）
    - resumeはworkerと同じ経路で起動する: `session` wrapper、run dirの`claude-settings.json`（`Stop` hookのidle marker）を使い、providerのコマンドは`claude --resume <run-id>`にする。起動したら`resume_started`を記録する。
    - 起動したsessionへ定型の解消依頼を送る。内容は理由（runの`last_error`、reviewの指摘、人の`send_back`の理由）と、runのbaseから現在のmainまでに着地したtaskのtitleとreceipt summary（mainのcommitの`Dagq-Task` trailerからtaskを引く）、「mainへrebaseして解消し、検証コマンドを再実行し、新しいheadでreceiptを書き直す」指示。
    - sessionがreceiptをworktreeの新しいheadで書き直してidleになったら、`resume_finished`を記録して[ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)のとおり`validating`→reviewに進む。前の試行で解消済みのrun（task 148の`resolved_head`）はsessionを開かない。
    - `integrate`を呼び済みのrunは`integration_approved`で表し、runtimeがそのまま着地まで進める。rebaseが再び衝突すれば再び`needs_session`になり、次のresumeに回る。
    - **試行の上限**: runごとに3回（`MAX_RESUME_ATTEMPTS`）。triageや復旧jobからの`resume`（決定40）も通算で数える。ただし、そのrunのreviewがpass済みで、`needs_session`にした理由がrebaseの衝突だけ（`integration_deferred`の衝突。検証の失敗、`evidence_missing`、`scope_violation`、人の`send_back`を含まない）の試行は、この3回に数えない。衝突だけの試行は別に数え、runごとに`[resume].conflict_only_limit`（既定5）で止める（同じ衝突が解けないまま回り続けないための柵）。
    - **使い切ったとき**: 数える試行が3回に達したか、衝突だけの試行が上限に達したrunは、次のように扱う。
      - reviewがpass済みで、最後の`needs_session`の理由が衝突だけなら、runtimeが自動で**引き継ぐretry**を行う（決定37の表）。runを`failed`（`reason: resume_exhausted`）にし、taskを`in_progress`から`ready`に戻し、次のrunに`inherit_from_run`（前のrunのID）を記録する。次のrunのworker promptは、前のrunの`refs/dagq/runs/<run-id>`のcommitを今のmainの上に載せ直し、衝突を解き、検証を再実行してreceiptを書くよう指示する。taskの中身（description・context）は書き換えないので、plan reviewにはかけない（決定8のreadyの権限の例外で、triageの`retry`と同じ扱い）。引き継ぐretryはtaskごとに1回まで（決定40の`retry_inherit`と同じ回数を数える）で、2回目は復旧jobに回す。`auto_repaired`（決定38、`repair: inherit_retry`）を記録する。
      - それ以外は、復旧job（決定39）に`resume_exhausted`のalertとして回す。今の実装（task 100）は`failed`にしてinbox宛ての`decide`のask（`retry` / `cancel`）を直接開くが、これを復旧jobの後に置き換える（復旧jobの`escalate`の`decide`のaskのoptionsは決定40）。
    - **adopt**: supervisorが入れ替わったとき、`needs_session`のrunで、resumeしたsessionのwrapperが生きている（heartbeatが30秒以内か`exited_at`が記録済み）のに、leaseがstale（旧supervisorのpidが死んでいるかheartbeatが古い）なものも、[ADR-0039](0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)の引き継ぎの対象にする（task 236の抜け）。引き継いだsupervisorはrun_eventsの最新の`resume_started`と送信の記録から`ResumeWatch`を再開し、解消依頼を2回送らない。wrapperが終了を記録しているか死んでいれば、今までどおり`resume_parked_runs`が次の試行に進む。`auto_repaired`（`repair: resume_adopted`）を記録する。
25. **`/exit`の時間切れでrunを手放さず、間隔を空けて再試行し、安全ならworkspaceを閉じて着地へ進める。**（ADR-0019の決定2を引き継ぎ、「`/exit`の再送はしない」を改めた）
    - `exit_request_timed_out`でも、`/exit`を送るcmuxの呼び出しの時間切れ（`backend_timeout`）でも、supervisorはleaseを手放さず、`runtime_error`でrunをabandonしない。
    - **再試行**: 送る前と時間切れの後に画面を読む。決定29の既知のダイアログならその規則で閉じる。入力欄に`/exit`が残っていればEnterだけを送り直す（決定31）。ダイアログの兆候が無く入力欄が空で準備できている（`input_ready`）ときだけ、`/exit`をもう一度送る。ダイアログの兆候がある画面には`/exit`もEnterも送らない（選択肢を押しうるため）。再試行の間隔と回数は`dagq.toml`の`[exit]`で設定し、既定は3回、30秒・60秒・120秒の間隔。再試行ごとに`exit_retried`（`attempt`、`cause`: `backend_timeout` / `exit_timeout`、`screen`: `input_ready` / `input_pending` / `dialog` / `unreadable`）を記録する。
    - **閉じて進める**: 再試行を使い切っても終わらないとき、次をすべて満たせば、runtimeはworkspaceを閉じ（sessionを終わらせ）、sessionが終わったものとして次へ進める: そのrunの最新のreviewの`verdict`が`pass`、worktreeがclean、receiptの`commit`が今のHEAD、rebaseの途中でない、閉じていない`worker_question`のaskが無い。reviewの後の`ExitWatch`なら着地へ、それ以外の段は`validating`へ進む（reviewがpassでない段ではこの経路を使わない）。`auto_repaired`（`repair: exit_forced_close`）を記録する。
    - 条件がそろわなければ、復旧job（決定39）に`stuck_exit`のalertとして回す。今の実装（task 104）は`exit_request_timed_out`の後にinbox宛ての`stuck_exit`のask（`exit` / `wait`）を直接開くが、これを復旧jobの後に置き換える。人が`recover`か`down`で止めるまでleaseを持って待つことは変えない。
26. **`integrate`は着地後にmainをoriginへpushする。**（ADR-0019の決定3を変えずに引き継ぐ。実装済み）成功は`push_finished`。`--no-push`で抑止し、`origin` remoteが無ければ`push_skipped`を記録する。pushの失敗は`push_failed`で、runは`integrated`のまま（着地は取り消さない）。pushの再試行は人が行い、inboxが`push_failed`を人に知らせる。
27. **`integrate`はreceiptの`follow_ups`をdraft taskとして登録する。**（ADR-0019の決定4を引き継ぐ。実装済み）着地したtaskと同じgoal（無ければgoalなし）に`draft`で登録し（各要素の`title`をtitle、`description`をdescriptionにする。どちらかが文字列でない要素は登録せず、その要素をeventに残す）、`Task.context`に元のtaskとrunを書き、1件ごとに`follow_up_registered`を記録する。採否は決定16のruntimeのplannerが決め、`ready`にするのは決定8のplan reviewだけ（ADR-0019の「`ready`にするか、cancelするかは人の判断」はADR-0041の決定16で改まったまま）。
28. **taskは要求するevidenceを持つ。**（ADR-0019の決定5を変えずに引き継ぐ。実装済み）`add --evidence e2e`（繰り返し可。値はreceiptのcheck名: `tests` / `e2e` / `subagent_review`）で指定する。`validating`で、要求したcheckがreceiptで`passed`でないか`evidence_or_reason`が空なら、runを`failed`ではなく`needs_session`（reason: `evidence_missing`）にし、`evidence_missing`を記録して決定24のresumeで補わせる。判定は`Receipt::check`より先に行う: 要求したcheckが`failed`か`not_applicable`か空のevidenceなら`evidence_missing`で、要求していないcheckと`result`の`failed`は`Receipt::check`が`failed`にする。
29. **supervisorはprompt待ちを検知し、既知のダイアログだけは決まった規則で閉じる。**（ADR-0019の決定6を引き継ぎ、「キーは送らない」を改めた）
    - `agent_started`の後、receiptもidle markerも無いまま一定時間止まったrunと、決定25・31で画面を読んだときについて、supervisorが画面を読み、ダイアログの兆候（先頭に`❯`を持つ番号付き選択肢、`Esc to cancel`など）があれば`prompt_waiting`を1回記録する。画面の読み取りと兆候の判定は`WorkspaceBackend` / `AgentSignals`の後ろに置き、domain / applicationはcmuxを直接参照しない（[ADR-0013](0013-layered-architecture-and-type-function-style.md)）。
    - **既知のダイアログ**は、`infrastructure::claude`が固定の一覧（画面の文言の型と、送るキー）として持ち、次の2つから始める。一覧に足すには、安全の条件と送るキーを決めるADRか、本ADRの決定37の表に行を足す変更が要る。
      - **「Background work is running」の確認画面**（`/exit`に対して出る）: worktreeがcleanで、receiptの`commit`が今のHEADを指し、supervisorが`/exit`を要求した後の段のときだけ、「Exit and stop tasks」を選ぶキーを送る。条件がそろわなければ送らない。
      - **Settings / Usageのパネル**（`/status`・`/usage`などで開いたまま入力を塞ぐもの）: Escを1回送って閉じる。権限や作業の中身に触れないので、段を問わない。
    - 送ったら`auto_repaired`（`repair: dialog_answered`、`dialog`、`key`）を記録し、決定31の確認で閉じたかを確かめる。閉じなければ2回目は送らない。
    - 一覧に無いダイアログ（trust、権限の確認、auto mode、未知の選択肢など）にはキーを送らない。trustと権限とauto modeへの応答は権限の判断なので、runtimeも復旧jobも答えない。復旧jobは状況を読んで人が要る理由の分類（決定41の`scope`）を付けてinboxに上げる。今の実装（task 100）はinbox宛ての`answer_prompt`のaskを直接開くが、これを復旧jobの後に置き換える。

### III. 止まったworkerのsessionの検知（ADR-0043の決定1〜7）

ADR-0043の原則「止まったworkerのsessionはsupervisorが決まった規則で検知し、まずworkerに一度だけ直させる」は引き継ぐ。「それでも動かなければinboxのaskで人に渡す」は、決定39の復旧jobを先に通すと改める。文を重ねたり、既知のダイアログ（決定29）以外のダイアログにキーを送ったりしないことは変えない。

30. **receiptの無いidleを検知し、一度だけ促し、なお止まっていれば復旧jobにかける。**（ADR-0043の決定1を引き継ぎ、askの前に復旧jobを通すと改めた。最初のsessionの促しと`stalled`のaskはtask 288で実装済みで、resume / reviseの段は今の実装では別の経路（未解消のresume、`approve_landing`）で終わる）
    - **対象**: supervisorがsessionを監視しているworkerのrun。最初のsession（`SessionWatch`）、resumeしたsession（`ResumeWatch`）、reviseを送ったsession（`ReviseWatch`）の3つの段（以下「段」）で見る。verdictの後の`ExitWatch`とreceiptの後のbackgroundの処理（Bの型）は対象外で、決定36が扱う。
    - **判定**（tickごと）: 次をすべて満たす状態が`[stall].idle_without_receipt_secs`（既定1200秒=20分）続いたら「receiptの無いidle」とする。
      - その段のidle markerがあり、その段で最後にsessionへ届いた入力（段の開始、supervisorが送った文、人の入力。決定31の送信の印で知る）より新しい。つまりsessionは入力を処理し終えてturnを閉じている。
      - その段で書き直されたreceiptが無い（最初のsessionはreceiptが無い。resume / reviseは段の開始より新しいreceiptが無い）。
      - そのrunに閉じていない`worker_question`のask（ADR-0022の決定2）が無い。answerの送信待ちのworkerは人を待っているので止まっていない。
      - そのrunに閉じていない`answer_prompt`のaskも、その段の`prompt_waiting`（決定29）も無い。ダイアログはそちらの経路が扱う。
      - 経過は最新のidle markerのmtimeから数える。markerの`background_tasks`に`running`の処理があってもidleに数える（task 182の型。backgroundの処理の有無はaskに載せる）。
    - **促し**: 段ごとに1回だけ、定型の促しの文をworkerのterminalへ送る（送信は決定31の確認の対象）。文は「receiptの無いまま止まっている。作業が終わったならcommitしてreceiptを書く。判断が要るなら`dagq ask --run <run-id> --kind worker_question`で聞く。backgroundの処理を待っているなら、何を待っているか、いつ終わる見込みか、戻らなければどうするかをterminalに書く」の趣旨で、run id・経過時間・markerの`background_tasks`の要約を含める。文面は`src/application/prompt.rs`の他の依頼文と同じ場所に置く。送る前に`stall_nudged`（payload: `phase`（`session` / `resume` / `revise`）、`idle_secs`、`threshold_secs`、`background_running`、`background_tasks`の要約）を記録する。
    - **復旧jobとask**: 促しの後にsessionが応答し、再びidleになって、上の判定がもう一度`idle_without_receipt_secs`続いたら（促しの送信が処理されなかった場合は決定31の経路が先に扱う）、復旧jobを`stalled`のalertで起動する（決定39）。復旧jobが直せば（`send_instruction`、`stop_processes`、`resume`など）askは作らない。`escalate`か`confidence: low`なら、inbox宛ての`kind: stalled`のaskを作る（人が要る理由は決定41の`recovery_failed`。`asked_by`は`supervisor`、`reason: idle_without_receipt`）。questionは人に宛てて、run id・task id・段・receiptの無いidleの経過時間・促しを送った時刻・markerの`background_tasks`（`running`の処理の`description`と`command`、無ければ無いこと）・`WorkspaceBackend::capture`で読んだ画面の末尾15行（`stuck_exit`と同じ`screen_tail`。読めなければその旨）と、復旧jobの見立て、選択肢の意味を持つ。optionsは次の2つ。
      - `wait`: sessionに触らず待つ。supervisorは判定の計時をanswerの時刻からやり直し、なお`idle_without_receipt_secs`止まっていれば新しい`stalled`のaskを作る（促しは送り直さない）。
      - `intervene`: 人がsessionに手を入れる。inboxはanswerを受け、人の指示で`dagq-recover` skillの手順（画面を読み、backgroundの処理を止めるか、指示を打ち込むか、runを止めて`recover`する）に従う。supervisorは計時をやめ、sessionが次に入力を受けるかreceiptを書くまで同じ段で新しいaskを作らない。
    - 同じ（run、kind）のopenなaskがあれば作らない（askの重複抑止。ADR-0022）。askが開いている間にsessionが自分で動いた（新しい入力の印かreceipt）ら、supervisorはそのaskを`stuck_exit`と同じく閉じる（未回答ならruntimeがanswerを書いて`ask_answered`（`runtime_closed: true`）を記録する。これはattentionではない）。runが終わったとき（sessionの終了、`recover`、abandon）も同じく閉じる。`stalled`のaskの`ask_opened`は、他のkindと同じくinboxのattention（`answer ask <id>`）になる。
    - 引き継いだ（adoptした）runは、run_eventsの`stall_nudged`とaskの有無からどこまで進んだかを読み、促しもaskも2回出さない。
31. **supervisorが送ったものは、処理されたかを確かめ、処理されていなければEnterだけを送り直し、なお動かなければ復旧jobにかける。**（ADR-0043の決定2を引き継ぎ、`/exit`も対象にし、askの前に復旧jobを通すと改めた。画面の入力欄を読む確認とEnterの送り直しはtask 285で実装済み）
    - **対象の送信**: resumeの解消依頼（決定24）、reviseの差し戻し（ADR-0027）、衝突の解消依頼（ADR-0027の決定4）、receiptの書き直しの依頼（決定38）、workerの質問へのanswerの配送（ADR-0022の決定2）、決定30の促しの文、決定42の「続けて」の文、そして`/exit`（task 285の実装どおり。`/exit`の時間切れの後の扱いは決定25）。
    - **画面の確認**（task 285の実装）: 打った後に画面を読み、入力欄に打ったものが残っていれば（`input_pending`）Enterだけを送り直す（`SUBMIT_RETRIES`、3回まで）。ダイアログの兆候があれば何も送らない。3回で残れば`submit_unconfirmed`を記録する。送ってから`start_wait`（既定60秒）の間に作業の兆候が無ければ、入力欄が空なら同じ文面を1回だけ送り直し（`submit_resent`）、それでも兆候が無ければ`submit_not_started`を記録する。下の処理された印による確認は、この画面の確認と併せて使う。
    - **処理された印**: sessionが送った文を入力として受け取ったこと。Claude adapterはrunごとの`claude-settings.json`に`Stop` hookと並べて`UserPromptSubmit` hookを書き、hookは入力を受けるたびに`<run-dir>/prompt-submit.json`を一時ファイル + renameで書く。markerの読み取りはidle markerと同じく`AgentSignals`（`infrastructure::claude`）の後ろに置き、applicationは送信の時刻とmarkerのmtimeを比べる。送信の時刻より新しい送信のmarker、idle marker、receiptのどれかがあれば「処理された」とする。送信のmarkerを持たないprovider（hookの無い古いClaude Codeを含む）では、idle markerとreceiptだけで判定する。
    - **確認**: 送信から`[stall].send_confirm_secs`（既定60秒）以内に処理された印が無ければ、画面を`WorkspaceBackend::capture`で読む。決定29と同じダイアログの兆候（`detect_prompt`）があれば、Enterは送らず、既知のダイアログなら決定29で閉じ、そうでなければすぐ復旧jobにかける（`stalled`のalert、`reason: send_unconfirmed`、ダイアログの種類）。この経路のダイアログは決定29が`prompt_waiting`として扱う（決定29は画面を読んだときの検知に広げた）。兆候が無ければEnterを1回だけ送り（`WorkspaceBackend`にEnterだけを送るportのmethod（`send_enter`）を足す。今のportは`send_text` / `capture` / `send_exit`だけ）、`send_retried`（payload: `send`（`resume` / `revise` / `rebase` / `answer` / `nudge` / `receipt_rewrite` / `continue` / `exit`）、`threshold_secs`、`screen_tail`）を記録する。送り直してから`send_confirm_secs`以内に処理された印が無ければ、`send_unconfirmed`（payload: `send`、`threshold_secs`、`waited_secs`）を記録し、復旧jobを`stalled`のalert（`reason: send_unconfirmed`）で起動する。復旧jobが`escalate`にしたときだけ、inbox宛ての`kind: stalled`のask（`reason: send_unconfirmed`）を作る。questionは送った文の種類と先頭、送信とEnterの時刻、画面の末尾15行、復旧jobの見立てを持ち、optionsは決定30と同じ`wait` / `intervene`。今の実装（task 285）の`submit_unconfirmed` / `submit_not_started`の後の`answer_prompt`のaskも、同じく復旧jobの後に置き換える。
    - **送信の順序**: 処理が確かめられていない送信がある間、supervisorは同じsessionに次の文も`/exit`も送らない。task 205のように、処理されていない文の後ろに`/exit`がつながることを防ぐ。例外は[wrapperが黙ったsession](../design/supervisor-lifecycle.md#wrapperが黙ったsession)の`/exit`で、その扱いは今のまま変えない。wrapperが黙っている間は、決定30の促しも決定31の確認とEnterの送り直しも行わない。
    - 文そのものは、入力欄が空で作業の兆候も無いとき（依頼が消えたとき）の1回を除いて送り直さない。貼られた文が入力欄に残っているなら、Enterだけで送れる。文を重ねると、処理されていた場合に同じ依頼が2回届く。`/exit`の再送は決定25の条件（入力欄が空で準備できている）に限る。
32. **`stalled`のaskの答えと、検知のその後を記録する。**（ADR-0043の決定3を引き継ぎ、復旧jobの結末を足した）
    - askのkindに`stalled`を足す（`asks`のCHECKに加えるので、schemaの`user_version`を上げて`migrations/`に追加する）。`reason`（`idle_without_receipt` / `send_unconfirmed` / `long_background`）はaskの`ask_opened`のpayloadとquestionに持つ。
    - askが答えられ、inboxと人が`dagq-recover` skillに従ってsessionを扱う経路は`stuck_exit`・`answer_prompt`と同じ。`cmux notify`はaskの登録のときに1回だけ送る（ADR-0022の決定5）。
    - 検知（`stall_nudged`、`send_retried`、復旧jobの起動（`recovery_requested`）、`stalled`のaskの`ask_opened`）ごとに、その結末を`stall_resolved`（payload: `detection`（`nudge` / `enter_retry` / `recovery` / `ask`）、`threshold`（決定33の設定名）、`threshold_secs`、`detected_after_secs`（receiptの無いidleの検知は、その判定を満たし始めた時刻（最新のidle markerのmtime）から検知までの秒。送信の検知は送信から検知までの秒）、`outcome`、`resolved_after_secs`（検知から結末までの秒））として1回記録する。`outcome`は次のどれか。
      - `resolved_by_nudge`: 促しの後、次の判定までにreceiptかworker_questionのaskが書かれた。
      - `resolved_by_enter`: Enterの送り直しの後、送信が処理された。
      - `resolved_by_itself`: askが開いている間に、supervisorも人も入力を送らずにsessionが動いた（`runtime_closed`でaskを閉じた場合）。
      - `answered_wait`: askの答えが`wait`だった（閾値が早すぎた疑い）。
      - `answered_intervene`: askの答えが`intervene`だった（人が手を入れた）。
      - `resolved_by_recovery`: 復旧jobの操作の後に、sessionが動いたかreceiptが書かれた。
      - `escalated`: 促しやEnterの送り直しや復旧jobでは解消せず、次の検知（復旧job、`stalled`のask）に進んだ。その先の結末は次の検知の`stall_resolved`が持つ。
      - `run_ended`: 結末の前にrunが終わった（`recover`、abandon、cancel、sessionの終了）。
    - **見逃しの疑い**も記録する。supervisorが送っていない入力（送信のmarkerが、supervisorの送信の記録の無い時刻に更新された）を、receiptの無いidleが続いている段で観測し、そのときの経過がまだその閾値を超えていなければ、閾値を超える前に人が気づいて手を入れたとみなし、`stall_preempted`（payload: `threshold`、`threshold_secs`、`idle_secs`（人の入力までの経過））を記録する。receiptの無いidleのまま人が`recover`したrunも、`recover`のeventから同じく数える。Claude Code自身が差し込む入力（backgroundの処理の完了通知など）が`UserPromptSubmit`を発火させる場合は、hookの入力で人の入力と区別できるものだけを数え、区別できないものは数えない（hookのfieldの確認は実装taskが行う）。
33. **閾値はrepositoryの`dagq.toml`の`[stall]`で設定し、既定値は人の決めた値にする。**（ADR-0043の決定4を変えずに引き継ぐ。実装済み）

    | 設定名 | 既定値 | 意味 |
    | --- | --- | --- |
    | `idle_without_receipt_secs` | 1200（20分） | receiptの無いidleが促しまで続く時間と、促しの後にaskまで続く時間（同じ値を2回使う） |
    | `send_confirm_secs` | 60 | 送った文が処理されるのを待つ時間と、Enterの送り直しの後に待つ時間 |
    | `background_alert_secs` | 1800（30分） | `stats`がbackgroundの処理を「長く動く」とするまでの時間（決定34） |

    - 値は正の整数（秒）。ADR-0040の決定3の`[run.env]`と同じく、runtimeが読むのはmain checkoutの作業ファイルの`dagq.toml`で、書式の誤り（未知のkey、整数でない値、0以下）はエラーにする。`[stall]`が無いか、keyが無ければ既定値を使う。この repositoryは`dagq.toml`を置かないので既定値で動く。
    - supervisorは起動時に読み、読んだ値をtaskの無いrun_eventsに`stall_config_loaded`（payload: 3つの値）として記録する。値を変えたら`down --wait` → `up`で起動し直す。`stats`は走っているsupervisorが最後に記録した`stall_config_loaded`の値でalertを判定し（ファイルだけを変えて起動し直していないときに、supervisorが使っていない値で検知の漏れを数えないため）、記録が無いときだけファイル（無ければ既定値）を読む。
    - 検知のeventはそのときの`threshold_secs`を持つので、閾値を変えた前後の結果を分けて集計できる。
    - 既定値と書式は[supervisor-lifecycle](../design/supervisor-lifecycle.md)に書く。
34. **`stats`は走っているrunのalertを返す。**（ADR-0043の決定5を引き継ぎ、`long_background`を復旧jobの入力にした） ADR-0040の決定5の項目に、`running_alerts`（runごとの配列）を足す。終わったrunの集計と違い、今の状態から導く: run_events・asksに加えて、run dirのidle markerと送信のmarkerとreceiptのmtime、cmuxのworkspaceの一覧を読む。新しい表は持たない。alertは次の4種類。
    - `idle_without_receipt`: 決定30の判定を満たし、経過が`idle_without_receipt_secs`を超えたrun。促し・askの有無を添える（supervisorが検知していれば促しかaskがあるはずなので、無ければ検知の漏れ）。
    - `long_background`: markerの`background_tasks`に`running`の処理があり、その処理が最初に`running`で現れたmarkerから`background_alert_secs`を超えたrun。receiptの前後を問わない。supervisorも同じ判定をtickごとに行い、当てはまれば復旧jobを`long_background`のalertで起動する（決定39。task 290）。
    - `running_outlier`: `running`の経過（claimから今まで）がそのgoalの作業時間の中央値の2倍を超えたrun（ADR-0040の決定5の閾値超えと同じ基準を走っているrunに当てる）。
    - `workspace_mismatch`: `running`などsessionを持つはずのrunのworkspaceが`cmux workspace list`に居ない、またはqueueの`[dagq]`のworkspace groupに、どの走っているrunにも`session_workspaces`にも対応しないworkerのworkspaceが居る。
    - cmuxに接続できないときは、`workspace_mismatch`だけを`unavailable`（理由付き）にし、他のalertは返す。`stats`自体は失敗させない。
35. **`stats`は閾値ごとの検知の結果を返す。**（ADR-0043の決定6を引き継ぎ、observerの出力をfindingにした） `stall_thresholds`（設定名ごと）に、`--since`以降の検知の件数（`detection`ごと）、`outcome`の内訳（決定32）、`detected_after_secs`と`resolved_after_secs`の中央値と最大値、`stall_preempted`の件数、使われた`threshold_secs`の値ごとの内訳を返す。集計はrun_eventsから再導出する（ADR-0040の決定5と同じ）。observer（goal 31）はこれを入力に読み、`answered_wait`が多い（早すぎる）、`stall_preempted`が多い（遅すぎる）、`running_alerts`の`idle_without_receipt`に促しもaskも無い（検知の漏れ）などを見て、閾値の見直しが要ると判断したらfinding（決定18、種類`threshold`）にする。observerは止まったsessionを自分では検知しない。
36. **Bの型（receiptの後のbackgroundの処理）は、`/exit`の経路と復旧jobで扱う。**（ADR-0043の決定7を改めた）receiptの後の`ExitWatch`はidle markerが`running`のbackgroundの処理を示している間`/exit`を送らずに待つ（今の実装。上限は`resume_timeout`）。その後の`/exit`の時間切れは決定25で、「Background work is running」の確認画面は決定29で、終わらない孤児のプロセスは`long_background`のalertから決定40の`stop_processes`で扱う。決定31の送信の順序（処理が確かめられていない送信の後ろに`/exit`を送らない）は変えない。

### IV. イレギュラーの3層、復旧job、人が要る理由、goal review、ディスク（新しい決定）

37. **イレギュラーは、runtimeの自動修正 → 復旧job → inboxの順に扱い、ケースごとの担当と自動で直す条件を決めておく。**
    - **runtime**は、下の表の条件がすべてそろうときだけ、決まった操作で直す。条件は画面・worktree・receipt・run_eventsから決まった規則で確かめられるものに限る。1つでも欠ければ直さず、復旧jobに回す。
    - **復旧job**（決定39、40）は、runtimeが直さなかったものと、runtimeの規則では決められないものを、状況を読んで許された操作で直す。直せないか自信が無いときだけinboxに上げる。
    - **inbox**には、決定41の人が要る理由のどれかに当たるものだけを出す: 受け入れ条件・ADR・goalの決定と食い違う着地や計画（reviewとplan reviewの`concern`、goal reviewの`ask`）、成果を捨てるかどうか（cancel、引き継がないretry、goalの`abandoned`）、認証、コスト・資源、復旧jobが直せなかった・自信が無いと返したもの。
    - ケースごとの担当（2026-09-24〜25に起きたもの）:

    | ケース | 担当 | 自動で直す条件と操作 |
    | --- | --- | --- |
    | 依頼・`/exit`が入力欄に残り送信されない | runtime（決定31） | 打った後に画面を読み、入力欄に残っていればEnterだけを送り直す（task 285で実装済み、`/exit`も対象） |
    | cmuxの時間切れや終了の時間切れで`/exit`が届かない | runtime（決定25） | 間隔を空けて再試行する。使い切っても、reviewがpass・worktreeがclean・receiptがHEADなら、workspaceを閉じて着地へ進める |
    | 既知のダイアログで止まる（Background work is running、Settings / Usageのパネル） | runtime（決定29） | Background workはworktreeがcleanでreceiptがHEADを指す`/exit`の後の段だけ「Exit and stop tasks」を選ぶ。Settings / UsageはEscで閉じる |
    | supervisorの入れ替え後に`needs_session`のresumeが拾われない | runtime（決定24） | resumeのsessionのwrapperが生きていてleaseがstaleなら、adoptの対象にして`ResumeWatch`を再開する |
    | rebaseの後にreceiptが書き直されない | runtime（決定38） | sessionがidleで、worktreeがclean、rebaseの途中でなく、HEADが今の`base`（resumeならmain）を含み、receiptの`commit`がHEADでなければ、決まった文面でreceiptの書き直しを1回促す |
    | 衝突だけでresumeを使い切る | runtime（決定24） | reviewがpass済みで理由が衝突だけの試行は3回に数えない。使い切ったら前のrunのbranchを引き継ぐretryを自動で行う |
    | backgroundの処理が終わらない（孤児のプロセスがパイプを握る） | 復旧job（決定40） | `long_background`のalertを受け、そのrunのworktreeに属する孤児のプロセスだけを止める |
    | receiptの無いidleが促しの後も続く、送った文が処理されない | 復旧job（決定30、31） | 画面とmarkerを読み、許された操作（指示の送信、待つ、resume）で直す |
    | 失敗・中断したrun | 復旧job（決定3、39） | 今のtriageのretry / resumeに、引き継ぐretryなどの操作を足す |
    | ログインが切れて止まる | inbox（決定42） | 認証は人にしかできない。queueで1件のaskにまとめる |
    | 利用上限に達して止まる | inbox（決定42） | コストは人が決める。queueで1件のaskにまとめる |
    | ディスクの空きが足りない | runtime → inbox（決定44） | claim / 検証を控えて掃除を自動で走らせ、それでも足りないときだけinbox |
    | testがworkerをSIGTERMで止める（exit 143） | 復旧job（決定39） | 原因の調査は別のtaskで行う。当面は今のtriageと同じくresumeを選ぶ |

38. **runtimeの自動修正は、安全の条件を確かめてから行い、1件ごとに`auto_repaired`を記録する。**
    - 自動修正は、条件を確かめた時点と操作の間に状態が変わらないよう、確かめてから操作までをrunのleaseを持った1回のtickの中で行い、run_eventsの書き込みはleaseの付いた書き込みにする。条件は決まった規則で判定し、LLMを使わない。
    - 1件ごとに、既存の個別のevent（`submit_retried`、`resume_started`など）に加えて`auto_repaired`を1件記録する。payloadは`repair`（修正の種類）、`layer`（`runtime` / `recovery`）、`conditions`（確かめた条件と値。例: `review: pass`、`clean: true`、`receipt_commit`、`head`）、`detail`。`repair`の値は`submit_enter_retry`、`exit_retry`、`exit_forced_close`、`dialog_answered`、`resume_adopted`、`receipt_rewrite_requested`、`conflict_resume_uncounted`、`inherit_retry`、`disk_cleanup`と、復旧jobが適用した操作（決定40の`action`の名前）から始め、足すときは実装taskが決めてよい（名前を変えるにはADRが要る）。
    - 同じ修正を同じ段で繰り返さない。種類ごとに段あたりの回数の上限を持ち（Enterの送り直しは3回、`/exit`の再試行は`[exit]`の回数、ダイアログへのキーとreceiptの書き直しの促しは1回）、上限を超えたら復旧jobに回す。
    - **receiptの書き直しの促し**: `SessionWatch` / `ResumeWatch` / `ReviseWatch`のpollで、sessionがidleで（idle markerが最後に送った入力より新しい）、worktreeがclean、rebaseの途中でなく、HEADがrunの`base_commit`（resume・衝突の依頼の段では依頼を送った時点のmain）を含み、receiptがあってその`commit`がHEADでないとき、「receiptを今のHEAD <sha>で書き直す」趣旨の決まった文面を段ごとに1回送る（決定31の確認を通す）。`auto_repaired`（`repair: receipt_rewrite_requested`、`receipt_commit`、`head`）を記録する。書き直されずに再びidleになったら、その段の今の扱い（resumeなら未解消、reviseなら`approve_landing`）に進む。

39. **triage jobを復旧jobに広げ、runtimeが直さなかったイレギュラーのalertを受けさせる。**
    - 復旧jobは、今のtriage job（決定2・3）を置き換えるheadlessのjobで、run 1件のalert 1件ごとに起動する。envとCLIの制限はtriage jobと同じ（`DAGQ_ROLE=reviewer`、読むコマンドだけ）で、jobは自分では何も変えず、verdictを返す。状態を変えるのはverdictを適用するruntimeだけ。
    - **入力になるalert**（run_eventsの`recovery_requested`、payloadは`alert`と根拠のevent ID）:
      - `failed`: runが`failed`になった（receiptの`failed`、validatingの失敗など。今のtriageの対象）
      - `interrupted`: wrapperが死んで自動`recover`で`interrupted`になった（今のtriageの対象）
      - `resume_exhausted`: 決定24の上限を使い切り、引き継ぐretryの条件に当たらない
      - `stuck_exit`: 決定25の再試行でも`/exit`で終わらず、閉じて進める条件もそろわない
      - `prompt_waiting`: 既知でないダイアログで止まっている（決定29）
      - `stalled`: receiptの無いidleが促しの後も続く（決定30）か、送った文が処理されない（決定31の`submit_unconfirmed` / `submit_not_started`）
      - `long_background`: backgroundの処理が`[stall].background_alert_secs`を超えて動いている（決定34のalert。task 290）
    - 同じrunの復旧jobは同時に1つで、run slotの数え方は今のtriageと同じ。alertが重なったら1つのjobにまとめて渡す。同じrunで復旧jobを起動するのはalertの種類ごとに3回までで、超えたら決定41の`recovery_failed`でinboxに上げる。
    - **jobに渡すもの**: alertとその根拠のevent、runとtaskの記録（description・acceptance・verification・paths・evidence）、receipt、`timeline RUN`（決定22）の出力、run dirのlog、runtimeがalertの時点で読んだ画面の末尾（`capture`）、idle markerの`background_tasks`、そのrunのworktreeに属するプロセスの一覧（pid、親pid、command、cwd、経過時間。runtimeが読んで渡す）、worktreeの`git status`とHEADとreceiptの`commit`、そのrunの過去の自動修正と復旧jobのverdict、許された操作の一覧（決定40）とverdictのschema。jobはmain checkoutのrepositoryの文書とsourceも読める。

40. **復旧jobのverdictは、許された操作の一覧から選び、自信が無ければinboxに上げる。**
    - verdictは`{"verdict": "repair" | "escalate", "confidence": "high" | "low", "diagnosis": "...", "actions": [...], "question": "...", "options": [...], "reason_category": "..."}`。未知のフィールドは拒否する。
    - **許された操作**（`actions`の要素。runtimeが適用の時点で前提を再検査し、1つでも前提が崩れていればverdict全体を`escalate`として扱う）:
      - `retry`: taskを`ready`に戻し、新しいrunで最初からやり直す。前提: そのrunのbranchに`base_commit`より先のcommitが無い（捨てる成果が無い）。commitがあるrunを最初からやり直すのは成果の破棄なので、jobは選べずinbox（`discard`）にする。
      - `retry_inherit`: 決定24の引き継ぐretry。前提: runのbranchにcommitがある。taskごとに1回まで。
      - `resume`: runを`needs_session`にし、決定24のresumeに回す（上限は通算で数える）。`instruction`を解消依頼に足してよい。
      - `send_instruction`: 生きているsessionに、jobが書いた指示を決定31の確認付きで1回送る（例: 待っている処理が戻らないならそれを止めてtestをやり直す）。前提: sessionが入力可能（idle markerが最後の入力より新しく、ダイアログの兆候が無い）。
      - `stop_processes`: 指定したpidのプロセスを止める（SIGTERM、猶予の後にSIGKILL）。前提: 各pidが、そのrunのworktreeをcwdに持つか、そのrunのsession wrapperの子孫で、wrapperとagent（Claude）のプロセスそのものではない。`long_background`の孤児のプロセスに使う。
      - `answer_known_dialog`: 決定29の一覧にあるダイアログに、その規則のキーを送る。前提は決定29と同じ。
      - `close_and_proceed`: 決定25の「閉じて進める」。前提は決定25と同じ。
      - `wait`: 何もせず、`recheck_after_secs`（上限1時間）の後に同じalertが続いていればもう一度jobを起動する（alertの種類ごとの3回に数える）。
    - 許されない操作: taskのcancel、commitのあるrunを最初からやり直すretry、taskの中身（description・acceptance・verification・paths）の編集、reviewを経ない着地、mainへの書き込み、push、branchやworktreeの削除、そのrunのworktreeとworkspaceの外への操作、queue DBへの直接の書き込み、既知でないダイアログへのキー。これらが要るならjobは`escalate`にする。
    - **escalate**: runtimeはinbox宛てのaskを作る。kindはalertに応じて今のものを使う（`failed` / `interrupted` / `resume_exhausted`は`decide`、`stuck_exit`は`stuck_exit`、`prompt_waiting`は`answer_prompt`、`stalled` / `long_background`は`stalled`）。questionにはjobの`diagnosis`と試した操作、`question`を載せ、optionsは今のkindのもの（`decide`は`retry` / `resume` / `cancel`、`stuck_exit`は`exit` / `wait`、`stalled`は`wait` / `intervene`）にjobの`options`を足す。人が要る理由（決定41）はjobの`reason_category`で、jobが直せなかったか自信が無いだけなら`recovery_failed`、成果の破棄を人に聞くなら`discard`、権限の判断なら`scope`。
    - **自信が無いとき**: `confidence: low`の`repair`は適用せず、`escalate`として扱う。jobの`actions`はaskの推奨の選択肢としてquestionに載せる。
    - jobの失敗（起動できない、非0終了、timeout、schemaに合わない）は、今のtriageの失敗と同じく対象を動かさず、inbox宛てのattention（`triage by hand`を`recover by hand`と読む）にする。人が要る理由は`recovery_failed`。
    - 適用した操作は`auto_repaired`（`layer: recovery`、`repair`: 操作の名前）と`recovery_finished`（verdict、confidence、適用した操作）で記録する。今の`triage_started` / `triage_finished`は、kindを変えずに復旧jobの起動・終了にも使ってよい（名前は実装taskが決める）。

41. **askの作成には「人が要る理由」の分類を必須にし、当てはまらないものはaskにしない。**
    - askに`reason_category`を持たせ、次の5つのどれかを必須にする。値を足すにはADRが要る。
      - `authentication`: ログイン・認証の切れ（決定42）
      - `cost`: コストと資源（利用上限、ディスクの空き（決定44））
      - `scope`: 受け入れ条件・範囲・ADR・goalの決定と食い違うか、それを変える判断（reviewとplan reviewの`concern`、`planner_question`、goal reviewの`ask`、権限の確認）
      - `discard`: 成果を捨てるかどうか（cancel、commitのあるrunを引き継がずにやり直すretry、goalの`abandoned`）
      - `recovery_failed`: 復旧jobが直せなかったか自信が無いと返した。jobの失敗（復旧job、goal review job、plan review job）を知らせるattentionも、inboxに見せるときはこの分類で表す
    - `dagq ask`（CLI）は`--because <category>`を必須にし、無いか一覧に無い値なら拒否する。runtimeとjobが作るaskも同じ列を書く（kindごとの既定: `approve_landing` / `approve_plan`は`scope`、`decide`は復旧jobの`reason_category`、`stuck_exit` / `answer_prompt` / `stalled`は`recovery_failed`か復旧jobの値、`blocked`はobserverの指定）。
    - **当てはまらないもの**はaskにしない。拒否されたaskの内容は、askを作ろうとした者がnote（`dagq note`）に残す。workerは自分で決められることは決めてreceiptの`summary`に書き、決められずtaskの範囲の外に出るなら`failed`のreceiptに理由を書く（復旧jobかplannerが扱う）。observerはfinding（決定18）だけを書く。runtimeは、分類できないaskを作る経路を持たない（上の既定で必ず分類する）。
    - `worker_question`（ADR-0022の決定2）はworkerが分類を付ける。受け入れ条件の解釈や範囲の変更は`scope`、成果を捨てるかは`discard`になる。
    - `ask_opened`のpayloadと`asks`の出力に`reason_category`を載せる。inboxは分類を人に見せる。既存のaskは導入のmigrationで、kindから上の既定を当てて埋める（`worker_question`と`blocked`は`scope`、決めきれないものは`recovery_failed`）。

42. **認証とコストのaskは、queueで種類ごとに1件にまとめる。**
    - `reason_category`が`authentication`か`cost`のaskは、run / taskに紐づけず、queueで種類ごと（`authentication`、利用上限の`cost`、ディスクの`cost`は`subject`で分ける）にopenなもの1件にする。openなaskがあれば新しく作らず、そのaskの`affected`（run / jobのIDの一覧）に足し、`ask_updated`を記録する。通知（`cmux notify`）は最初の1回だけ。
    - **検知**: workerの画面、復旧jobの入力、headless jobの出力に、認証切れ（ログインを求める文言、401、`Invalid API key`など）か利用上限の文言があれば、runtimeは`auth_required` / `usage_limited`を記録し、上のaskを開くか足す。文言の型は`infrastructure::claude`が持つ。
    - **待ち**: openな`authentication`のaskがある間、supervisorは新しいclaimとheadless jobの起動を控える（同じ理由でどれも失敗するため）。走っているrunのleaseは持ったまま待つ。`cost`（利用上限）も同じ。
    - **answer**: optionsは`done`（人がログインした / 上限が戻った）と`cancel_affected`（ディスクのaskは決定44の`done` / `wait`）（止まったrunを人の判断で捨てる。`discard`の判断を含むので、affectedの一覧をquestionに載せる）。`done`ならruntimeが控えを解き、止まったsessionには「続けて」の決まった文面を送り（決定31の確認付き）、失敗したjobは起動し直す。answerはruntimeが適用する（`runtime_delivers`）。

43. **goalの達成は、headlessのgoal review jobが判断する。**（goal 13のtask 115の案をこれに置き換える）
    - **起動の条件**: `goal`がopenでdraftでなく、所属taskが1件以上あり、すべて`completed`か`canceled`で、`completed`が1件以上あり、goalに属する`draft` / `submitted`のtask（runtimeのplannerの待ちや`keep_draft`で残ったものを含む）も、goalのdraftへの閉じていない`planner_question`も、goalへの閉じていないgoal reviewのaskも無い。前回のgoal reviewの後に所属taskの状態が変わっていないgoalには起動しない。
    - 同じgoalのjobは同時に1つ。queue全体では同時に1つで（plan review jobと同じ）、workerのslotには数えない。起動・終了・失敗は`goal_review_started` / `goal_review_finished` / `goal_review_failed`で記録する。envとCLIの制限はreview jobと同じ（`DAGQ_ROLE=reviewer`）。
    - **入力**: goalのtitle・description・acceptance・constraints・doc、所属taskのそれぞれのtitle・description・acceptance・状態・cancelの理由（重複先）、着地したrunのreceipt（`summary`、tests・e2e・subagent_reviewのevidence、`follow_ups`とそのdraftの行方）と着地commit、goalに付いたnote、goalを対象にするopenなfinding、前回までのgoal reviewのverdictとgapのdraft。jobはmain checkoutのrepositoryの文書とsourceを読んで、acceptanceが満たされているかを確かめる。
    - **verdict**: `{"verdict": "achieved" | "gaps" | "ask", "criteria": [{"criterion": "...", "met": true | false, "evidence": ["..."]}], "gaps": [{"title": "...", "description": "...", "criterion": "..."}], "summary": "...", "question": "...", "options": [...], "reason_category": "..."}`。`criteria`はacceptanceの項目ごとに1つ。
      - **achieved**: runtimeが1トランザクションで起動の条件を再検査し、goalを`achieved`で閉じる（`goal close --verdict achieved`と同じ検査）。`criteria`（項目ごとの根拠）を`goal_reviewed`のpayloadに記録し、closeの理由に`summary`を書く。条件が崩れていれば閉じず、次の起動を待つ。
      - **gaps**: runtimeが`gaps`の各要素を、そのgoalの`draft`のtaskとして出どころ`goal_gap`（材料は`criterion`とjobの`summary`とgoal reviewの記録）で登録する。goalは開けたまま。以後は決定16の経路（goal 29のtask 282）でruntimeのplannerが採否を決めて補い、それらのtaskが終わると再び起動の条件がそろう。同じgoalの`gaps`が3回続いたら、4回目のgoal reviewが`achieved`でなければ`ask`として扱う（補い続けるループを止める柵）。
      - **ask**: acceptanceの変更、`abandoned`での close、goalの分割など、人の判断が要るときだけ。inbox宛ての`approve_goal`のask（`reason_category`は`scope`か`discard`）を作る。optionsは`achieved`（そのまま閉じる）/ `abandoned`（閉じる）/ `gaps: <足りないもの>`（gapのdraftを登録する）/ `keep_open`。answerはruntimeが適用する（`runtime_delivers`）。`keep_open`の後は、所属taskの状態が変わるまで起動しない。
    - jobの失敗は、goalを動かさず`goal_review_failed`をinbox宛てのattention（`recovery_failed`）にする。同じgoalには、所属taskの状態が変わるか人が`goal review ID`（手で起動するコマンド。形は実装taskが決める）を打つまで起動しない。
    - plannerはgoalの完了を見てcloseする役を持たなくなる（決定16の最後の項を改める）。`goal close`のコマンドは人の判断（人が開いたplannerでの対話、askのanswer）のために残す。

44. **ビルド成果物はrunが終わったら消し、worktreeはtaskが終わったら消し、claimと着地の検証の前に空き容量を確かめる。**
    - **ビルド成果物**: runが終わった（`integrated`、`failed`、`interrupted`、`canceled`、abandon）ら、supervisorはそのrunのworktreeのビルド成果物のディレクトリ（既定は`target/`。`llvm-cov-target`は`target/`の下にある）を消す。ソースとcommit（branchと`refs/dagq/runs/<run-id>`）は残す。消す前に大きさを測り、`build_artifacts_removed`（`run_id`、`bytes`、`paths`）を記録する。resumeが同じworktreeを使うときは作り直しになる（時間は掛かるが正しさは変わらない）。
    - **worktree**: taskが`completed`か`canceled`になった時点で、そのtaskのrunのworktreeを消す（`git worktree remove`。branchとrefsは残す）。taskが`ready`（retry待ち）や`in_progress`の間は消さない。`worktree_removed`を記録する。
    - **空き容量の確認**: claimの前と、`integrate`の検証コマンドの前に、worktreeを置くファイルシステムの空きを確かめる。閾値の既定は、claimの前が「直近のrun（`build_artifacts_removed`の直近`[disk].sample_runs`件、既定20件）の`bytes`の最大値 × `[disk].claim_factor`（既定2）」、`integrate`の前が「同じ最大値 × `[disk].integrate_factor`（既定1.5）」。記録がまだ無いときは確かめない（`[disk].min_free_bytes`を設定すれば、その値を下限に使う）。値は`dagq.toml`の`[disk]`で変えられる（書式の誤りはエラー、無ければ既定。runtimeが読むのはmain checkoutの作業ファイル）。
    - **足りないとき**: claimか検証を控え（`disk_low`を記録。検証を控えたrunは`awaiting_integration`のまま着地slotを空ける）、掃除を自動で走らせる: 終わったrunのビルド成果物の消し残し、`completed` / `canceled`のtaskのworktreeの消し残し、`git worktree prune`。`auto_repaired`（`repair: disk_cleanup`、消した`bytes`）を記録して確かめ直し、足りればそのまま進める。それでも足りないときだけ、決定42のqueueで1件の`cost`のask（`subject: disk`、optionsは`done`（人が空けた）/ `wait`）をinboxに上げる。控えている間、走っているrunはそのまま続ける。

45. **`stats`は自動修正の件数をaskの集計と並べて返す。**
    - `stats`に`auto_repairs`を足す: `--since`以降の`auto_repaired`の件数を、`layer`（`runtime` / `recovery`）と`repair`ごとに返す。復旧jobのverdictの内訳（`repair` / `escalate`、`confidence`、alertの種類ごと）も返す。
    - askの集計（task 325）に`reason_category`ごとの件数を足し、`auto_repairs`と同じ期間で並べる。inboxに来るaskの件数と自動で直した件数を並べ、inboxの負担が減ったかを確かめられるようにする。集計はrun_eventsとasksから再導出し、新しい表を持たない（ADR-0040の決定5と同じ）。
    - observerは`auto_repairs`と`reason_category`ごとの件数を入力に読み、自動修正の条件の見直し（直せたはずのものが復旧jobやinboxに上がっている、自動修正の後に同じrunで失敗が続く）をfinding（決定18、種類`threshold`など）にしてよい。

実装の状況: 実装済みなのは、決定1〜3のうちADR-0044の時点のもの（triageの`retry` / `resume` / `ask`）、決定6・7・9・11〜15、決定8（引き継ぐretryの例外を除く）、決定10（ADR-0046の手段を除く）、決定16（最後の項のgoal closeを除く）、決定24のうち`needs_session`のresume・`resolved_head`・3回の上限、決定26〜28、決定29の`prompt_waiting`の検知、決定30の`SessionWatch`の促しと`stalled`のask（task 288）、決定31のうち画面の確認とEnterの送り直し（task 285。`/exit`を含む）、決定33、決定34・35のうちalertと閾値ごとの結果の集計（task 297など）。ADR-0044の時点で未実装だったもの（決定4・5のobserverの変更、決定10のADR-0046の手段、決定17のfindingに関わるもの、決定18〜23）と、本ADRで改めたもの・足したもの（決定1〜3の復旧jobとgoal review job、決定8の引き継ぐretryの例外、決定16の最後の項、決定21の`auto_repairs`の読み取り、決定24の衝突だけの試行・引き継ぐretry・adopt、決定25、決定29の既知のダイアログ、決定30・31の復旧jobへの経路、決定32の復旧jobの結末、決定34の`long_background`からの復旧job、決定35のfinding、決定36、決定37〜45）は未実装で、goal 31とgoal 34の後続taskが実装する。runtimeのtestは、task 182の型（backgroundの処理を残したidle markerのままreceiptが来ない）とtask 205の型（送った文の後に入力の印が来ない）の再現（ADR-0043から引き継ぐ）に加え、決定37の表の各行について、条件がそろうときだけ直し、欠けるときは復旧jobに回ることを確かめる。

## Alternatives

- **既知のダイアログにもキーを送らない**（ADR-0019の決定6のまま）: 画面の構造にruntimeを結合しないが、「Background work is running」とSettingsのパネルは起きるたびに人を呼び、inboxが手でキーを送っていた。キーを送るのは一覧にあるダイアログだけにし、送る条件をworktreeとreceiptで確かめ、閉じたかを確かめて2回目は送らない。権限の判断を含むダイアログ（trust、権限、auto mode）は一覧に入れない。
- **`/exit`を再送しない**（ADR-0019の決定2のまま）: ダイアログの選択肢を押す危険は避けられるが、cmuxの時間切れのように画面が空のまま届かなかった場合も人を待つ。再送は画面を読んで入力欄が空で準備できているときに限り、ダイアログの兆候があれば送らない。
- **すべてのイレギュラーを復旧jobに任せる（runtimeは直さない）**: 規則が1か所にまとまるが、決まった規則で安全が確かめられるもの（Enterの送り直し、adopt、receiptの書き直しの促し）までLLMの起動を待ち、判断がぶれる。runtimeが直せるものはruntimeで直し、状況を読む必要があるものだけをjobにする。
- **復旧jobに操作を自分で打たせる**: jobがCLIでretryやプロセスの停止を行えば適用の仕組みが要らないが、状態を変えるのはverdictを適用するruntimeだけという原則（決定2）が崩れ、前提の再検査もjobの手順に頼ることになる。jobは操作を選ぶだけにし、runtimeが適用の時点で前提を確かめる。
- **askの分類を任意にする、または自由文の理由にする**: 作る側の手間は減るが、人の判断が要らないaskがinboxに上がり続け、何が人を呼んだかを数えられない。分類を必須の閉じた集合にし、当てはまらないものは作れないようにする。
- **認証のaskをrunごとに作る**: 今の一意性（run・kind）のままで済むが、同時に止まった複数のrunが同じ1つの操作（ログイン）のために複数のaskになる。queueで1件にまとめ、止まったrunを一覧で持つ。
- **衝突だけのresumeも3回に数える**（draftのtask 158）: 暴走の心配は無いが、review pass済みの成果が衝突だけで人の手に渡る。衝突だけの試行は3回に数えず、別の上限（既定5）と、使い切ったときの引き継ぐretryで扱う。
- **plannerがgoalの達成を判断してcloseする**（今のplannerの手順、goal 13のtask 115の案）: 人が開いたplannerが居ないとgoalが閉じず、plannerごとに判断の基準がぶれる。goalの状態から起動の条件は決まった規則で分かるので、supervisorがheadlessのjobを起動し、acceptanceの項目ごとの根拠を記録する。
- **goal reviewの`gaps`でgoalを閉じ、新しいgoalにする**: goalの範囲が分かれて追いにくくなる。足りないものは同じgoalのdraftにし、決定16の経路で補う。
- **worktreeをrunの終わりにすぐ消す**: ディスクは最も空くが、resumeと引き継ぐretryが前のworktreeを使えなくなり、人が失敗の中身を確かめる場所も無くなる。ビルド成果物だけをすぐ消し、worktreeはtaskが終わってから消す。
- **固定の空き容量の閾値にする**: 設定が単純だが、repositoryごとにビルドの大きさが大きく違う。直近の実測の最大値から決め、設定で倍率と下限を変えられるようにする。
- ADR-0044・ADR-0019・ADR-0043のAlternativesは、それぞれの決定を引き継いだ本ADRでもそのまま有効で、ここには繰り返さない（observerのdraft goal、observerの直接のsubmit、一律の人の承認、`report`を先に作る、noteに構造を足す、常駐のgate session、`integrate`がresumeを待ってblockする、observerに止まったsessionを検知させる、送った文そのものを送り直す、など）。置き換えたADRの本文は書き換えないので、その理由はそれぞれのADRで読める。

## Consequences

- inboxに来るのは、決定41の分類を持つaskとattentionだけになる。今のinbox宛てのask（`decide`、`stuck_exit`、`answer_prompt`、`stalled`）は、復旧jobが直せなかったものだけになる。件数の変化は`stats`の`auto_repairs`とaskの`reason_category`の集計で確かめる。
- runtimeが画面にキーを送る場面が増える（既知のダイアログ、`/exit`の再送）。Claude CodeのUIが変わると一覧の型が外れるが、外れたときは何も送らずに復旧jobに回るので、誤操作ではなく人の手間に戻る。
- 復旧jobがプロセスを止められるようになる。対象はそのrunのworktreeとsessionの子孫に限り、runtimeが適用の時点で確かめるが、同じworktreeを使う別の人の作業（人がworktreeで開いたshellなど）も対象になりうる。
- 決定24の引き継ぐretryと決定40の`retry_inherit`で、review pass済みの成果が衝突で捨てられなくなる。新しいrunは前のrunのcommitを載せ直すので、reviewを再び受ける。
- 決定42の控えの間、queue全体が止まる。認証と利用上限は同じ理由でどのrunも失敗するので、失敗を積み上げないための控えで、inboxに1件のaskで知らせる。
- goalはgoal review jobが閉じる。plannerの`goal close`の手順と、`dagq-planner` skillの「全taskの完了を見たらclose」は、実装の後に書き換える。goal 13のtask 115は、本ADRの決定43を実装するtaskに置き換える（planner（人）がtask 115をcancelする）。
- ディスクの掃除で、終わったrunのビルド成果物とtaskの終わったworktreeが消える。人が失敗したrunの`target/`を後から確かめることはできなくなる（log・receipt・run dirは残る）。
- schemaが変わる: askの`reason_category`と`affected`と一意性（`authentication` / `cost`はqueueで種類ごと）、kindの`approve_goal`、runの`inherit_from_run`、goal reviewの記録。run_eventsに`auto_repaired`、`exit_retried`、`recovery_requested`、`recovery_finished`、`auth_required`、`usage_limited`、`ask_updated`、`goal_review_started` / `goal_review_finished` / `goal_review_failed` / `goal_reviewed`、`build_artifacts_removed`、`worktree_removed`、`disk_low`が加わる（名前は実装taskが決めてよい）。`dagq.toml`に`[resume]`・`[exit]`・`[disk]`が加わる。
- `dagq ask`に`--because`が必須になり、pluginのskill（`dagq-inbox`、`dagq-recover`、`dagq-planner`、`dagq`）とAGENTS.mdのworker・inbox・plannerの節（「判断が要るときは`dagq ask`」「plannerがgoalをcloseする」「triageが失敗したrunは人が判断する」）は、実装に合わせてgoal 34のtaskで新しい分担に書き換える。
- [ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定1のaskの列・kind・一意性は、本ADRが足した`reason_category`・`affected`・`approve_goal`とqueueで1件の一意性を含めて読む。一覧を今の値で書き直すことは、ADRの棚卸しでADR-0022を置き換える統合ADRに任せる。
- ADR-0019は[ADRの棚卸し](../plans/adr-inventory.md)の組D（0008、0019、0027）に入っていた。組Dの統合ADRは、0019の代わりに本ADRの決定24〜29を参照する（0019は本ADRが置き換え済み）。
- ADR-0019・ADR-0043・ADR-0044の本文は書き換えず、`superseded`にして本ADRを指す。それらの「決定N」を参照する文書とsource、ADR-0045・ADR-0046などの本文の参照は、Contextの対応表で本ADRの決定として読める。
- [overview](../design/overview.md)、[supervisor-lifecycle](../design/supervisor-lifecycle.md)、[domain-model](../design/domain-model.md)、[persistence](../design/persistence.md)、[plugin-integration](../design/plugin-integration.md)は各実装taskで更新する。
