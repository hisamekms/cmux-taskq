---
id: adr-0041
type: adr
title: 役割を5つにし、plannerをproposalごとのオンデマンドのworkspaceにし、taskにsubmitted状態を足し、supervisorが起動するplan review jobだけがreadyにし、follow_upのdraftもruntimeが立てるplannerのproposalとして同じgateを通す
status: accepted
created: 2026-09-25
updated: 2026-09-25
accepted_on: 2026-09-25
supersedes:
  - adr-0024
  - adr-0037
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - planner
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
  - adr-0022
  - adr-0024
  - adr-0026
  - adr-0027
  - adr-0028
  - adr-0029
  - adr-0035
  - adr-0036
  - adr-0037
  - adr-0038
  - adr-0040
  - design-overview
  - design-supervisor-lifecycle
  - design-domain-model
  - design-plugin-integration
  - design-persistence
---

# ADR-0041: 役割を5つにし、plannerをproposalごとのオンデマンドのworkspaceにし、taskにsubmitted状態を足し、supervisorが起動するplan review jobだけがreadyにし、follow_upのdraftもruntimeが立てるplannerのproposalとして同じgateを通す

## Context

[ADR-0024](0024-retire-maintainer-into-jobs-and-observer.md)（goal 12、2026-09-23）は常駐のmaintainerを退役させ、役割をsupervisor / worker / planner / inbox / observerの5つにし、run 1件に対する判断（review、triage）と継続的改善（observer）をheadlessのjobにした。常駐のsessionはsupervisor（in-cmux mode）・inbox・plannerの3つで、`up`がそれを開く。[ADR-0037](0037-follow-up-triage-job-decides-follow-up-drafts.md)（goal 22、2026-09-24）は、`integrate`が作るfollow_upのdraft taskの採否を、supervisorが起動するheadlessのfollow-up triage jobに決めさせた。ADR-0037の実装はgoal 22のtask 205が進めている。

その後、常駐のplannerが詰まりになった。plannerは人との対話による計画に加えて、計画の交通整理を抱えている: draftの棚卸し、重複や実装済みの検出、ADRとの矛盾、ADR番号の割り当て、依存の付け替え、他のtaskの退避。これが人を待たせる。2026-09-25のdraftの棚卸しでは、この作業に1 sessionの大半を使った。plannerは1つしか開けないので、AIが考えている間に人が別の計画を進めることもできない。登録したtaskは`ready`を打てばそのままclaimされるので、他の計画や既にreadyのtaskとの整合を見る場所も無い。

2026-09-25のplannerとの対話で、ユーザーは次を決めた（goal 29のconstraints）。

- 計画（goalとtaskの束 = proposal）は、人が開くオンデマンドのplannerと、runtimeが立てるplanner（follow_up用など）が作る。人はplannerのworkspaceを複数同時に開ける。常駐のplannerはやめ、`up`はplannerを開かない。inboxが唯一の常駐sessionになる。
- taskに`submitted`（plan review待ち）を足す。plannerとjobはsubmitまでを行い、`ready`にするのはsupervisorが起動するplan review jobだけにする。人は明示したbypassで飛ばせる。
- plan reviewは、機械的な検査をCLIの決まった規則で、意味の検査をLLMで行う。verdictは`pass` / `revise` / `concern`で、jobが自分でしてよいのは、readyにする、依存を足す、優先度を下げる、明らかな重複をcancelする、まで。
- 差し戻し先のplannerが閉じていれば、runtimeが新しいplannerを立てる。人が開いたplannerとruntimeが立てたplannerで、人の判断が要るものの届け先を変える。
- goal 22のfollow-up triage job（ADR-0037）は、follow_upのdraft 1件ごとにruntimeが立てるplannerに置き換える。goal 22のtask 205（実行中）は着地させ、その後にgoal 29のtaskで直す。
- 人に届くものはすべてinboxにし、plannerに返すのはそのplanner自身のproposalへのreviseだけにする。

[ADR-0035](0035-adr-is-superseded-whole-with-dates-and-banner.md)は決定を1つでも変えるADRに、古いADRのまだ生きている決定を書き直して引き継ぎ、古いADRを丸ごと置き換えることを求める。本ADRはADR-0024とADR-0037を丸ごと置き換える。決定1〜6はADR-0024の決定1〜6を同じ番号で書き直したもの（決定1・2・6は上の人の決定で改め、決定3〜5もreadyの権限・submit・proposalに合わせて一部を改めた。改めた箇所は各決定に書く）で、ADR-0024の「決定N」を参照している文書は本ADRの決定Nと読める。決定7〜17は新しい決定で、決定16がADR-0037の今も有効な決定（対象の条件、深さとそのmigration、自動採用の上限、askのoptions、出自の記録）を引き継ぐ。goal 28で決めたtaskの優先度の定義（[ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)の決定4）は変えない。

## Decision

**原則。** 常駐のsessionは、人に届くものの窓口（inbox）とループそのもの（supervisor）だけにする。人と対話する計画は、計画ごとにオンデマンドのworkspace（planner）を開いて行う。run 1件と計画1件に対する判断はheadlessのjobにし、jobはqueueを読むだけでverdictを返し、状態を変えるのはverdictとanswerを適用するruntimeだけにする。適用は1トランザクションにする。継続的改善は定期起動のjobにし、状態を変える権限を持たせない。repository固有の規則（AGENTS.mdのverificationの規則、ADR番号など）はruntimeに埋め込まず、jobのpromptがrepositoryの文書から読む。人に届くものはすべてinboxに届ける。run_eventsのkindは追加だけで、既存のkind名とpayloadは変えない。schemaを変えるtaskは`user_version`を上げて`migrations/`に追加する。以下の17点を決める。

1. **役割はsupervisor / worker / planner / inbox / observerの5つにする。maintainerは退役したまま。**
   - **supervisor**: runtimeの`supervise`プロセス。claim、worker起動、validating、resume、jobの起動（review、triage、plan review）、verdictとanswerの適用、着地、runtimeが立てるplannerの起動、後始末を行う。
   - **worker**: runごとにsupervisorが開くオンデマンドのworkspaceで、worktreeで作業するClaude session。
   - **planner**: proposal（決定7）ごとのオンデマンドのworkspace。人が`dagq plan`で開くものと、runtimeが立てるもの（決定12、14、16）がある。goalとtaskを書き、proposalをsubmitする（決定8）。`ready`にはしない。
   - **inbox**: 唯一の常駐session。openなaskを人に見せてanswerを書き戻し、それ以外のattentionを人に知らせる。自分では判断しない（[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定4）。
   - **observer**: supervisorのtimerで定期起動するheadlessのjob（決定4）。
   - 検査はsupervisorが起動するheadlessのplan review jobが行う（決定10、11）。plan review jobは同時に1つで、runのreview jobと対になる。
   - maintainerという役割、`[<repo>]dagq maintainer`のworkspace、`DAGQ_ROLE=maintainer`は無い。[ADR-0010](0010-maintainer-and-resident-supervisor.md)以降のADRに残るmaintainerの記述は書き換えず、[overview](../design/overview.md)の用語集で「review job / triage job / observer / inboxのいずれか」に読み替える（レビューと着地はreview job、失敗runの扱いはtriage job、継続的な監視はobserver、人への相談とanswerの実行はinbox。`up` / `down`などの操作は人がinboxかplannerのsessionから打つ）。
2. **jobは、cmux workspaceを持たないheadlessのClaude実行にする。**
   - 起動は`AgentProvider`のheadless実行のport（`headless_command`。Claudeでは`claude -p`）を使う。promptに入力のpathと出力のJSON schemaを渡し、stdoutのJSONを読む。未知のフィールドは拒否する。
   - jobは次の4種類。
     - **review job**: `awaiting_integration`のrunをレビューし、`pass` / `revise` / `concern`を返す（[ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)の決定2、[ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)）。
     - **triage job**: `failed` / `interrupted`のrunのreceipt・log・run_eventsを読み、`retry` / `resume` / `ask`のverdictを返す（決定3）。
     - **plan review job**: submitされたproposalを検査し、`pass` / `revise` / `concern`を返す（決定10、11）。
     - **observer job**: supervisorのtimerで定期起動し、noteとdraft goalとaskを残す（決定4）。
   - review / triage / plan reviewのjobのenvは`DAGQ_ROLE=reviewer`と`DAGQ_QUEUE`で、CLIは読むコマンドだけを許す。cwdはrepositoryのmain checkoutで、jobはrepositoryの文書とsourceを読める。
   - jobの起動・終了・失敗はrun_eventsに記録する（`review_started` / `review_finished` / `review_failed`に倣う）。headless実行が失敗したとき（起動できない、非0終了、timeout、stdoutがschemaに合わない）は、対象を動かさず、inbox宛てのattentionかaskにする。timeoutは`AgentProvider::review_timeout`（既定600秒）で、ループはプロセスを`try_wait`で見るだけで止まらない。
3. **triage jobのverdictでruntimeが動き、安全に自動化できる後始末はruntimeが行う。**（ADR-0024の決定3を引き継ぐ。retryをreadyの権限の例外と明記したこと以外は実装済み）
   - supervisorは`failed`になったrunと、下の自動`recover`で`interrupted`になったrunごとにtriage jobを1回起動する。verdictは`{"verdict": "retry" | "resume" | "ask", "reasons": [...], "summary": "..."}`に固定し、payloadに記録する。
     - **retry**: runtimeがtaskを`in_progress`から`ready`に戻す。次のclaimで新しいrunが作られる。taskの中身は変わらないので、plan reviewにはかけない（決定8のreadyの権限の例外）。
     - **resume**: runtimeがrunを`failed` / `interrupted`から`needs_session`にし、自動resume（[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)の決定1）で同じsessionに続けさせる。試行3回の上限はrunごとの通算で数え、triageからのresumeも含める。上限を超えたrunは再びtriageに回らず、`decide`のaskになる。
     - **ask**: `kind: decide`のaskをinbox宛てに作り、`reasons`と`summary`をquestionに載せて待つ。answerに従う操作は人がinboxかplannerのsessionから打つ（inboxは自分では判断しない）。
   - triage済みのrunのworkspaceはruntimeが閉じる。
   - wrapperが死んでleaseの無いrunは、supervisorが`recover`まで自動で行う。`recover`の結果は`interrupted`で、taskを`ready`にはせず、triage jobに回す。[ADR-0012](0012-adopt-stale-lease-of-live-wrapper.md)の「wrapperが死んだrunの`recover`は人の判断で手動」は、`recover`の実行だけ自動に改めたまま。[ADR-0003](0003-supervisor-owns-lifecycle.md)・[ADR-0007](0007-run-level-leases-parallel-execution.md)の「孤児runは自動再実行しない」は、再実行をtriageのverdictに委ねることで維持する。
4. **observerはsupervisorのtimerで定期起動するjobにし、状態を変えない。**（ADR-0024の決定4を引き継ぐ。拒否するコマンドに`submit`を足したこと以外は実装済み）
   - 起動間隔は既定1時間で、`supervise --observe-interval`で変える。0なら起動しない。supervisorが居ないときは動かない。
   - 入力は`stats --since <前回の起動のcursor>`、直近のnote、openなask。
   - 出力は次の3つだけ。
     - **note**: run_eventsのkind `observation`として、task / run / goalのいずれかに紐づけて記録する。別の表は作らない。
     - **ask**: 閾値超えの詰まりを`kind: blocked`のaskとしてinbox宛てに上げる。optionsにobserverの見立てを載せる。runにもtaskにも紐づかない閾値はrun_idもtask_idも持たない`blocked`をopenなもの1件にまとめる。
     - **draftのgoal**: 改善の提案を決定5のdraft goalとして登録する。根拠にしたnoteのidをdescriptionに書く。
   - runtimeは`DAGQ_ROLE=observer`のsessionからは許可したコマンドだけを受ける: 読み取り、noteの記録、`ask`、`goal add --draft`とそのdraft goalへの`add`（draftのtask）。それ以外（`ready`、`submit`、`integrate`、`recover`、cancel、`goal ready`、`goal close`、`answer`、通常の`add`など）は拒否する。observerは個々の詰まりを解消せず、自分のdraftをplan reviewに出すこともできない。
5. **goalにdraft状態を持たせる。**（ADR-0024の決定5を引き継ぐ。plan reviewのpassでgoalのdraftを外すこと以外は実装済み）
   - `goal add --draft`でdraftのgoalを作り、`goal ready ID`でdraftを外す。draftのgoalに属するtaskは`candidates`に出ず、supervisorはclaimしない。
   - observerが登録したdraftのgoalは、人が開いたplannerで人と採否を決める。採るなら、plannerがそのgoalとtaskを自分のproposalにしてsubmitし（決定7、8）、plan reviewがpassにしたときにgoalのdraftも外れる。`goal ready`は引き続き使えるが、goalのdraftを外すだけで、所属taskを`ready`にはしない（taskは決定8のとおりplan reviewかbypassでだけ`ready`になる）。採らないなら`goal close`（`abandoned`）にする。
   - [ADR-0009](0009-goal-groups-tasks.md)の「goalは状態機械を持たない」は、draftか否かの1点に限って改めたまま。進捗は従来どおりtaskの状態から導く。
6. **`up`はsupervisorとinboxだけを開き、plannerは`dagq plan`で開く。**
   - `up`が作るworkspaceはsupervisor（in-cmux mode）とinboxだけになる。常駐のplannerはやめ、`up`は`session_workspaces`のplannerの行を忘れる（maintainerの行と同じ扱い。残ったworkspaceは人が閉じる）。
   - 人は`dagq plan`で人が開くplannerのworkspaceを開く。何度打っても新しいworkspaceを開き、複数同時に開ける。workspaceは[ADR-0026](0026-identify-workspaces-by-uuid-env-and-queue-group.md)のとおりUUIDで識別し、`--env`に`DAGQ_ROLE=planner`と`DAGQ_QUEUE`を持たせ、queueのworkspace groupに入れる。titleは表示専用で、[ADR-0028](0028-workspace-titles-are-repo-and-role.md)の`[<repo>]planner`にproposalを見分ける識別子を足す（形は実装taskが決める）。
   - `up` / `down` / 固定バイナリの更新は、人がinboxかplannerのsessionから打つ。
   - in-cmux modeのsupervisorが止まったときは、inboxの`watch`が`supervisor_stopped`で拾って人に知らせ、人が`up`を打つ。`watch --role inbox`は`ask_opened`・`ask_answered`・`supervisor_stopped`とそれ以外のattentionを受ける。in-cmux modeに自動再起動が無いこと（[ADR-0011](0011-cmux-socket-password-and-in-cmux-fallback.md)）は変えない。
7. **proposalを、plan reviewと差し戻しの単位にする。**
   - proposalは、goal（0個以上）とtaskの束に、持ち主のplanner（workspaceのUUIDと、人が開いたかruntimeが立てたか）を結び付けたもの。plan reviewはproposal単位で検査し、reviseはproposalの持ち主に返す。
   - 1つのtaskは同時に1つのproposalにだけ属する。plannerは`add`や`goal add`で書いたgoal / taskを自分のproposalに入れる。既存のdraftのtask（observerのdraft goalのtask、保留・退避したtask）も、plannerが自分のproposalに入れてsubmitできる。
   - 持ち主のplannerが閉じた後に差し戻すときは、runtimeが立てた新しいplannerがproposalの持ち主になる（決定12）。
8. **taskの状態に`submitted`を足し、`ready`にするのはplan review jobだけにする。**
   - `submitted`はplan review待ち。plannerが`dagq submit`でproposalを出すと、proposalのtaskが`draft`から`submitted`になる。supervisorは`submitted`のtaskをclaimしない。
   - `draft`は「まだ出していない」の意味だけになる。follow_upの提案、保留、退避、observerの提案などは、submitしない限りreadyにならない。
   - `ready`にするのは、plan review jobのverdictを適用するruntime（決定11）と、`concern`のaskに人が`ready`と答えたときのruntimeだけ。plannerとjobはsubmitまでを行う。
   - 例外は2つ。人が明示したbypass（`ready --bypass-review`。bypassしたことをeventに記録する）と、triageの`retry`（決定3。中身の変わらない同じtaskを戻すだけ）。bypassの無い`ready`は、どのroleから打たれても拒否する。
   - `goal close --verdict achieved`は所属taskに`completed` / `canceled`以外があれば拒否することを変えない。`submitted`のtaskもcloseを止める。
9. **taskの中身を編集できるのは`draft`と`submitted`のあいだだけにする。**
   - `draft`と`submitted`のtaskは、description・acceptance・`verification_commands`・`paths`・evidence・contextを編集できる。`submitted`のtaskを編集したら、そのproposalはplan reviewを受け直す（検査中なら、そのverdictは適用しない）。
   - `ready`のtaskは編集しない。変える必要があるときは、決定14の手順で`submitted`に戻してから直す。`in_progress`以降のtaskは編集しない。
10. **plan reviewの検査を、CLIの決まった規則とLLMに分ける。**
    - **機械的な検査**はCLI（`dagq lint`など）が決まった規則で判定する: 依存の循環、完了・cancel済みのtaskへの依存、存在しないtask / goalへの依存、宣言`paths`のglobの妥当性（[ADR-0029](0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)の`validate_path_globs`）、evidenceの値、宣言`paths`とverification・evidenceの整合、titleとacceptanceが空でないこと。plannerはsubmitの前に同じ検査を自分で打てる。`submit`もこの検査を行い、通らないproposalはsubmitできない。
    - **意味の検査**はplan review jobのLLMが行う: 他のtaskとの重複、既に実装済み、ADR・goalのconstraintsとの矛盾、同じファイルを触るtaskの間の依存の提案、他のsubmittedのproposalとの食い違い（決定15）、既にreadyのtaskとの食い違い（決定14）。
    - repository固有の規則（AGENTS.mdのverificationの規則、ADR番号の割り当てなど）はruntimeに埋め込まない。plan review jobのpromptが、repositoryのAGENTS.md / CLAUDE.md、`docs/adr/`、goalのdocを読んで反映せよと指示する。
    - promptには、proposalのgoalとtask（description・acceptance・verification・paths・evidence・context・依存・優先度）、`lint`の結果、他のsubmittedのproposalと既にreadyのtaskの一覧、関係するgoalのdescription・acceptance・constraints、verdictのschemaを渡す。
11. **plan reviewのverdictは`pass` / `revise` / `concern`にし、jobが自分でしてよい修正を4つに限る。**
    - verdictは`{"verdict": "pass" | "revise" | "concern", "reasons": [...], "summary": "...", "actions": [...]}`。`actions`はjobが自分でしてよい修正だけを持つ: readyにしてよいこと（passそのもの）、依存を足す、優先度を下げる、明らかな重複をcancelする（重複先のtaskを示す）。これら以外の修正（description・acceptance・verification・pathsの書き換え、taskの分割、依存の削除、優先度を上げる）はjobにさせず、reviseでplannerに直させる。フィールドの正確な形は実装taskが決める。
    - **pass**: runtimeが1トランザクションで`actions`を適用し、proposalのtaskを`submitted`から`ready`にし、proposalのdraftのgoalのdraftを外す。
    - **revise**: runtimeがproposalのtaskを`draft`に戻し、`reasons`をproposalの持ち主のplannerに返す（決定12）。reviseの回数には上限を置き、同じproposalのreviseが2回（runのreviewの`MAX_REVISE_ATTEMPTS`と同じ）を超えたら、3回目のplan reviewがpassでなければconcernとして扱う。
    - **concern**: 人の判断が要るもの。`approve_plan`のask（新しいkind）をinbox宛てに作り、`reasons`と`summary`をquestionに載せて待つ。optionsは`ready`（そのまま通す）/ `send_back`（人の理由を付けてplannerに差し戻す）/ `cancel`（proposalのtaskをcancelする）。answerはsupervisorが適用する（runの`approve_landing`と同じ形）。`ready`・`send_back`・`cancel`のanswerは`runtime_delivers`でattentionにしない（`send_back`は決定12のreviseと同じくplannerに理由を返す）。
    - jobが自分でcancelするのは明らかな重複だけにする。重複か疑わしいもの、既に実装済みに見えるもの、ADR・constraintsと矛盾するものはconcernにする。
    - plan review jobはqueue全体で同時に1つ。workerのslotには数えない。
12. **reviseはproposalの持ち主のplannerに返し、閉じていればruntimeが新しいplannerを立てる。**
    - 持ち主のplannerのworkspaceが生きていれば、runtimeは`reasons`と直す手順（直してsubmitし直す、自分の判断で直せないものは下の規則で人に聞く）を`WorkspaceBackend::send_text`で送る。
    - 閉じていれば、runtimeが新しいplannerのworkspaceを立て、proposalと`reasons`を初期promptに載せて続けさせる（runのresumeと同じ考え方）。新しいplannerは「runtimeが立てたplanner」で、proposalの持ち主になる。
    - runtimeが立てるplannerの同時の数には上限を置く。workerの`--parallel`と同じく`supervise`のoptionで設定でき（既定1）、workerのslotとは別に数える。人が開いたplannerは数えない。上限に達していれば、空くまで立てるのを待つ。
13. **人が開いたplannerとruntimeが立てたplannerで、人の判断の届け先を変える。**
    - **人が開いたplanner**: 計画の意図が変わる修正（受け入れ条件、範囲、goalとの関係）は、そのworkspaceで人に聞く。reviseを送ってから一定時間、plannerが再submitもidleからの応答もしなければ、inboxにもattentionで知らせる。
    - **runtimeが立てたplanner**（人がいない）: 人の判断が要るものは`planner_question`のask（新しいkind）をinbox宛てに作る。answerはそのplannerのworkspaceにruntimeが`answer to ask <id>: ...`として送り、plannerが適用する（`worker_question`と同じ形。[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定2）。
    - plannerのworkspaceは、期限を過ぎても閉じない。inboxに知らせるだけにする。期限は`supervise`のoptionで設定でき、既定はrunの`resume_timeout`と同じ1時間にする。
    - runtimeが立てたplannerは、proposalをsubmitしたか、cancelしたか、follow_upのdraftを`keep_draft`で残したら終わる。`planner_question`のanswerを待つ間は開いたままにする（answerをそのworkspaceに送るため）。終わったworkspaceはruntimeが閉じる（workerの`/exit`とcloseと同じ手順）。answerを送る時点やreviseを返す時点でworkspaceが閉じていれば、決定12のとおり新しいplannerを立てて渡す。
14. **readyのtaskを変える必要があるときは、`submitted`に戻して新しいplannerに直させる。**
    - plan reviewが、既に`ready`のtaskを変える必要がある（新しいproposalと食い違う、前提が崩れた）と判断したら、verdictにそのtaskと理由を書く。runtimeはそのtaskを`ready`から`submitted`に戻してclaimされないようにし、そのtaskだけの新しいproposalを作って、runtimeが立てたplannerに理由を渡して直させる。直したproposalは再びplan reviewを通る。
    - `in_progress`のtaskは直さない。plan reviewは、着地の後に直すtask（そのtaskに依存する）を足すようproposalの持ち主にreviseで求める。
15. **同時に出されたproposalは出された順に1件ずつ検査し、interruptだけ先にする。**
    - plan review jobはsubmitされた順（submitの時刻の古い順）に1件ずつ起動する。検査のときは、まだreadyになっていない他のproposal（submitted、reviseで持ち主が直しているもの）と、既にreadyのtaskとも照らし合わせる。
    - 食い違えば後から出した方を差し戻す。検査中のproposalが先に出された他のproposalと食い違えば、検査中のものをreviseにする。後から出されたものと食い違うだけなら検査中のものは通し、後から出されたものはその検査のときにreadyのtaskとの食い違いとして差し戻される。
    - interruptの優先度（[ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)の決定4）を持つtaskを含むproposalは、他より先に検査する。gateは飛ばさない。
16. **follow_upのdraftは、1件ごとにruntimeが立てるplannerのproposalにする。**（ADR-0037のfollow-up triage jobを置き換える）
    - `integrate`がreceiptの`follow_ups`からdraft taskを登録し、`follow_up_registered`を記録することと、その形（titleとdescriptionだけ）は変えない（[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)の決定4）。
    - **対象**は、statusが`draft`で、そのtaskを`task_id`に持つ`follow_up_registered`があり、まだfollow_upのplannerが扱っていないdraft。人が`add`で作ったdraftとobserverのdraft goalのtaskは対象にならない。導入前から残っているdraftも対象にする。対象の順はtaskのIDの昇順。
    - supervisorは対象のdraft 1件ごとにplannerを1つ立てる（決定12の上限の中で）。立てることは`BEGIN IMMEDIATE`の中で対象の条件を再検査してから記録するので、2つのsupervisorが同じdraftにplannerを立てることはない。plannerが3つのどれも選ばずに終わった（workspaceが閉じた、sessionが死んだ）draftは、次のplannerを立てて続けさせる。立てるのはdraft 1件あたり3回までで、超えたらinbox宛てのattentionにする。初期promptには、draftのtitle・description、元のtask（title・description・acceptance・verification・paths・evidence）、元のrunのreceiptの`summary`と`follow_ups`、goal（title・description・acceptance・constraints・doc）と同じgoalの他のtaskを載せる。
    - plannerは3つから選ぶ。
      - **採用**: acceptance・verification・paths・evidence・依存を補い、draftを自分のproposalとしてsubmitする。plan reviewを通ってreadyになる。出自として、taskの`context`の冒頭に「follow-up draft（task <元task-id> の run <run-id> の receipt が提案）」と書き、`follow_up_adopted`（`task_id`、`source_task_id`、`source_run_id`、`by: "planner" | "person"`、`ask_id`、`depth`）を記録する。
      - **不採用**: draftをcancelする。
      - **判断できない**: `planner_question`のask（決定13）をinbox宛てに作る。optionsは`adopt` / `cancel` / `keep_draft`。answerはplannerのworkspaceに送られ、plannerが適用する。`keep_draft`ならdraftのまま残し、人が開いたplannerが後で扱う。
    - **自動で採用しない上限**（ADR-0037の決定6を引き継ぐ）: 次のdraftは、runtimeが立てたplannerからは、人の判断（`planner_question`のanswerの`adopt`）を経ずにsubmitできない。CLIの`submit`がこれを拒否する。人が開いたplannerからのsubmitは人の判断を経たものとして扱い、拒否しない（`keep_draft`で残したdraftもこの経路で出せる）。
      - 閉じたgoalのfollow_up（draftのgoalがnullか、そのgoalが閉じている）
      - 深さ2以上のfollow_up
    - **深さ**は「人の判断を経ずに続いたfollow_upの段数」で、`tasks`の列`follow_up_depth`に持つ。`add`で人やplannerが登録したtaskは0。`integrate`がtask Pのrunの`follow_ups`からdraftを登録するとき、draftの深さはPの深さ + 1。follow_upのplannerが人に聞かずにsubmitしたtaskは深さをそのまま持つ。人がanswerで`adopt`を選んだtask、人が開いたplannerがsubmitしたtask、bypassで`ready`にしたtaskは0に戻す。migrationは既存のtaskを0にし、`follow_up_registered`の`task_id`が指すtaskのうちstatusがまだ`draft`のものを1にする。acceptanceが空のtaskは決定10の`lint`がsubmitを拒否する。
    - goal 22のfollow-up triage job、`follow_up`のask、`task_leases`はこの決定で要らなくなる。task 205が入れた実装は、goal 29のtaskで決定16に置き換える。
    - goal closeの前にplannerが確かめるもの: 所属goalのfollow_upのdraftのうち、plannerの待ち、openな`planner_question`、`keep_draft`で残ったもの。`keep_draft`は人が開いたplannerで人と決めて片付ける。
17. **人に届くものはすべてinboxにし、plannerに返すのはそのplanner自身のproposalへのreviseと、そのplannerが作ったaskのanswerだけにする。**
    - inboxに届くもの: `approve_plan`・`planner_question`・他のすべてのask、`plan_review_failed`、人が開いたplannerの期限切れ、それ以外のすべてのattention。
    - **`plan_review_failed`**: plan review jobが失敗したら（決定2の失敗）、proposalを`submitted`のまま動かさず、`plan_review_failed`をinbox宛てのattentionにする。supervisorは同じproposalを自動ではもう一度かけない。人の指示で、inboxがbypassでreadyにするか、plannerに直させて再submitさせる。
    - plannerはruntimeからaskも報告も受けない。受けるのは、自分のproposalへのreviseと、自分が作った`planner_question`のanswerだけ。

実装はgoal 29の後続taskが行う。決定1〜6のうちADR-0024から変えずに引き継いだ部分は実装済みで、それ以外（各決定に書いた変更と決定7〜17）は本ADRの時点では未実装。

## Alternatives

- **常駐のgate session（検査役のClaude session）を置く**: 人が見ている前で検査が進むが、ADR-0024で常駐のmaintainerをやめたのと同じ理由で採らない。検査はproposal 1件に対する処理で、常駐させるとcontextとcompactionを抱え、寝ている間はproposalが止まる。runのreview jobと同じくheadlessのjobにすれば、supervisorの起動が排他を兼ねる。
- **gateが全部を自分で直す**: plannerとの往復が減るが、description・acceptance・pathsの書き換えは計画の意図に触れ、誰が決めたかが分からなくなる。gateが書き換えたtaskを検査するものも無い。jobに許すのは、意図を変えずに安全側に倒れる4つ（ready、依存を足す、優先度を下げる、明らかな重複のcancel）に限る。
- **plannerを常駐1つのままにする**: 実装は小さいが、計画の交通整理が1つのsessionに集まり、人がAIを待つ詰まりが解けない。AIが考えている間に人が別の計画を進めることもできない。
- **draftのまま印で区別する（`submitted`を状態にしない）**: schemaの変更は小さいが、「出したがまだ検査していない」と「まだ出していない」がどちらも`draft`になり、claimしない理由とreadyの権限の判定が印の組み合わせになる。follow_up・保留・退避のdraftと取り違えやすい。状態にすれば、claimされないこと、編集できること、plan reviewの対象であることが状態から決まる。
- **interruptの優先度を持つtaskにgateを飛ばさせる**: 急ぎのtaskは早く流れるが、急ぎで書いたtaskほど重複や矛盾を含みやすく、`interrupt`は他のreadyより必ず先に走るので、誤りの影響も大きい。検査の順を先にするだけにする。人が本当に飛ばしたいときは`ready --bypass-review`がある。
- **follow-up triage job（ADR-0037）を残す**: 実装が進んでいるが、follow_upだけ別のgateを通り、plan reviewが見る他のproposalとの整合を見ない。採否の判断は、補ったtaskを書くplannerに持たせ、検査はplan reviewに一本化する。
- **follow_upの採否を`integrate`の中で同期に決める、workerがreceiptに完全なtaskを書く、observerに決めさせる**（ADR-0037から引き継ぐ）: `integrate`の単一の着地slotをClaudeの待ちで塞ぎ、失敗が着地の失敗と混ざる。workerは自分のtaskの範囲しか見ておらず、着地後に既に済んだかを判断できない。observerは状態を変えない柵（決定4）の上に成り立ち、draft 1件ごとの判断に向かない。
- **maintainerを常駐で残す、複数にする**（ADR-0024から引き継ぐ）: run 1件に対する処理で常駐を置く理由が無く、複数にするとattentionのclaimの仕組みが要る。runごとのjobならsupervisorの起動が排他を兼ねる。
- **observerを常駐sessionにする**（ADR-0024から引き継ぐ）: observerは詰まりを解消しないので即座に反応する必要が無く、timerの定期起動のjobで足りる。ADR-0024が退けた「noteを`docs/journal/`に書く」は、`docs/journal/`が[ADR-0036](0036-delete-frozen-work-records.md)で削除されたので選択肢から外れた。
- **follow_upを自動採用する上限を設けない**（ADR-0037から引き継ぐ）: 人の手はさらに減るが、自動で採用したtaskが次のfollow_upを生み、人の判断を経ずにgoalが膨らみ続けうる。閉じたgoalの続きは新しいgoalにすべきか人が決めることで（ADR-0009）、acceptanceの無いtaskはreviewの基準が無い。

## Consequences

- 人は複数のplannerを同時に開いて計画を進められ、計画の交通整理（重複、実装済み、矛盾、依存）はplan review jobに移る。人に届くのは、plannerで聞かれる意図の変更と、inboxの`approve_plan` / `planner_question` / `plan_review_failed`だけになる。
- `ready`は一段遅くなる。submitからreadyまでplan review 1回分（最大`review_timeout`）かかり、同時に1つなのでproposalが多いと順番を待つ。急ぎは`interrupt`で順を先にし、それでも待てなければ人が`ready --bypass-review`を打つ。
- schemaが変わる: taskの状態`submitted`、proposal（持ち主のworkspaceと種別）とtaskの所属、`tasks.follow_up_depth`、askのkind `approve_plan`と`planner_question`。run_eventsのkindにplan reviewの起動・終了・失敗（`plan_review_failed`はinbox宛てのattention）、submit、bypass、runtimeが立てたplannerの起動・終了が加わる。`candidates`とclaimは`submitted`を除く。
- `up`はplannerを開かなくなり、`dagq plan`が加わる。inboxとplannerの初期prompt、SessionStart hook、plugin skill（`dagq-planner`、`dagq`、`dagq-inbox`、`dagq-recover`）は新しい流れに書き換わる。AGENTS.mdの役割の節は、実装が入った後にskillのtaskで書き換える。
- goal 22のfollow-up triage job（ADR-0037）の実装（task 205）は、着地の後にgoal 29のtaskで決定16に置き換える。
- plan reviewの判定を誤ると、要らないtaskがreadyで走るか、要るtaskが差し戻される。前者はrunのreviewの`concern`、observerの`stats`、人のcancelで拾い、後者はreviseの上限とconcernのaskで人に届く。
- `DAGQ_ROLE`による権限の判定（observerの許可の一覧、`reviewer`の読み取りだけの制限、bypassの無い`ready`の拒否）は申告に依存する柵で、悪意ある実行を防ぐものではなく、promptの誤りから状態を守るにとどまる。
- [ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)の本文は書き換えず、決定4の「`ready`にするか、cancelするかは人の判断」は決定16で読み替える。[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)と[ADR-0028](0028-workspace-titles-are-repo-and-role.md)の「`up`がplannerを開く」「常駐のplanner」の記述も、本ADRの決定1・6で読み替える。
- [overview](../design/overview.md)、[supervisor-lifecycle](../design/supervisor-lifecycle.md)、[domain-model](../design/domain-model.md)、[persistence](../design/persistence.md)、[plugin-integration](../design/plugin-integration.md)は各実装taskで更新する。
