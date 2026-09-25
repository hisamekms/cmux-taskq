---
id: adr-0044
type: adr
title: ADR-0041の役割・proposal・plan reviewを引き継ぎ、observerの検出をfindingにし、findingとaskのanswerからruntimeが立てるplannerがproposalを作り、人へのエスカレーションはAIが判断し、observerは変化の無いときに何もせず、記録を見るCLIを持つ
status: superseded
created: 2026-09-26
updated: 2026-09-26
accepted_on: 2026-09-26
superseded_by: adr-0047
superseded_on: 2026-09-26
supersedes:
  - adr-0041
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
  - adr-0036
  - adr-0037
  - adr-0038
  - adr-0039
  - adr-0040
  - adr-0041
  - adr-0042
  - adr-0043
  - adr-0046
  - design-overview
  - design-supervisor-lifecycle
  - design-domain-model
  - design-plugin-integration
  - design-persistence
---

# ADR-0044: ADR-0041の役割・proposal・plan reviewを引き継ぎ、observerの検出をfindingにし、findingとaskのanswerからruntimeが立てるplannerがproposalを作り、人へのエスカレーションはAIが判断し、observerは変化の無いときに何もせず、記録を見るCLIを持つ

> **置き換え済み（2026-09-26）**: このADRの決定は現在有効ではない。現行の決定は[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)を読む。

## Context

[ADR-0041](0041-on-demand-planners-proposals-submitted-and-plan-review-job.md)（goal 29、2026-09-25）は、役割をsupervisor / worker / planner / inbox / observerの5つにし（[ADR-0024](0024-retire-maintainer-into-jobs-and-observer.md)を置き換え）、plannerをproposalごとのオンデマンドのworkspaceにし、taskに`submitted`を足してsupervisorが起動するplan review jobだけが`ready`にし、follow_upのdraftをruntimeが立てるplannerに決めさせた（[ADR-0037](0037-follow-up-triage-job-decides-follow-up-drafts.md)を置き換え）。その実装はgoal 29のtask 274〜277と280〜282で着地した。task 282は人の決定（2026-09-25）で、runtimeが立てるplannerの対象をfollow_upのdraftからruntimeやjobが作ったdraft全般（出どころ`follow_up` / `goal_gap`）に広げた。

ADR-0041の決定4は、observerの出力をnote（run_eventsのkind `observation`）・`blocked`のask・draftのgoalの3つにしていた。運用で次の問題が出た（goal 31）。

- 検出は自由文のnoteで、根拠のevent IDは本文の中にしか無い。同じ状態を毎回書き直す。queueが止まっていた5時間、observerはほぼ同じnoteを毎時間書いた。
- 再発した問題はdraft goalにするが、observerはheadlessの単発のjobなので、plan reviewの差し戻しに応えられない。draft goalは人が開いたplannerが拾うまで動かない。
- `blocked`のaskの選択肢に「register a goal」があっても、answerを受けて動く者が居ない。
- 記録が読みにくい。`dagq events --all`にはrun_idもpayloadも出ず、絞り込めない。止まっていた区間はClaudeのtranscriptを読まないと分からない。observerが何を読んで何を書いたかは`output.log`の文章にしか無い。
- taskにもrunにも紐づかない`blocked`のaskは、openなものが1件にまとまる（`asks_open`の部分UNIQUE index）。別々のqueue全体の問題を同時に人に聞けない（task 244）。
- observerは変化の無い時間にも毎時間起動し、ユーザー設定のMCPサーバーを読み込んでから`stats`を読むだけで終わる。

2026-09-25のplannerとの対話で、ユーザーは次を決めた（goal 31のconstraints）。

- observerは見つけるところまで（findingの記録と更新、`blocked`のask）を行う。proposalを作るのはruntimeが立てるplanner（goal 29の仕組みで、同時の数の上限もそれに従う）。observerはdraft goalを書かない。
- findingを新しい記録にする。種類、対象（run / task / goal / queue）、最初と最後に見た時刻、発生回数、根拠のevent ID、状態（open / proposed / resolved / dismissed）、紐づいたproposalを持つ。同じ問題は新しい記録を作らず、既存のfindingの回数と根拠を更新する。noteは人とplannerの自由文のメモとして残す。
- proposalへの経路は2つ: observerがfindingを「proposalにすべき」と判断したとき（再発の回数、影響）と、inboxのaskに人が「提案にする」と答えたとき。runtimeがplannerを立て、findingやaskを渡してproposalを作らせる。plannerは既存のgoalへのtaskか新しいgoalかを選び、plan reviewに出す。findingから立てたplannerは、proposalを作る前にgoal 33の`dagq search` / `related`（[ADR-0046](0046-full-text-search-related-and-duplicate-of.md)）で既存のtaskを確かめる。
- 人の承認を一律には求めない。plannerとplan review（AI）が人の判断が要ると判断したときだけinboxのaskにする。新しいgoalでも同じ。
- CLIを先に作る（人と、observer・planner・plan reviewが同じものを読むため）: `dagq findings`、`dagq events --full`と`--run` / `--task` / `--kind` / 時刻の絞り込み、`dagq timeline RUN`、`dagq observe --history`。`dagq report`（Markdown / HTML）は後回しにする。
- observerは前回から自分以外のeventが無ければ何もせずに終わる。状態が変わっていないfindingを書き直さない。MCPを読み込まずに起動する。起動間隔は3時間にし、1日1回の傾向の観測は残す。observerはgoal 30の閾値の妥当性の`stats`（[ADR-0043](0043-detect-stalled-worker-sessions-nudge-once-then-ask.md)の閾値ごとの結果）を入力に読み、見直しが要ると判断したらfindingにする。
- `stats`の`conflict_hotspots`をobserverが読み、衝突の割合が閾値を超えて再発するファイルをfindingにする。そのfindingはこの経路でリファクタリング（ファイルの分割など）のproposalになる。
- task 244（taskの無い`blocked`のaskが1件しか開けない）は、findingの設計で要らなくなるかを本ADRで判断する。

ADR-0041の決定4を変えるので、[ADR-0042](0042-adr-is-superseded-whole-and-deprecation-date-is-deprecated-on.md)の決定2により、ADR-0041のまだ生きている決定を書き直して引き継ぎ、ADR-0041を丸ごと置き換える。ADR-0041はADR-0024とADR-0037を置き換えていたので、本ADRはその2本から続く決定もまとめて持つ。決定1〜17はADR-0041の決定1〜17を同じ番号で書き直したもので、ADR-0041（とADR-0024）の「決定N」を参照している文書とsourceは本ADRの決定Nと読める。書き直しでは、goal 29で着地した実装に合わせて形を具体にした（各決定に書く）。決定4は上の人の決定で改め、決定1・2・5・17をそれに合わせた。決定18〜23は新しい決定。goal 28で決めたtaskの優先度（[ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)の決定4）と、ADR-0046の検索・関連・重複の記録は変えない。

## Decision

**原則。** 常駐のsessionは、人に届くものの窓口（inbox）とループそのもの（supervisor）だけにする。人と対話する計画は、計画ごとにオンデマンドのworkspace（planner）を開いて行う。run 1件と計画1件に対する判断はheadlessのjobにし、jobはqueueを読むだけでverdictを返し、状態を変えるのはverdictとanswerを適用するruntimeだけにする。適用は1トランザクションにする。継続的改善は定期起動のjob（observer）にし、run / task / goalの状態を変える権限を持たせず、見つけたものを構造のある記録（finding）に積み上げる。findingを計画に変えるのはruntimeが立てるplannerで、人に聞くかどうかはplannerとplan review（AI）が判断する。人とAIは同じCLIで記録を読む。repository固有の規則（AGENTS.mdのverificationの規則、ADR番号など）はruntimeに埋め込まず、jobとplannerのpromptがrepositoryの文書から読む。人に届くものはすべてinboxに届ける。run_eventsのkindは追加だけで、既存のkind名とpayloadは変えない。schemaを変えるtaskは`user_version`を上げて`migrations/`に追加する。以下の23点を決める。

1. **役割はsupervisor / worker / planner / inbox / observerの5つにする。maintainerは退役したまま。**
   - **supervisor**: runtimeの`supervise`プロセス。claim、worker起動、validating、resume、jobの起動（review、triage、plan review、observer）、verdictとanswerの適用、着地、runtimeが立てるplannerの起動と終了、後始末を行う。
   - **worker**: runごとにsupervisorが開くオンデマンドのworkspaceで、worktreeで作業するClaude session。
   - **planner**: proposal（決定7）を書くオンデマンドのworkspace。人が`dagq plan`で開くもの（`origin: person`）と、runtimeが立てるもの（`origin: runtime`。決定12、14、16、19）がある。goalとtaskを書き、proposalをsubmitする（決定8）。`ready`にはしない。
   - **inbox**: 唯一の常駐session。openなaskを人に見せてanswerを書き戻し、それ以外のattentionを人に知らせる。自分では判断しない（[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定4）。
   - **observer**: supervisorのtimerで定期起動するheadlessのjob（決定4）。findingを記録・更新し、`blocked`のaskを上げる。
   - 検査はsupervisorが起動するheadlessのplan review jobが行う（決定10、11）。plan review jobは同時に1つで、runのreview jobと対になる。
   - maintainerという役割、`[<repo>]dagq maintainer`のworkspace、`DAGQ_ROLE=maintainer`は無い。[ADR-0010](0010-maintainer-and-resident-supervisor.md)以降のADRに残るmaintainerの記述は書き換えず、[overview](../design/overview.md)の用語集で「review job / triage job / observer / inboxのいずれか」に読み替える（レビューと着地はreview job、失敗runの扱いはtriage job、継続的な監視はobserver、人への相談とanswerの実行はinbox。`up` / `down`などの操作は人がinboxかplannerのsessionから打つ）。
2. **jobは、cmux workspaceを持たないheadlessのClaude実行にする。**
   - 起動は`AgentProvider`のheadless実行のport（`headless_command`。Claudeでは`claude -p`）を使う。promptに入力のpathと出力のJSON schemaを渡し、stdoutのJSONを読む。未知のフィールドは拒否する。
   - jobは次の4種類。
     - **review job**: `awaiting_integration`のrunをレビューし、`pass` / `revise` / `concern`を返す（ADR-0040の決定2、[ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)）。
     - **triage job**: `failed` / `interrupted`のrunのreceipt・log・run_eventsを読み、`retry` / `resume` / `ask`のverdictを返す（決定3）。
     - **plan review job**: submitされたproposalを検査し、`pass` / `revise` / `concern`を返す（決定10、11）。
     - **observer job**: supervisorのtimerで定期起動し、findingと`blocked`のaskを残す（決定4）。verdictは返さず、許されたCLIだけで書く。
   - review / triage / plan reviewのjobのenvは`DAGQ_ROLE=reviewer`と`DAGQ_QUEUE`で、CLIは読むコマンドだけを許す。cwdはrepositoryのmain checkoutで、jobはrepositoryの文書とsourceを読める。observerのenvは`DAGQ_ROLE=observer`（決定4）。
   - jobの起動・終了・失敗はrun_eventsに記録する（`review_started` / `review_finished` / `review_failed`、`plan_review_started` / `plan_review_finished` / `plan_review_failed`、`observe_started` / `observe_finished`）。headless実行が失敗したとき（起動できない、非0終了、timeout、stdoutがschemaに合わない）は、対象を動かさず、inbox宛てのattentionかaskにする。review / triage / plan reviewのtimeoutは`AgentProvider::review_timeout`（既定600秒）で、ループはプロセスを`try_wait`で見るだけで止まらない。
3. **triage jobのverdictでruntimeが動き、安全に自動化できる後始末はruntimeが行う。**（ADR-0041の決定3を引き継ぎ、verdictのフィールド名を実装に合わせた。実装済み）
   - supervisorは`failed`になったrunと、下の自動`recover`で`interrupted`になったrunごとにtriage jobを1回起動する。verdictは`{"verdict": "retry" | "resume" | "ask", "reason": "...", "instruction": "..."}`に固定し（ADR-0041は`reasons`と`summary`と書いていたが、実装の形に合わせた）、payloadに記録する。
     - **retry**: runtimeがtaskを`in_progress`から`ready`に戻す。次のclaimで新しいrunが作られる。taskの中身は変わらないので、plan reviewにはかけない（決定8のreadyの権限の例外）。
     - **resume**: runtimeがrunを`failed` / `interrupted`から`needs_session`にし、自動resume（[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)の決定1）で同じsessionに続けさせる。試行3回の上限はrunごとの通算で数え、triageからのresumeも含める。上限を超えたrunは再びtriageに回らず、`decide`のaskになる。
     - **ask**: `kind: decide`のaskをinbox宛てに作り、`reason`と`instruction`をquestionに載せて待つ。answerに従う操作は人がinboxかplannerのsessionから打つ（inboxは自分では判断しない）。
   - triage済みのrunのworkspaceはruntimeが閉じる。
   - wrapperが死んでleaseの無いrunは、supervisorが`recover`まで自動で行う。`recover`の結果は`interrupted`で、taskを`ready`にはせず、triage jobに回す。[ADR-0039](0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)（ADR-0012を置き換え）の引き継ぎはそのまま。[ADR-0003](0003-supervisor-owns-lifecycle.md)・[ADR-0007](0007-run-level-leases-parallel-execution.md)の「孤児runは自動再実行しない」は、再実行をtriageのverdictに委ねることで維持する。
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
   - 本ADRより前にobserverが登録したdraftのgoalは、人が開いたplannerで人と採否を決める。採るなら、plannerがそのgoalとtaskを自分のproposalにしてsubmitし、採らないなら`goal close`（`abandoned`）にする。
   - [ADR-0009](0009-goal-groups-tasks.md)の「goalは状態機械を持たない」は、draftか否かの1点に限って改めたまま。進捗は従来どおりtaskの状態から導く。
6. **`up`はsupervisorとinboxだけを開き、plannerは`dagq plan`で開く。**（ADR-0041の決定6を変えずに引き継ぐ。実装済み）
   - `up`が作るworkspaceはsupervisor（in-cmux mode）とinboxだけ。常駐のplannerはやめ、`up`は`session_workspaces`のsupervisor・inbox以外の行（常駐plannerの`planner`行とmaintainerの行）を忘れる。残ったworkspaceは人が閉じる。
   - 人は`dagq plan`で人が開くplannerのworkspaceを開く。何度打っても新しいworkspaceを開き、複数同時に開ける。plannerは`planners`表の1行で、workspaceは[ADR-0026](0026-identify-workspaces-by-uuid-env-and-queue-group.md)のとおりUUIDで識別し、`--env`に`DAGQ_ROLE=planner`、`DAGQ_QUEUE`、`DAGQ_PLANNER_ORIGIN`、`DAGQ_PLANNER_ID`を持たせ、queueのworkspace groupに入れる。titleは表示専用で、[ADR-0028](0028-workspace-titles-are-repo-and-role.md)の`[<repo>]planner`にplannerのIDを足した`[<repo>]planner#<planner-id>`（runtimeが立てたものは後ろに対象のproposalかdraft taskを足す）。
   - `up` / `down` / 固定バイナリの更新は、人がinboxかplannerのsessionから打つ。
   - in-cmux modeのsupervisorが止まったときは、inboxの`watch`が`supervisor_stopped`で拾って人に知らせ、人が`up`を打つ。`watch --role inbox`は`ask_opened`・`ask_answered`・`supervisor_stopped`とそれ以外のattentionを受ける。in-cmux modeに自動再起動が無いこと（[ADR-0011](0011-cmux-socket-password-and-in-cmux-fallback.md)）は変えない。
7. **proposalを、plan reviewと差し戻しの単位にする。**（ADR-0041の決定7を変えずに引き継ぐ。実装済み）
   - proposalは、goal（0個以上）とtaskの束に、持ち主のplanner（workspaceのUUIDと、人が開いたかruntimeが立てたか）を結び付けたもの。plan reviewはproposal単位で検査し、reviseはproposalの持ち主に返す。
   - 1つのtaskは同時に1つのproposalにだけ属する。plannerは`add`や`goal add`で書いたgoal / taskを自分のproposalに入れる。既存のdraftのtask（保留・退避したtask、本ADRより前のobserverのdraft goalのtask）も、plannerが自分のproposalに入れてsubmitできる。
   - 持ち主のplannerが閉じた後に差し戻すときは、runtimeが立てた新しいplannerがproposalの持ち主になる（決定12）。
8. **taskの状態に`submitted`を足し、`ready`にするのはplan review jobだけにする。**（ADR-0041の決定8を変えずに引き継ぐ。実装済み）
   - `submitted`はplan review待ち。plannerが`dagq submit`でproposalを出すと、proposalのtaskが`draft`から`submitted`になる。supervisorは`submitted`のtaskをclaimしない。
   - `draft`は「まだ出していない」の意味だけになる。runtimeやjobが作ったdraft、保留、退避などは、submitしない限りreadyにならない。
   - `ready`にするのは、plan review jobのverdictを適用するruntime（決定11）と、`concern`のaskに人が`ready`と答えたときのruntimeだけ。plannerとjobはsubmitまでを行う。
   - 例外は2つ。人が明示したbypass（`ready --bypass-review`。bypassしたことをeventに記録する）と、triageの`retry`（決定3。中身の変わらない同じtaskを戻すだけ）。bypassの無い`ready`は、どのroleから打たれても拒否する。
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
16. **runtimeやjobが作ったdraftは、1件ごとにruntimeが立てるplannerに採否を決めさせる。**（ADR-0041の決定16を引き継ぎ、task 282の人の決定で対象をfollow_upのdraftからruntimeやjobが作ったdraft全般に広げた。ADR-0037のfollow-up triage jobを置き換えたまま。実装済み）
    - runtimeやjobがdraftを作るときは、出どころ（`follow_up`: `integrate`がreceiptの`follow_ups`から登録したもの、`goal_gap`: goalを判断するjobがgapから作ったもの）と、plannerに見せる材料を記録する。`integrate`がreceiptの`follow_ups`からdraft taskを登録し、`follow_up_registered`を記録することと、その形（titleとdescriptionだけ）は変えない（ADR-0019の決定4）。
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
    - goal closeの前にplannerが確かめるもの: 所属goalのdraftのうち、plannerの待ち、openな`planner_question`、`keep_draft`で残ったもの。`keep_draft`は人が開いたplannerで人と決めて片付ける。
17. **人に届くものはすべてinboxにし、plannerに返すのはそのplanner自身のproposalへのreviseと、そのplannerが作ったaskのanswerだけにする。**（ADR-0041の決定17を引き継ぎ、実装で足したattentionとfindingのaskを書いた）
    - inboxに届くもの: `approve_plan`・`planner_question`・`blocked`（決定4、23）と他のすべてのask、`plan_review_failed`、`planner_unresponsive`、`draft_planner_exhausted`、決定19のfindingのplannerの上限超え、それ以外のすべてのattention。
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
    - **入力に足すもの**: goal 30の閾値の妥当性の`stats`（ADR-0043の決定6の閾値ごとの結果`stall_thresholds`と、決定3の結末の内訳）を読み、閾値の見直しが要ると判断したら種類`threshold`のfindingにする。`stats`の`conflict_hotspots`を読み、衝突の割合が閾値を超えて再発するファイルを種類`conflict_hotspot`、対象`queue`、`subject`にそのファイルのpathを持つfindingにする。これは決定19の経路でリファクタリング（ファイルの分割など）のproposalになる。
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

実装の状況: 決定1〜3、6〜15と決定16はgoal 29のtask 274〜277、280〜282で実装された（細部は設計文書にある）。決定10のうちplan review jobがADR-0046の`search` / `related`を使うこと（`related`とjobにdagqの読み取りを許すこと）、決定4の変更（noteとdraft goalの廃止、許可の一覧、間隔の既定）、決定5のうちobserverのdraft goalをやめたこと、決定17のうちfindingに関わるもの、決定18〜23は本ADRの時点では未実装で、goal 31の後続taskが実装する。

## Alternatives

- **observerがdraft goalを書き続ける**（ADR-0041の決定4のまま）: 実装は既にあるが、observerはheadlessの単発のjobなので、plan reviewのreviseにも人の質問にも応えられない。draft goalは人が開いたplannerが拾うまで動かず、goal 31の問題（同じ提案の書き直し、誰も動かない選択肢）が残る。observerは見つけるところまでにし、計画はreviseに応えられるruntimeのplannerに作らせる。
- **observerがproposalを直接submitする**: plannerを立てる手間が要らないが、observerはreviseを受け取れず、`search` / `related`で既存のtaskを確かめて書き直すこともできない。submitの権限を持たせると、状態を変えない柵（決定4）も崩れる。
- **findingから作るproposalに、人の承認を一律に求める**: 誤った計画が走る余地は減るが、人の手が要らないものまで人を待ち、goal 29でplannerの交通整理をplan reviewに移した狙いに逆らう。人に聞くかの判断はplannerとplan review（決定20）に持たせ、誤りはrunのreview、observerの`stats`、人のcancelで拾う。
- **`dagq report`（Markdown / HTML）を先に作る**: 人には読みやすいが、observer・planner・plan reviewが読む形にならず、同じ事実を2つの経路で組み立てることになる。先にCLIで記録を読めるようにし、reportはその出力を並べ直すだけになってから決める。
- **noteに構造（種類、対象、根拠）を足してfindingの代わりにする**: 表を増やさずに済むが、noteは人とplannerの自由文のメモとしても使われており、状態（open / proposed / resolved / dismissed）とproposalとの紐付けを持たせると、メモと手当ての対象が混ざる。同じ問題の積み上げ（回数と根拠）もnoteの追記では表せない。
- **findingを自動で`resolved`にしない（人が閉じる）**: 誤って閉じることはないが、手当てが着地したfindingが`proposed`のまま残り、`findings`の一覧が読めなくなる。proposalのtaskの完了を解消とし、再発したら`open`に戻すことで、手当てが効かなかったことも残る。
- **task 244をfindingと別に直す（taskの無い`blocked`の一意性を別の鍵にする）**: 小さな変更で済むが、鍵に使えるものが問題の種類しか無く、同じ種類の別の問題を分けられない。findingが問題の同一性を持つので、その鍵を使う。
- **変化の無いときもobserverを起動し、observerに「何もしない」と判断させる**: 判断を一本化できるが、起動とMCPの読み込みとpromptの分の時間とtokenを毎回使う。自分以外のeventが無いかは決まった規則で判定できるので、runtimeが判定する。
- **常駐のgate session（検査役のClaude session）を置く**（ADR-0041から引き継ぐ）: ADR-0024で常駐のmaintainerをやめたのと同じ理由で採らない。検査はproposal 1件に対する処理で、常駐させるとcontextとcompactionを抱え、寝ている間はproposalが止まる。
- **gateが全部を自分で直す**（ADR-0041から引き継ぐ）: description・acceptance・pathsの書き換えは計画の意図に触れ、誰が決めたかが分からなくなる。jobに許すのは、意図を変えずに安全側に倒れる4つに限る。
- **plannerを常駐1つのままにする**（ADR-0041から引き継ぐ）: 計画の交通整理が1つのsessionに集まり、人がAIを待つ詰まりが解けない。
- **draftのまま印で区別する（`submitted`を状態にしない）**（ADR-0041から引き継ぐ）: 「出したがまだ検査していない」と「まだ出していない」がどちらも`draft`になり、claimしない理由とreadyの権限の判定が印の組み合わせになる。
- **interruptの優先度を持つtaskにgateを飛ばさせる**（ADR-0041から引き継ぐ）: 急ぎで書いたtaskほど重複や矛盾を含みやすく、誤りの影響も大きい。検査の順を先にするだけにし、本当に飛ばしたいときは`ready --bypass-review`を使う。
- **follow-up triage job（ADR-0037）を残す、follow_upの採否を`integrate`の中で同期に決める、workerがreceiptに完全なtaskを書く、observerに決めさせる**（ADR-0041から引き継ぐ）: follow_upだけ別のgateを通ること、着地のslotをClaudeの待ちで塞ぐこと、workerが着地後の状況を見られないこと、observerが状態を変えない柵の上に成り立つことから採らない。
- **maintainerを常駐で残す、observerを常駐sessionにする**（ADR-0024から続く）: run 1件に対する処理と、即座に反応する必要の無い観測に常駐を置く理由が無い。noteを`docs/journal/`に書く案は[ADR-0036](0036-delete-frozen-work-records.md)で`docs/journal/`が削除されたので選択肢から外れた。
- **follow_upを自動採用する上限を設けない**（ADR-0037から続く）: 自動で採用したtaskが次のfollow_upを生み、人の判断を経ずにgoalが膨らみ続けうる。

## Consequences

- observerの検出は、種類・対象・回数・根拠を持つfindingとして積み上がり、同じ問題は1件にまとまる。人とAIは`findings`で影響の大きい順に読める。observerの書いたnoteとdraft goalは増えなくなる。既存のobserverのnoteは記録として残り、既存のdraft goalは決定5のとおり人が開いたplannerで扱う。
- 改善の提案は、observerの判断か人のanswerを受けて、runtimeが立てるplannerがproposalにし、plan reviewを通って`ready`になる。runtimeが立てるplannerの上限（`--runtime-planners`）をrevise・draft・findingが共有するので、findingが多いとdraftやreviseの配送が待つ。上限は運用で見直す。
- 人の承認を一律に求めないので、plannerとplan reviewの判定を誤ると、要らないtaskが走ることがある。runのreviewの`concern`、observerの`stats`とfinding、人のcancelで拾う。
- observerは変化の無い時間に起動しなくなり、MCPも読まないので、時間とtokenが減る。間隔が3時間になるので、閾値超えの詰まりをobserverが`blocked`のaskにするまで最大3時間かかる。走っているrunの詰まりは[ADR-0043](0043-detect-stalled-worker-sessions-nudge-once-then-ask.md)のsupervisorの検知が先に拾う。
- schemaが変わる: findingの表、`blocked`のaskとfindingの結び付けと`asks_open`の一意性、findingから立てたplannerの記録。run_eventsにfindingの記録・更新・遷移、observerの`skipped`、findingのplannerの起動が加わる。
- CLIに`findings`、`events --full`と絞り込み、`timeline`、`observe --history`が加わり、observerの許可の一覧が変わる（`note`と`goal add --draft`と`add`を外し、findingのコマンドを足す）。
- task 244は別のtaskとしては要らない（決定23）。planner（人）はtask 244をcancelし、findingを実装するtaskのacceptanceにaskの一意性の変更を含める。
- pluginのskill（`dagq`の`reference/observer.md`、`dagq-inbox`、`dagq-planner`）とAGENTS.mdのobserverの節は、実装が入った後にgoal 31のtaskで新しい流れに書き換える。
- `DAGQ_ROLE`による権限の判定（observerの許可の一覧、`reviewer`の読み取りだけの制限、bypassの無い`ready`の拒否）は申告に依存する柵で、悪意ある実行を防ぐものではなく、promptの誤りから状態を守るにとどまる（ADR-0041から引き継ぐ）。
- [ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)の決定4の「`ready`にするか、cancelするかは人の判断」は決定16で、[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)と[ADR-0028](0028-workspace-titles-are-repo-and-role.md)の「`up`がplannerを開く」「常駐のplanner」は決定1・6で読み替える（ADR-0041から引き継ぐ）。
- ADR-0041の本文は書き換えず、`superseded`にして本ADRを指す。ADR-0041の「決定N」を参照する文書とsource、ADR-0043・ADR-0045・ADR-0046の本文の参照は、本ADRの同じ番号の決定として読める（決定4だけは内容が変わった）。
- [overview](../design/overview.md)、[supervisor-lifecycle](../design/supervisor-lifecycle.md)、[domain-model](../design/domain-model.md)、[persistence](../design/persistence.md)、[plugin-integration](../design/plugin-integration.md)は各実装taskで更新する。
