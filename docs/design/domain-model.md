---
id: design-domain-model
type: design
title: Domain model
status: current
created: 2026-09-21
updated: 2026-09-25
last_verified: 2026-09-25
scope: domain
related:
  - adr-0041
  - adr-0038
  - adr-0003
  - adr-0004
  - adr-0007
  - adr-0008
  - adr-0009
  - adr-0016
  - adr-0019
  - adr-0040
  - adr-0024
  - adr-0029
  - adr-0034
  - design-persistence
---

# Domain model

## Implementation status

ステップ2で`Task`、`TaskDependency`、`TaskRun`、`RunEvent`、ステップ3で`RunProcess`と`SupervisorLease`、ステップ4で`Receipt`を実装した。ステップ6（[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)）で`SupervisorLease`を`RunLease`に置き換え、ステップ7（[ADR-0008](../adr/0008-merge-queue-squash-landing.md)）でrunの`integrating`と`needs_session`、`IntegrationOutcome`の`needs_session` / `failed` / `no_run_awaiting`を加えた。[plan](../plans/current.md#after-first-dogfooding)のステップ9の後に`Goal`と`Task.goal_id` / `Task.context`、receiptの`follow_ups`を加えた（[ADR-0009](../adr/0009-goal-groups-tasks.md)）。goal 12でgoalのdraft状態（`GoalStatus`）と、run_eventsのkind `observation`で表すnote（`NewNote`）を加えた（ADR-0024の決定4、5）。goal 8のtask 73で`Task.required_evidence`（`EvidenceCheck`）と`Receipt::missing_evidence`を加えた（ADR-0019の決定5）。goal 21のtask 195で失敗・保留・中断の理由の分類コード`ReasonCode`（`src/domain/reason.rs`）を加えた（[ADR-0034](../adr/0034-domain-events-carry-reason-codes-actor-and-configuration-changes.md)の決定1。[理由の分類コード](#理由の分類コードcode)）。goal 20のtask 178でtaskからgoalへの依存（`TaskGoalDependency`）を加えた（[ADR-0038](../adr/0038-task-depends-on-a-goal-until-it-is-achieved.md)）。Rustの型と手動遷移規則は`src/domain/`（構成は[集約: TaskとGoal](#集約-taskとgoal)）、ストレージとprovider/workspaceの契約は`src/application.rs`、永続化は`src/infrastructure/sqlite.rs`と`src/infrastructure/runtime_store.rs`にある。`AgentSession`と`Workspace`は独立エンティティにせず、TaskRunの`id`（Claude session ID）と`workspace_id`で表す。

## Entities

- `Goal`: 複数のtaskが解く上位の課題。title、description、acceptance、constraints（命名・境界・やらないこと）、doc（repository内の参照文書のpath、任意）を持つ。verification_commandsは持たず、進捗は所属taskのstatusから導出する。状態は`status`（`GoalStatus`: `draft` | `open`）の1点だけで（ADR-0024の決定5がADR-0009の「状態機械を持たない」をこの1点に限って改めた）、`draft`のgoalのtaskは`candidates`に出ずclaimされない。閉じたことは`status`と独立に`closed_at`と`verdict`（`achieved` | `abandoned`）で1回だけ記録する。
- note（observation）: task / run / goalのいずれかに紐づく自由記述。独立のentityにせず、`RunEvent`のkind `observation`（payload `{text, kind, by}`）で表す。`kind`は小文字・数字・`-`・`_`のslug（既定`note`）、`by`は書いた環境の`DAGQ_ROLE`で、無ければ`human`。
- `Task`: ユーザーが登録する作業。公開statusを持つ。`goal_id`（任意）で1つのgoalに属し、`context`（既定は空）に「なぜやるか」と参照文書を持つ。
- `TaskDependency`: taskからpredecessorへの有向辺。循環は禁止する。
- `TaskGoalDependency`: taskからgoalへの有向辺（[ADR-0038](../adr/0038-task-depends-on-a-goal-until-it-is-achieved.md)）。goalが`achieved`で閉じるまでtaskはclaimされない。goalの所属taskが全部終端でも、goalが開いている間（follow_upsのdraftが足されうる間）は待ち、`abandoned`で閉じたgoalは解かない。goalは所属taskを待つとみなし（暗黙の辺）、task依存の辺と合わせたグラフで循環を禁止する。自分の属するgoalへの辺は張れない。
- `TaskRun`: 1回の実行試行。provider、worktree、branch、結果、実行statusを持つ。
- `AgentSession`: providerが起動したセッション。プロセスとprovider固有識別子を持つ。
- `Workspace`: cmux workspace。TaskRunと1対1で関連し、receipt検証を通った後にsupervisorが閉じる。閉じたことの確認は`TaskRun.workspace_closed_at`で持つ。
- `RunLease`: 1つのrunを所有するプロセス（実行中はsupervisor、着地中は`integrate`）のPIDとheartbeat。runごとに高々1つで、そのプロセスがrunを扱っている間だけ存在する。
- `RunProcess`: runごとのsession wrapperとagentのPID、heartbeat、終了コード。
- `RunEvent`: 実行中に発生した永続イベント。
- `Receipt`: agentが提出する完了レシート。run ID、結果、commit、tests/e2e/subagent_reviewの状態と証跡または理由、要約と、任意の`follow_ups`（workerが提案する後続task。`{"title", "description"}`の配列）を持つ。構造の整合性は`Receipt::check`、Gitと検証コマンドの確認はsupervisorが行う。`follow_ups`は配列であることだけを確認し、検証には使わない。`integrate`が着地後に各項目（titleのあるもの）を元のtaskと同じgoalのdraft taskとして登録し（goalが閉じていればgoalなし）、runに`follow_up_registered`を記録する（ADR-0019の決定4）。`ready`にするかは人が決める。`IntegrationOutcome`の`integrated`は登録したtaskを`follow_ups`（`RegisteredFollowUp`: `task_id`、`title`の配列）に持つ。

`Task.id`と`Goal.id`はSQLiteの整数ID、`TaskRun.id`はUUID。Taskはtitle、description、acceptance、verification_commands、required_evidence、paths、priority、goal_id、contextを保持する。`priority`は[ADR-0040](../adr/0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)の決定4の5段階の`Priority`（`low` < `normal` < `high` < `urgent` < `interrupt`の順の`Ord`、既定`normal`）で、`add --priority LEVEL`で与え、draft / readyの間は`set-priority TASK LEVEL`で変える（`task::set_priority`。それ以外のstatusは`TaskNotEditable`で拒否し、変化があったときだけ`task_priority_changed`（`from`、`to`）を記録する）。CLIとJSONは名前（小文字）だけを扱い、数値は受け付けない（`Priority::from_str`、clapの`value_parser`）。走っているrunは止めず、次のclaimにだけ効く。`paths`はrunが変えてよいパスのglobの配列（[ADR-0029](../adr/0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)）で、`add --paths`で与え、draft / readyの間は`set-paths TASK --paths GLOB... | --none`で置き換える（変化があったときだけ`task_paths_changed`（`from`、`to`）を記録する）。空なら制限しない。globの規則と判定は`domain::scope`が持つ: repository root起点のパス全体に合わせ、`*`と`?`は1つのsegmentの中、segment全体が`**`なら0個以上のsegmentに合い、ほかは字義どおり。`validate_path_globs`が空・`/`始まり・`.` / `..` / 空のsegmentを拒否し（`DomainError::InvalidPathGlob`）、`out_of_scope(globs, changed)`がどのglobにも合わない変更パスを返し、`scope_violation_reason`が`changed paths outside the task's --paths: <paths>`の形にする。`required_evidence`はvalidationがreceiptに要求するcheckの配列（`EvidenceCheck`: `tests` | `e2e` | `subagent_review`。receiptのcheck名と同じ）で、`add --evidence`で与え、`NewTask::required_evidence()`が与えた順に重複を除いて保存する。無ければ空配列で、従来どおり何も要求しない。`Receipt::missing_evidence(required)`は要求されたcheckのうち`status`が`passed`でないか`evidence_or_reason`が空白のものを要求の順に返し、`evidence_missing_reason`がそれを`evidence missing: e2e`（複数は`, `区切り）の形にする。TaskRunはprovider、base commitと、branch/worktree/workspace/receipt/log/result commitの任意参照を持ち、idle marker `idle.json`のpathは`run_dir`から導出する。`run_dir`・worktree・receipt・logの配置は`RunPaths`（`<runs dir>/<run-id>/`の`worktree/`、`receipt.json`、`claude.debug.log`）が決め、storeは読み出しのたびに`TaskRun::relocated`でqueueの今の`runs/`から解決し直す（[ADR-0017](../adr/0017-resolve-run-paths-from-the-queue-directory.md)）。claim時のproviderは`claude`のみで、リソース参照は作成前のためnullになる。

## Current operations

- `add`でdraftを作り、`draft → ready`、`ready → draft`、`draft/ready → canceled`を手動操作できる。
- `claim`だけが`ready → in_progress`へ遷移させる（`task::claim`。readyでなければ`TaskNotClaimable`）。同じトランザクションで`TaskRun::new`がclaimed状態のTaskRunを作り、イベントを記録し、supervisorからのclaimはそのrunの`RunLease`も作る。キュー全体の実行枠はなく、依存が解けたtaskは`supervise --parallel N`の上限まで同時に実行される。
- supervisorはrunを（遷移の判断は[集約: TaskRun](#集約-taskrun)のコマンド）`claimed → starting`（path計画）→ `running`（agent起動）→ `validating`または`failed`（wrapper終了）→ `awaiting_integration`または`failed`（receipt検証）へ進め、`awaiting_integration`のworkspaceを閉じて`workspace_closed_at`を記録し、休止したrunの`RunLease`を解放する。各遷移はそのrunのleaseまたはwrapperの所有を要求する。runtime errorではsupervisorがそのrunだけを手放す（statusは変えず、`last_error`を書き、leaseを消す）。
- `integrate`だけが`awaiting_integration | needs_session → integrating`と、そこからの`→ integrated`（Taskは`in_progress → completed`、`result_commit`は`main`に積んだsquash commit）、`→ needs_session`（rebaseの衝突、再検証の失敗）、`→ failed`（セッションが書き直したreceiptが`failed`）、`→ 元のstatus`（mainを進める前のerror）を行う。`integrating`はqueue全体で1件。結果は`IntegrationOutcome`（`integrated` / `needs_session` / `failed` / `no_run_awaiting`）で返す。`integrate --next`は`awaiting_integration`のrunを検証完了の古い順に取り、`needs_session`は`integrate ID`で明示的に再開する。
- `in_progress`のTaskは、未完了run（claimed/starting/running/validating/awaiting_integration/integrating/needs_session）がある間は手動変更できない。すべてのrunが`failed`または`interrupted`になった`in_progress`は`ready`/`draft`/`canceled`へ手動で戻せる。再試行は新しいTaskRunになる。終端状態は変更できない。依存の追加・削除はdraft/readyだけに許可する。
- `recover`は未完了runを、そのrunの登録プロセスとleaseの所有者が停止していることを確認してから`interrupted`にする（`integrating`なら`awaiting_integration`へ戻す）。他のrunには触れない。Taskは`in_progress`のままで、`ready`への復帰は別操作。
- attention（人の判断で止まっている遷移か、supervisorが動かしている遷移。ADR-0016）の判定はdomainが持つ。`event_attention(kind, payload)`はrun_eventsの1件を、`run_attention(status, exit_pending, push_pending, leased)`はrunの今のstatus（と、`/exit`のtimeout、pushの失敗、lease行の有無）を、`supervisor_attention(pulses)`は`supervisors`表から導出した`SupervisorPulse`（token、pid、alive、stale）の並びを判定し、`AttentionNext`（`review and integrate` / `resuming (runtime)` / `triaging (runtime)` / `triage by hand` / `restart supervisor`など）を返す（`send /exit`はtask 104で消え、`/exit`のtimeoutはsupervisorの`stuck_exit`のaskになった。`inspect and close workspace`はtask 98で消え、`failed` / `interrupted`のrunはsupervisorのtriageになった。`resume session`と`answer the prompt in workspace <id>`はtask 100で消え、3回resumeして解消しない`needs_session`はsupervisorが`failed`にしてtriageの`decide`のask（`retry` / `cancel`）に、`running`のrunのdialog（`prompt_waiting`）は`answer_prompt`のaskになった。`needs_session`のrunはどの場合も`resuming (runtime)`。ADR-0024のConsequences）。`AttentionNext`は文字列としてserializeする。attentionはすべてinboxのもの（`ATTENTION_ROLE`、ADR-0024の決定6）で、plannerのものは無い。staleの規則`heartbeat_stale(alive, age)`と`HEARTBEAT_TIMEOUT_SECS`（30秒）もdomainにある。supervisorの起動・停止はrun_eventsに載せない。読み口は[supervisor-lifecycle](supervisor-lifecycle.md#events--watch)の`status` / `events` / `watch`。
- `succeeded`（統合を待たずに成功とする運用）への遷移はまだ公開していない。
- `candidates`は全依存がcompletedで、goal依存のgoalがすべて`achieved`で閉じた（`closed_at IS NOT NULL AND verdict = 'achieved'`）ready taskをclaim順（下記`graph`の`candidates`と同じ）で返す（条件は`READY_QUERY`、順は`claim_order`。supervisorのclaimも同じ条件と順）。CLIの`candidates`はtaskのJSONに`effective_priority`を足す（`application::claim_candidates`）。claimはTaskごとの未完了run 1件の制約だけを再確認する。
- `graph [--goal ID]`は未完了（draft / ready / in_progress）のtaskの依存の見取り図を返す（[ADR-0040](../adr/0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)の決定4）。storeの`graph_input`が未完了taskと直接の依存元（完了済みも含む）、`candidates`のIDを1つのsnapshotで読み、application層の純粋関数`dependency_graph`が`DependencyGraph`を作る。taskごとに`depends_on`（直接の依存元すべて）、`goal_dependencies`（依存先goalのIDすべて）、`ready_after`（未完了の依存元のID、続けて`achieved`で閉じていない依存先goalを`{"goal": ID}`で。`abandoned`で閉じたgoalも残る）、`blocks`（直接依存している未完了taskと、自分の属する閉じていないgoalに依存している未完了task）、`unblocks`（`blocks`を辿った推移閉包の未完了taskの数。canceledと完了済みは数えない）、`priority`と`effective_priority`（名前）を持つ。`effective_priority`は自分の優先度と、同じ推移閉包のうち`ready`でdraftのgoalに属さないtaskの優先度の最大値（`application::effective_priority`）で、draft・canceled・completedのtaskとdraftのgoalのtaskからは継承しない（閉包を辿る途中のdraftのtaskは通り抜ける。継承するかは待っている側のtaskのstatusだけで決まる）。goal依存は`graph_input`が`GraphGoalDependency`（goal IDとverdict）で渡す。`candidates`はclaim順（`ClaimRank`: `effective_priority`の降順 → `unblocks`の降順 → IDの昇順。goal 13のgoalのrankは優先度の次・`unblocks`の前に入る位置をコメントで示してある）、`critical`は`unblocks`が最大のtask（同数なら小さいID）から`blocks`のうち`unblocks`が最大のものを辿った鎖で、どのtaskも他を塞いでいなければ空。`--goal`は`tasks`・`candidates`・`critical`の起点をそのgoalに絞るだけで、数は全goalの未完了taskで数える（鎖はgoalの外へ出てよい）。循環（goal依存と所属を経由するものも）は依存の追加時に拒否済みなので前提にする。
- supervisorの`fill_slots`はclaimのたびに`dependency_graph`の`candidates`の順を作り、`claim_for_supervisor_in_order`に渡す。claimのトランザクションはその順で最初にまだclaim可能なtaskを取り、どれも取れなければ同じトランザクションで読み直した`graph_input`から`dependency_graph`で作ったclaim順の先頭を取る（`claim_order`）。SQLの`READY_QUERY`はID順のままで、順序はこの1つの関数が決める。順序は`graph`で再現できるので`claim_reordered`は記録しない。storeの`claim`（lease無し）も同じclaim順。
- canceled、失敗、中断、統合待ち、着地中、セッション待ちは依存の完了条件を満たさない。awaiting_integration / integrating / needs_sessionのTaskはin_progressのまま保持し、integratedになった時点でcompletedになる。
- `RunEvent.run_id`はtask登録・依存変更などrun作成前のイベントではnullになる。goal単位のイベント（`goal_created`、`goal_updated`、`goal_closed`）は`task_id`がnullで`goal_id`を持ち、`goal show`に並ぶ。`task_goal_changed`はtaskのイベントで、`from`と`to`にgoal IDを持つ。goal依存の追加・削除はtaskのイベント`goal_dependency_added` / `goal_dependency_removed`（`goal_id`）で、task依存の`dependency_added` / `dependency_removed`（`predecessor_id`）とは別のkindにする。
- `goal add`でgoalを作り、`add --goal ID`と`set-goal TASK GOAL`でtaskを所属させ、`set-goal TASK --none`で外す。所属の変更は依存の追加・削除と同じくdraft/readyのtaskだけに許し、閉じたgoalへの追加と付け替えは拒否する。`task_created`のpayloadは`goal_id`を持ち、`set-goal`は変化があったときだけ`task_goal_changed`を記録する。
- `goal edit`はtitle、description、acceptance、constraints、docを差し替え、`goal_updated`のpayloadに`old`と`new`のgoal全体を残す。閉じたgoalも編集できる（記録の訂正のため）。走行中のrunは`prompt.txt`のスナップショットのままで、次のclaimから新しい記述が使われる。
- `edit TASK`はdraftのtaskのtitle、description、acceptance、verification_commands（`--verify`、繰り返しで配列全体を置き換え、`--no-verify`で空）、required_evidence（`--evidence` / `--no-evidence`）、paths（`--paths` / `--no-paths`）、contextを差し替える（[ADR-0041](../adr/0041-on-demand-planners-proposals-submitted-and-plan-review-job.md)の決定9。titleはgoal 29のtaskで足した。入力は`TaskEdit`、判断は`task::edit`）。与えた値は`add`と同じ規則で検査し（空白のtitleとverification command、不正なglobを拒否。required_evidenceとpathsは重複を除く）、編集できるかは`TaskStatus::content_editable`が決める。今はdraftだけで、それ以外のstatusは`TaskContentNotEditable`（`task N is ready; only a draft task can be edited`）で拒否する。submittedが入ったらこの関数に足す（readyのtaskはsubmittedに戻してから直す。決定14）。値が変わったフィールドだけを`task_edited`の`from`と`to`（task JSONのキー名）に残し、何も変わらなければ記録しない。`show`のcompactな表示は`from` / `to`がobjectのとき、その中の文字列も300文字で切る（`view::event_gist`）。priority・goal・依存はそれぞれ`set-priority`・`set-goal`・`dependency`で変える。走行中のrunは`prompt.txt`のスナップショットのままになる。
- `goal close ID --verdict achieved|abandoned`はverdictを1回だけ記録する。`achieved`は所属taskに`completed` / `canceled`以外があれば拒否し、`abandoned`は`in_progress`があれば拒否する（`GoalVerdict::allows`、判断の入口は`domain::goal::close`）。閉じたgoalを再び閉じることはできず、続きは新しいgoalに登録する。`goal_closed`のpayloadはverdictとclose時点のstatus別件数を持つ。
- `list`は既定で終端状態（`TaskStatus::is_terminal`）でないtaskを新しい順（ID降順）に最大20件、`{"tasks", "next", "total"}`で返す。要素はid/status/title/goal_id/dependencies/goal_dependencies（依存先goalのID昇順）/latest_run（最新runのidとstatus）に縮約し、`--full`で残りの全項目を足す。`next`は次ページの先頭task ID（`--before`に渡す。`--before ID`はID以下のtaskを返す）で、続きがなければnull。`total`はフィルタ後の件数。フィルタとページの条件はapplication層の`TaskQuery`、出力は`TaskPage` / `TaskListItem`で、domainの`Task`は変えない。
- `goal list`はgoalごとに`status`（`draft` | `open`）、`closed`、`verdict`、所属taskのstatus別件数（`TaskStatusCounts`）を返し、`goal show`はgoal（`status`を含む）、所属taskのid/title/status、そのgoalに依存している未完了task（`dependents`、id/title/status）、goalのイベントを返す。`show`は`dependencies`の隣に`goal_dependencies`（goal ID昇順）を返す。
- `goal add --draft`はdraftのgoalを作り、`goal ready ID`がdraftを`open`にして`goal_status_changed`（`from: draft`、`to: open`）を記録する。`goal ready`は閉じていないdraftにだけ許す（`domain::goal::ready`）。draftのgoalにも`add --goal`と`set-goal`でtaskを所属させられ、`goal close`もできる（採らない提案は`abandoned`で閉じる）。draftのgoalのtaskは`ready`にしても`candidates`・`graph`の`candidates`・supervisorのclaimに出ない（`READY_QUERY`がgoalの`status = 'draft'`を除く）。`graph`は所属goalのあるtaskに`goal_status`を添える。
- `note --task ID | --run RUN_ID | --goal ID --text TEXT [--kind SLUG]`はkind `observation`のrun_eventを1件書いて返す。`--task`はtaskのイベント、`--run`はそのrunのtaskとrunのイベント、`--goal`はgoal単位のイベントになる。`notes [--goal ID] [--task ID] [--since CURSOR] [--limit N]`はobservationだけを古い順に返し（`{"notes", "cursor"}`）、`--since`なしは直近`--limit`件（既定20）、`--since`ありはcursorより後の最初の`--limit`件。`--goal`はgoalのnoteに加えて所属taskとそのrunのnoteを、`--task`はtaskとそのrunのnoteを含む。`cursor`は最後のnoteのイベントID（空なら`--since`の値、それも無ければ0）。
- `DAGQ_ROLE=observer`の環境からは、CLIの入口（`main.rs`の`observer_access`）が許可の一覧にないコマンドを`{"error": "observer may not change queue state"}`で拒否する（ADR-0024の決定4）。許すのは読み取り（`locate` / `list` / `show` / `candidates` / `graph` / `status` / `events` / `watch` / `stats` / `doctor` / `goal list` / `goal show` / `notes`）、`note`、`goal add --draft`、そしてdraftで閉じていないgoalへの`add --goal`（taskはdraftで登録される）だけ。`ready` / `draft` / `cancel` / `integrate` / `recover` / `goal ready` / `goal close` / `goal edit` / 通常の`goal add` / goalなしかopenなgoalへの`add` / `dependency` / `set-goal` / `review` / `init` / `up` / `down`などは拒否する。一覧に載せない限り後から足したコマンドも拒否される。task 99で`ask --kind blocked`（observerが閾値超えをinboxに上げるask。taskにもrunにも紐づかなくてよい唯一のkind）を一覧に足した。ほかのkindの`ask`、`answer`、`ask close`、`observe`、`supervise`は拒否する。判定は`DAGQ_ROLE`の申告に依存する柵で、悪意ある実行は防がない。
- `show`と`goal show`の既定出力は`src/view.rs`が`TaskDetail` / `GoalDetail`から作る圧縮形で、全文は`--full`（ADR-0016の決定4）。キー名は全文と同じで、省くか切り詰めるだけ。長い文字列（taskの`description`/`acceptance`/`context`、goalの`title`/`description`/`acceptance`/`constraints`、runの`last_error`、eventの要点の値）は300文字で切って`…`を付け、それを持つobjectに`truncated: true`を足す。`show`は最新runの`id`/`status`/`branch`/`result_commit`/`last_error`/`worktree_path`/`workspace_id`と、あれば`last_error_code`（[理由の分類コード](#理由の分類コードcode)）だけを`runs`に1件、そのrunの`processes`、直近10件（`--events N`で変更）のイベントを`id`/`kind`/`created_at`、runに属するイベントなら`run_id`、payloadの`status`/`reason`/`last_error`/`from`/`to`/`code`だけで返し、pathは出さない。`goal show`はイベントを直近10件の`kind`/`created_at`だけにする。どちらも全件数を`runs_total` / `events_total`で添える。どちらも`observations`に、自分に紐づくnote（`show`はtaskとそのrun、`goal show`はgoal自身）の直近5件を`id`/`created_at`/`run_id`（あれば）/`text`（300文字で切る）/`kind`/`by`で古い順に添える。
- scheduling（`candidates`、`claim`）が見るgoalの性質は、draftのgoalのtaskを除くことと、goal依存の先のgoalが`achieved`で閉じたかだけ。supervisorのclaim順は効く優先度・解放数・IDで決まり（goalのrankはまだ無い）、goalをまたぐ依存も許す。

## IDとcommitのnewtype

IDとcommitはドメインプリミティブのnewtype（`src/domain/ids.rs`、`domain`から再公開。[ADR-0013](../adr/0013-layered-architecture-and-type-function-style.md)の決定4）で、task IDとgoal IDのように意味の違う値を型で区別する。内部のフィールドは非公開で、生成は下の入口だけ、値の取り出しは`as_i64` / `as_str` / `into_string`で行う。型エイリアスは使わない。

| 型 | 包む値 | 生成と検証 | trait | 使う場所 |
| --- | --- | --- | --- | --- |
| `TaskId` | `i64`（`tasks.id`） | `TaskId::new`（検証なし。正であることは`NewTask::validate`などが`NonPositiveId`で確かめる） | `Copy`、`Eq`、`Ord`、`Hash`、`Display` | `Task.id`、`TaskRun.task_id`、`GoalTask.id`、`RunEvent.task_id`、`Ask` / `NewAsk`の`task_id`、`NoteTarget::Task`、`NoteQuery.task_id`、`TaskDetail.dependencies`、`NewTask.dependencies`、`RegisteredFollowUp.task_id`、`Attention.task_id`、`stats`の`RunStats` / `Alert`、`TaskStore`の引数、`TaskQuery.before` / `TaskPage.next`、graphの型 |
| `GoalId` | `i64`（`goals.id`） | `GoalId::new`（検証なし） | `TaskId`と同じ | `Goal.id`、`GoalSummary.id`、`Task.goal_id`、`NewTask.goal_id`、`RunEvent.goal_id`、`NoteTarget::Goal`、`NoteQuery` / `StatsQuery` / `TaskQuery`の`goal_id`、`DomainError`のgoal ID、`TaskStore`の引数、observerの`ask_and_goal_high_water` / `written_by`のgoalの境界（その時点の最大のgoal ID。goalが無ければ`GoalId::new(0)`） |
| `AskId` | `i64`（`asks.id`） | `AskId::new`（検証なし） | `TaskId`と同じ | `Ask.id`、`AttentionNext`の`ask_id`（`answer ask N`などの表示）、`Attention.ask_id`、`TriageAction::Ask`、`AskStore`の`answer` / `close_ask` / `ask_delivered`、`RunStore`の`decide_triage` / `exhaust_resumes`、`asks`の`read_ask`、supervisorの`open_landing_ask` / `open_triage_ask`の戻り値、observerの`ask_and_goal_high_water` / `written_by`のaskの境界 |
| `EventId` | `i64`（`run_events.id`） | `EventId::new`（検証なし。0は最初のeventより前のcursor） | `TaskId`と同じ | `RunEvent.id`と、eventの順に読むcursor: `RunStore::latest_event_id`、`events_between` / `event_id_before` / `record_queue_event`、`NoteQuery.since` / `NotePage.cursor`、`StatsQuery.since` / `RunStats.finished_event_id` / `Stats.next_cursor`、`watch::events`の`after`と`WatchOptions.after`、`ObserveOptions.since`とobserverのcursorファイル、`written_by`のeventの境界 |
| `RunId` | `String`（UUID） | `RunId::new` / `TryFrom<String>` / `TryFrom<&str>`。空白だけの値は`Blank { field: "run ID" }` | `Clone`、`Eq`、`Ord`、`Hash`、`Display`、`AsRef<str>`、文字列との`PartialEq` | `TaskRun.id`、`RunEvent` / `Ask` / `NewAsk` / `Attention`の`run_id`、`RunLease.run_id`、`RunProcess.run_id`、`NoteTarget::Run`、`RunPaths::new`、`Receipt::check`の引数、`runtime_store` / `asks`のrun ID引数 |
| `CommitSha` | `String`（40桁か64桁の16進） | `CommitSha::parse(value, field)` / `TryFrom<String>` / `TryFrom<&str>`（field名`commit`）。外れれば`InvalidCommit { field }` | `RunId`と同じ | `TaskRun.base_commit` / `result_commit`、`IntegrationOutcome::NeedsSession.main`、`Validation.result_commit`、`Landing`の`commit` / `source_commit` / `main_before`、`TaskStore::claim`と`claim_for_supervisor`・`begin_integration`・`begin_resume`・`skip_resume`の引数、`GitRepository`の`main_head` / `head` / `merge_base` / `commit_tree`の戻り値 |

- serdeでは`#[serde(transparent)]`で素の値として出るので、CLIのJSON出力は変わらない。`RunId`と`CommitSha`の`Deserialize`は生成と同じ検証を通す。
- SQLiteとの変換（`ToSql` / `FromSql`）はinfrastructure（`src/infrastructure/sql_ids.rs`）にある。bindは素の値で、読み出しは生成と同じ検証を通すので、空のrun IDや不正なcommitを持つ行は変換エラーになる。`params_from_iter`に渡す`Value`だけは`as_i64()`で素の値にする。
- CLIの引数はclapでは`i64` / `String`のまま受け、`main.rs`がnewtypeに変えてからapplicationに渡す。`--run`に空白だけを渡すと`run ID must not be blank`になる。
- 例外: `Receipt`の`run_id`と`commit`はagentが書くファイルの形のまま`String`で持つ。`Receipt::check`が決まった順で検証し（run_idの一致、result、各check、commitの形式）、最初に外れた項目のエラー文を`last_error`に書くため、パースの時点では検証しない。`GitRepository`の`rebase` / `is_ancestor` / `diff_*` / `changed_paths` / `tree_of`などの引数は`main`やref、`<commit>^{tree}`も受けるGitのrevisionなので`&str`のまま。cmuxの`workspace_id`は対象外。`supervisor_token`（supervisorとintegrateがleaseと統合枠を持つときのtoken）は`&str`のまま（task 171の判断）: `IdGenerator`が1プロセスに1度作る不透明なUUIDで、中身の制約も表示上の意味も無く、`runtime_store`の約240箇所と`application`の約150箇所を通るだけで、隣の`&RunId`とはすでに型で区別されている。取り違えうるのは同じ`&str`の`message` / `workspace`（`abandon_run`、`cleanup_failed`、`workspace_created`）で、newtypeにする利益はそこに限られるので、run storeの他の変更と衝突しない時期に別taskで判断する。
- askとeventのIDは`AskId` / `EventId`（task 171）。CLIの`--since` / `--after` / askの`ID`はclapでは`i64`で受け、`main.rs`が変える。payloadのJSON（`ask_id`）から読むときは`as_i64()`の値を`AskId::new`で包む。

## 集約: TaskとGoal

`src/domain/`はファイルを観点ごとに分けたディレクトリモジュールで、`mod.rs`が従来の`pub use`を持つので`crate::domain::Task`などのパスは変わらない（[ADR-0013](../adr/0013-layered-architecture-and-type-function-style.md)の決定3、5）。

| ファイル | 中身 |
| --- | --- |
| `mod.rs` | `string_enum!`のenum（`TaskStatus`、`GoalStatus`、`GoalVerdict`など）、review / triageのverdict、ask、note、push、attention |
| `error.rs` | `DomainError`と`require` |
| `ids.rs` | `TaskId`・`GoalId`・`RunId`・`CommitSha` |
| `task.rs` | 集約`Task`、`TaskAction`、`TaskStatus`の遷移規則、taskのコマンドとクエリ |
| `goal.rs` | 集約`Goal`、`GoalVerdict::allows`、goalのコマンドとクエリ |
| `input.rs` | 入力型`NewTask`・`NewGoal`・`GoalEdit`・`RunPlan`と、復元用の`TaskRecord`・`GoalRecord`・`RunRecord`（公開フィールドのplain data） |
| `run.rs` | 集約`TaskRun`とrunのコマンドとクエリ（[集約: TaskRun](#集約-taskrun)） |
| `views.rs` | 集約でない読み取り用の型（`TaskDetail`、`GoalSummary`、`GoalDetail`、`GoalTask`、`Predecessor`、`GoalPredecessor`、`RunEvent`、`RunProcess`、`RunLease`、`SupervisorRegistration`、`TaskStatusCounts`）、`ClaimOutcome`・`IntegrationOutcome`・`RegisteredFollowUp`、`Receipt`、runのファイル配置`RunPaths` |
| `scope.rs`、`stats.rs` | pathのglob、`stats`の集計（従来どおり） |

`Task`と`Goal`のフィールドは非公開で、`task.rs` / `goal.rs`の外からは下の関数でしか作れず、変えられない。

- 新規作成: `Task::new(id, NewTask, created_at)`は`NewTask::validate`（作成時のルール）を通し、statusを`draft`、`required_evidence`とpathsを重複除去、`updated_at`を`created_at`にする。`NewTask.dependencies`はtaskの外の辺なのでstoreが別に保存する。`Goal::new(id, NewGoal, created_at)`は`NewGoal::validate`を通し、`draft`なら`draft`、それ以外は`open`、未close、空白の`doc`はnullにする。IDはstoreが採番し（`AUTOINCREMENT`の次の値）、`created_at`はDBの現在時刻を渡す。
- 復元: `Task::restore(TaskRecord)`と`Goal::restore(GoalRecord)`は保存済みの状態をそのまま再現し、作成時のルールは再適用しない。検証するのは保存済みの行が必ず満たすことだけ: 正のID（`NonPositiveId`の`task ID` / `goal ID`）、空白でないtitle、taskの`goal_id`が正、goalの`closed_at`と`verdict`が同時にnullか同時に非null（`GoalCloseInconsistent`）。infrastructureの`task_row` / `goal_row`がrowからrecordを組み、復元の拒否は`rusqlite`の変換エラーの原因として包む。
- `Serialize`は残し、フィールド名と順序は従来と同じなのでCLIのJSON出力は変わらない。`Deserialize`は集約から外した（`TaskDetail`・`GoalDetail`・`Predecessor`・`IntegrationOutcome`も集約を含むので外した）。入力型の`NewTask` / `NewGoal` / `GoalEdit`は`Deserialize`を持つ。
- taskのコマンド（所有権を受け取り、成功時に更新後のtaskを返す）: `task::transition(task, action, unfinished_run)`（遷移の判断は`TaskStatus::transition`）、`task::set_goal(task, goal_id)`（draft / readyのtaskだけ。移し先のgoalが開いているかは`goal::check_accepts_tasks`）、`task::set_paths(task, paths)`（globを検査し重複を除く。draft / readyだけ）。依存の辺はtaskの外なので判断だけを持つ: `task::check_not_self`（自己依存）、`task::check_dependencies_editable`（draft / readyだけ）、`task::check_acyclic(task_id, predecessor_id, creates_cycle)`（循環はSQLの再帰CTEが全依存グラフから見つけ、その結果を渡す）。goal依存（ADR-0038）も同じ形で判断だけを持つ: `task::check_not_own_goal(&task, goal_id)`（自分の属するgoalへの依存）、`task::check_goal_acyclic(task_id, goal_id, creates_cycle)`（`dependency add --goal`と`add --depends-on-goal`）、`task::check_membership_acyclic(&task, goal_id, depends_on_goal, creates_cycle)`（`set-goal`で移し先のgoalにtaskが直接依存していれば自分のgoalへの依存、間接に待っていれば循環）。循環の検出はstoreの`waits_for`が、task→predecessor、task→依存先goal、goal→所属taskの3種の辺を合わせた再帰CTEで行い、task依存の`check_acyclic`もこのグラフで判定する。`add`はtaskを所属goalごと保存してから辺を1本ずつ同じ検査で足すので、`add --goal G --depends-on-goal H`も同じ規則で拒否される（`NewTask::validate`は`G = H`を先に`OwnGoalDependency`で拒否する）。
- goalのコマンド: `goal::edit(goal, GoalEdit)`、`goal::ready(goal)`、`goal::close(goal, verdict, &counts, closed_at)`（閉じたgoalは閉じられない、verdictが所属taskのstatusを許すか。成功すると`closed_at`・`verdict`・`updated_at`を設定する）。クエリは`goal::check_accepts_tasks(&goal)`、`Goal::is_closed` / `is_draft`。
- 値の取り出しは`Task`・`Goal`のメソッド（`id()`、`title()`、`status()`、`goal_id()`、`paths()`、`verdict()`など。文字列と配列は借用で返す）と`task::dependencies_editable(&task)`。`into_title()`はtitleだけを所有権ごと取り出す。
- `updated_at`はDBの`strftime(...,'now')`が更新のたびに書き（schemaの一部）、storeは保存後に読み直して返す。`goal::close`だけは`closed_at`と`updated_at`を同じ値にするため時刻を引数に取る。

## 集約: TaskRun

`TaskRun`（`src/domain/run.rs`）のフィールドは非公開で、`run.rs`の外からは下の関数でしか作れず、変えられない。状態遷移の判断（どのstatusからどのstatusへ移れるか）は全部ここにあり、`runtime_store.rs`と`sqlite.rs`のSQLは判断をしない。

- 新規作成: `TaskRun::new(id, &task, &base_commit, provider, claimed_at)`はclaimの初期状態を作る。`task`は`task::claim`で`in_progress`にしたtaskで、そうでなければ`RunOfUnclaimedTask`。statusは`claimed`、`requested_provider`と`actual_provider`は`provider`、`base_commit`は小文字にし、path・workspace・結果・`last_error`はnull、`created_at`は`claimed_at`。run IDとclaim時刻はstoreが渡す（IDは`Uuid::new_v4`、時刻はDBの`strftime`。注入は次のタスク）。
- 復元: `TaskRun::restore(RunRecord)`は保存済みの行をそのまま再現する。古いversionのqueueには今の規則では生じない行（workspaceの無いclose時刻など）が残るので、検証するのはどのversionでも守られてきたことだけ: `integrated`のrunは`result_commit`を持つ（外れれば`RunInconsistent`）。infrastructureの`stored_run_row`がrowからrecordを組み、復元の拒否は`rusqlite`の変換エラーの原因として包む。読み出しの`run_row`はその後に`relocated`でpathを今の`runs/`から解決し直す。
- `Serialize`は残し、フィールド名と順序は従来と同じなのでCLIのJSON出力は変わらない。`Deserialize`は外した（`ClaimOutcome`も`TaskRun`を含むので外した）。
- 値の取り出しは`id()`、`task_id()`、`status()`、`base_commit()`、`branch()`・`worktree_path()`・`run_dir()`・`last_error()`など（文字列は`Option<&str>`、commitは`Option<&CommitSha>`で借用して返す）、`relocated(runs_dir)`、`idle_marker_path()`。

コマンドは`fn command(run: TaskRun, ...) -> Result<TaskRun, DomainError>`の形で、許されない遷移は`RunTransitionNotAllowed { status, operation }`（見つけたstatusと操作名）で拒否する。

| コマンド | 許すstatus | 結果 |
| --- | --- | --- |
| `start_provisioning(run, &RunPlan)` | `claimed` | `starting`。`repo_path`・`run_dir`・`branch`・`worktree_path`・`receipt_path`・`log_path`を設定 |
| `attach_workspace(run, workspace_id)` | `starting`でworkspace未設定 | `workspace_id`を設定 |
| `mark_running(run)` | `starting` | `running` |
| `finish_session(run, exit_code)` | `starting` / `running` | 0か`None`（sessionを開いたまま、ADR-0027）なら`validating`、非0なら`failed`と`session exited with code N` |
| `accept(run, result_commit)` | `validating` | `awaiting_integration`と`result_commit` |
| `reject(run, result_commit, reason, resumable)` | `validating` | `resumable`（evidence不足かscope違反だけ）なら`needs_session`、ほかは`failed`。`result_commit`は検証済みのcommitかnull、`reason`があれば`last_error` |
| `restart_validation(run)` | `awaiting_integration` | `validating`（`revise`の後、ADR-0027） |
| `decide_landing(run, to, reason)` | `awaiting_integration`、`to`は`needs_session` / `failed` | `to`と`last_error` |
| `abandon(run, message)` | どれでも | `last_error`だけ（runtime error。statusは変えない） |
| `interrupt(run)` | `claimed` / `starting` / `running` / `validating` / `integrating`（`run::UNFINISHED`） | `interrupted`。`integrating`は`awaiting_integration` |
| `begin_integration(run)` | `awaiting_integration` / `needs_session` | `integrating` |
| `defer_integration(run, reason)` | `integrating` | `needs_session`と`last_error` |
| `fail_integration(run, reason)` | `integrating` | `failed`と`last_error` |
| `abort_integration(run, revert_to, message)` | `integrating`、`revert_to`は`awaiting_integration` / `needs_session` | `revert_to`と`last_error` |
| `finish_integration(run, commit)` | `integrating` | `integrated`、`result_commit`、`last_error`はnull |
| `skip_resume(run, approved)` | `needs_session` | 承認済みはそのまま、ほかは`validating` |
| `finish_resume(run, to, reason)` | `needs_session`、`to`は`validating` / `awaiting_integration` / `failed` | `to`、`reason`があれば`last_error` |
| `exhaust_resumes(run, reason)` | `needs_session` | `failed`と`last_error`（ADR-0024） |
| `resume_after_triage(run, instruction)` | `failed` / `interrupted` | `needs_session`と`last_error` |
| `workspace_closed(run, closed_at)` | `awaiting_integration` / `needs_session`で、workspaceがあり未close | `workspace_closed_at` |
| `triage_closed_workspace(run, workspace_id, closed_at)` | どれでも | runのworkspaceで未closeのときだけ`workspace_closed_at` |
| `record_close_failure(run, message)` | `awaiting_integration` / `needs_session`で未close | `last_error`（close失敗） |
| `record_cleanup_failure(run, message)` | どれでも | `last_error`（着地後のworktree / branch削除の失敗） |

クエリは`run::check_ready_for_wrapper(&run)`（`starting`でworkspaceあり）、`run::check_resumable(&run)`（`needs_session`）、`run::check_triageable(&run)`（`failed` / `interrupted`）。

```text
claimed ──start_provisioning──▶ starting ──mark_running──▶ running
   │                               │ finish_session            │ finish_session
   │                               ▼                           ▼
   │                           validating ◀──────────────────────
   │                  accept │    │ reject            ▲ restart_validation / skip_resume / finish_resume
   │                         ▼    ▼                   │
   │        awaiting_integration  failed / needs_session
   │           │  ▲   decide_landing ─▶ needs_session / failed
   │ begin_    │  │ interrupt / abort_integration
   │ integration▼ │
   │         integrating ──finish_integration──▶ integrated
   │               ├──defer_integration──▶ needs_session ──begin_integration──▶ integrating
   │               └──fail_integration───▶ failed
   └──interrupt（claimed / starting / running / validating）──▶ interrupted
failed / interrupted ──resume_after_triage──▶ needs_session ──exhaust_resumes──▶ failed
```

storeの保存は「`stored_run`で読む → domainのコマンド → `save_run`で書く」の形で、[persistence](persistence.md#集約の読み書き)にある。

## DomainError

domainの関数は業務上の拒否を`DomainError`（`src/domain/error.rs`）で返す。`std::error::Error`と`Display`を実装し、`anyhow`・`rusqlite`などI/OやDBのライブラリには依存しない。I/Oを行うapplication / infrastructure / runtimeは境界で`?`により`anyhow::Error`へ変換し、原因の説明が要る場所だけ`context`を足す。`Display`はCLIが`{"error": ...}`に出す文、runtimeが`last_error`に書く文そのもので、`DomainError`の導入前の文字列と一致する。variantは業務上の拒否だけで、汎用の`Other(String)`は持たない。

| variant | 返す関数 | 持つ情報 | `Display` |
| --- | --- | --- | --- |
| `UnknownValue` | `string_enum!`の`FromStr`（`TaskStatus`、`RunStatus`、`Provider`、`SupervisorMode`、`SessionRole`、`GoalStatus`、`GoalVerdict`、`ReceiptResult`、`CheckStatus`） | enum名、値 | `unknown <Enum>: <value>` |
| `TaskHasUnfinishedRun` | `TaskStatus::transition` | action | `task has an unfinished run; recover or integrate it before applying <Action>` |
| `TransitionNotAllowed` | `TaskStatus::transition` | 現在のstatus、action | `cannot apply <Action> to task in <status> state` |
| `Blank` | `NewTask::validate`、`NewGoal::validate`、`goal::edit`、`Task::restore`、`Goal::restore`、`NewNote::validate`、`RunId::new` | field名（`task title`、`verification commands`、`goal title`、`note text`、`run ID`） | `<field> must not be blank` |
| `NonPositiveId` | `NewTask::validate`、`Task::new` / `restore`、`Goal::new` / `restore` | field名（`dependency IDs`、`goal dependency IDs`、`goal ID`、`task ID`） | `<field> must be positive` |
| `GoalAlreadyClosed` | `goal::close`、`goal::ready` | goal ID、記録済みのverdict | `goal <id> is already closed as <verdict>` |
| `GoalCloseBlocked` | `goal::close` | goal ID、verdict、verdictを許さないstatusと件数 | `goal <id> cannot be closed as <verdict>: <n> task(s) <status>, ...` |
| `MalformedReceipt` | `Receipt::parse` | パーサーの理由 | `receipt is not a valid completion receipt: <reason>` |
| `ReceiptRunMismatch` | `Receipt::check` | receiptのrun_id、runのID | `receipt run_id <a> does not match run <b>` |
| `AgentReportedResult` | `Receipt::check` | result、summary | `agent reported result <result>: <summary>` |
| `ReceiptCheckFailed` | `Receipt::check` | check名、evidence_or_reason | `receipt reports <check> as failed: <evidence>` |
| `ReceiptCheckUnexplained` | `Receipt::check` | check名、status | `receipt <check> is <status> without evidence or reason` |
| `InvalidCommit` | `Receipt::check`、`CommitSha::parse` / `TryFrom` | field名（`receipt commit`、`base commit`、`commit`、Gitの出力なら`HEAD`・`main commit`など） | `<field>: must be a full 40- or 64-character hexadecimal Git object ID` |
| `FollowUpsNotArray` | `Receipt::check` | なし | `receipt follow_ups must be an array` |
| `MissingRunDirectory` | `TaskRun::idle_marker_path` | なし | `missing run directory` |
| `GoalNotDraft` | `goal::ready` | goal ID | `goal <id> is not a draft` |
| `GoalClosed` | `goal::check_accepts_tasks`（`add --goal`と`set-goal`） | goal ID、記録済みのverdict | `goal <id> is closed as <verdict>; create a new goal for further work` |
| `GoalCloseInconsistent` | `Goal::restore` | goal ID | `goal <id> has a close time without a verdict or a verdict without a close time` |
| `TaskNotEditable` | `task::set_goal`、`task::set_paths`、`task::check_dependencies_editable` | 変える対象（`the goal`、`the paths`、`dependencies`） | `<what> can only be changed for draft or ready tasks` |
| `SelfDependency` | `task::check_not_self` | なし | `a task cannot depend on itself` |
| `DependencyCycle` | `task::check_acyclic` | task ID、predecessor ID | `dependency <task> -> <predecessor> would create a cycle` |
| `OwnGoalDependency` | `task::check_not_own_goal`、`task::check_membership_acyclic`、`NewTask::validate` | goal ID | `a task cannot depend on its own goal <goal>; the goal already waits for it` |
| `GoalDependencyCycle` | `task::check_goal_acyclic` | task ID、goal ID | `dependency <task> -> goal <goal> would create a cycle` |
| `GoalMembershipCycle` | `task::check_membership_acyclic` | task ID、goal ID | `moving task <task> to goal <goal> would create a cycle: the task already waits for the goal` |
| `TaskNotClaimable` | `task::claim` | task ID、status | `task <id> is <status>, not ready` |
| `RunOfUnclaimedTask` | `TaskRun::new` | task ID、status | `task <id> is <status>; a run starts only for a claimed task` |
| `RunTransitionNotAllowed` | `run`のコマンドとクエリ | 見つけたstatus、操作名 | `cannot <operation> a run in <status> state` |
| `RunInconsistent` | `TaskRun::restore` | run ID、理由 | `run <id> <reason>` |
| `InvalidNoteKind` | `NewNote::validate` | kind | `note kind "<kind>" must be a slug of lowercase letters, digits, '-' and '_'` |

`goal close`の判断（閉じたgoalは閉じられない、verdictが所属taskのstatusを許すか）は`goal::close`が持ち、`SqliteQueue::close_goal`はgoalとstatus別件数を読んで渡し、返ったgoalを書くだけである。taskの手動遷移、goalの付け替え、pathsと依存の変更、閉じたgoalへの追加も同じく、`sqlite.rs`は読んでdomainの関数に渡し、その結果を保存する（`ensure!`で業務上の拒否を決める箇所は残っていない）。runの遷移も同じく`runtime_store.rs`は読んで`run`のコマンドに渡し、その結果を保存する。ただしrunのコマンドの拒否（`RunTransitionNotAllowed`）はCLIの`{"error"}`と`last_error`に出る文を変えないため、storeが操作ごとの従来の文（`run is not owned by this supervisor`、`run <id> is not integrating`など）に置き換えて返す。置き換えたdomainの拒否の理由はrun directoryの`refusals.log`に残す（[persistence](persistence.md#集約の読み書き)）。各エラー文はdomainの単体テストで固定している。DBに保存された文字列が既知のenum値でないときは、`enum_col`が`UnknownValue`を`rusqlite`の変換エラーの原因として包む。

## Invariants

- Taskは自分自身に依存できない。
- Taskは高々1つのgoalに属し、閉じたgoalには属せるtaskが増えない。goalのverdictは1回だけ記録され、`closed_at`と`verdict`は同時にnullか同時に非nullである。
- 依存グラフは循環しない。グラフはtask依存の辺、goal依存の辺、goalから所属taskへの暗黙の辺を合わせたもの（ADR-0038）。
- Taskは自分の属するgoalに依存できない。
- `in_progress`はschedulerがclaimしたTaskだけが持つ。
- TaskRunが成功するには完了レシート、base commitの上に積まれたbranch headのコミット、clean worktreeが必要で、Taskが`completed`になるにはさらに`integrate`がrebase後に1回だけ実行する検証コマンドの成功が必要（ADR-0040の決定1）。receiptの自己申告だけでは成功しない。
- Taskが`completed`になるのは、その`integrated` runを`integrate`がmainへ着地させたときだけ。着地commitのtreeは再検証したworktreeのtreeに等しく、messageは`Dagq-Task` / `Dagq-Run` trailerでrunに結び付く。`integrated` runはTaskごとに1件、`integrating` runはqueueごとに1件。
- mainはtaskごとに1つのsquash commitの直線で、merge commitとrun branchのfast-forwardは作らない。runの詳細履歴は`refs/dagq/runs/<run-id>`に残る。
- `needs_session`のrunはsupervisorがresumeする（ADR-0019の決定1、[supervisor-lifecycle](supervisor-lifecycle.md#needs_session)）。解消・検証コマンドの再実行・receiptの書き直しはresumeしたセッションが行う。resume中もrunは`needs_session`のままで、進行は`resume_started` / `resume_finished`で表す（新しい状態は足さない）。解消したrunは、`integration_approved`があれば（`integrate`が呼ばれていれば）supervisorが`integrate`と同じ手順で着地させ、無ければ`needs_session → validating`にしてresumeしたsessionを開いたままreviewに進める（ADR-0027の決定3）。`failed` receiptなら`needs_session → failed`。どちらもsupervisorのleaseの下で行う（`finish_resume`）。3回の試行で解消しなければ、supervisorが`needs_session → failed`にしてtriageの`decide`のask（`retry` / `cancel`）で人に渡す（`exhaust_resumes`。`triage_finished`を`by: runtime`で記録するので、headlessのtriageは走らない。task 100）。`run_attention`はどの`needs_session`も`resuming (runtime)`。
- `awaiting_integration`のrunは、supervisorがleaseを持つ間はheadless reviewの途中で、新しい状態は足さず`review_started` / `review_finished` / `revise_requested` / `revise_finished` / `conflict_precheck` / `conflict_resolved` / `review_failed`で進行を表す（[ADR-0027](../adr/0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)、[supervisor-lifecycle](supervisor-lifecycle.md#review-supervisor)）。`revise`の書き直しと、passの後の`git merge-tree`の事前判定で見つかったmainとの衝突を生きているsessionが解消した書き直し（決定4）は、`awaiting_integration → validating`に戻して照合し直す。reviewの`concern`の`approve_landing`に`send_back`か`cancel`と答えると、leaseの無い`awaiting_integration`のrunが`needs_session`か`failed`になる（`landing_decided`）。runのsessionは`/exit`とworkspaceのcloseまで、verdictが出るまで開いたまま。reviewの verdict は`ReviewVerdict`（`pass | revise | concern`、`reasons`、`summary`）で、reviseはrunごとに`MAX_REVISE_ATTEMPTS`（2）回まで。
- workspaceを閉じる前にTaskRunをcleanedにしない。閉じたことをcmuxの応答で確認して`workspace_closed_at`に記録するまでは開いている扱いで、close失敗はrun状態を変えない。
- agentの異常終了だけでTaskを自動再実行しない。`failed` / `interrupted`のrunの再試行はsupervisorのtriageのverdict（`retry` / `resume` / `ask`）か人の操作で決まり、同じtaskの`failed` / `interrupted`のrunが2件以上（`TRIAGE_RETRY_FAILURES`）ならtriageはretryせずaskにする。leaseが無くsessionの止まった孤児runの`recover`はsupervisorが自動で行うが、taskを`ready`にはせずtriageに回す（ADR-0024の決定3、[supervisor-lifecycle](supervisor-lifecycle.md#triage-supervisor)）。triageの進行は新しい状態を足さず`triage_started` / `triage_finished` / `triage_failed` / `triage_decided`で表し、`domain::triage_state`が最後の`resume_started`以降のそれらから`Pending` / `Failed` / `Finished`を導く。
- 実装途中のprovider fallbackは行わず、起動不能など安全に判定できる場合だけfallbackする。
- 1 runの失敗・中断・復旧は他のrunの状態、lease、プロセス、リソースを変えない。

### `TaskRun.last_error`

`last_error`は「そのrunを止めた、または人の確認が要る最新の理由」を1つだけ持つ。書き込みは後のものが前のものを上書きし、着地（`integrated`）でnullになる。理由の全履歴は`run_events`が持つ。書くのは次の場面で、それぞれ対応するイベントと組で残る。

| 場面 | status | 書く文 | イベント |
| --- | --- | --- | --- |
| 非0終了 | `starting`/`running` → `failed` | `session exited with code N`（Nはwrapperが報告した終了コード。signalで終わったセッションは128） | `supervision_finished` |
| receipt検証の拒否 | `validating` → `failed` | 最初に外れた項目の理由をそのまま。`receipt was not submitted at <path>`、receiptの構造や`run_id`の不一致のエラー文、`worktree is on <ref> instead of refs/heads/<branch>`、`receipt commit <sha> is not the head of <branch> (<head>)`、`no commit was made on top of base <sha>`、`commit <sha> does not descend from base <sha>`、`worktree is not clean:` に続く`git status`（検証コマンドはvalidatingでは実行しない。ADR-0040の決定1） | `validation_finished` |
| runtime error / provisioning error | 変えない | errorの文をそのまま。worktree・workspaceの作成失敗は`run <run-id> provisioning failed: <error>`、監視中は`wrapper heartbeat expired; session may still be alive`、検証処理そのもの（Git・DB）のerror文。supervisorはそのrunのleaseを消して手放す（abandon）。wrapper自身のerrorとsupervisor heartbeatの失敗も同じ列に書くがleaseは残す（wrapperは子が死んでいれば続けて終了コード127を報告し、runは`session exited with code 127`で`failed`になる） | `runtime_error` |
| cleanup失敗 | 変えない | 受け入れたrunのworkspace closeの失敗は`workspace <workspace-id> could not be closed: <error>`、着地後のworktree/branch削除の失敗は`landed worktree <path> could not be removed: <error>` | `cleanup_failed` |
| 着地の保留・中断・失敗 | `integrating` → `needs_session` / 元のstatus / `failed` | rebaseの衝突や再検証の失敗の理由（検証コマンドの失敗は`verification command "<cmd>" exited with <code> after the rebase onto <main>; see <run-dir>/integrate-<attempt>-verify-N.log`）、mainを進める前のGit/DB errorは`integration stopped before main moved: <error>`、セッションが書き直した`failed` receiptの理由（[supervisor-lifecycle](supervisor-lifecycle.md#integrate)） | `integration_deferred` / `integration_error` / `integration_failed` |

`last_error`はstatusと最後のイベントに合わせて読む。`failed`なら非0終了・検証拒否・着地時の`failed` receipt、`claimed`/`starting`/`running`/`validating`で`last_error`があればsupervisorが手放したrun（`doctor`にleaseなしで出る）、`awaiting_integration`で`last_error`があればclose失敗（`cleanup_failed`、`workspace_closed_at`はnull）かmainを進める前に止まった着地（`integration_error`）、`needs_session`なら着地の衝突である。`show`・`doctor`・superviseの結果の`errors`に出て、`list`には出ない。`recover`は`last_error`を上書きしない。

### 理由の分類コード（`code`）

[ADR-0034](../adr/0034-domain-events-carry-reason-codes-actor-and-configuration-changes.md)の決定1（task 195）。失敗・保留・中断を記録するイベントは、payloadに`code`（`domain::ReasonCode`、snake_caseの閉じた集合）と、コードごとの構造化した値を持つ。今の`reason` / `message` / `error` / `last_error`の自由文は項目も文言も変えずに残す。コードは「なぜ」で、「どの工程で」はイベントのkindが持つ（同じ`backend_timeout`が`runtime_error`にも`screen_capture_failed`にも付く）。足す値にpath・workspace ID・pidなどマシン依存の値は入れない（[ADR-0032](../adr/0032-classify-records-into-domain-events-diagnostics-coordination-and-bodies.md)。既存の項目の`workspace_id`などはそのまま）。schemaは変えず、`task_runs`に列は足さない。コードが入る前のイベントには`code`が無く、読む側はそれを許す。コードの一覧は`ReasonCode::ALL`と`meaning()`が正で、名前を変えるにはADRが要る。

| code | 意味 | 一緒に入る値 |
| --- | --- | --- |
| `session_exit_code` | sessionが自分の非0の終了コード（1–127）で終わった | `exit_code` |
| `session_killed` | sessionがsignalで終わった（wrapperがsignalのときに報告する128、またはshellの128+N。143はSIGTERM） | `exit_code`、128+Nなら`signal`（N） |
| `exit_timeout` | `/exit`の後、exit timeout内にsessionが終わらなかった | （`timeout_secs`は既存） |
| `heartbeat_lost` | wrapperのheartbeatが途絶え、processは生きている | （`heartbeat_age_secs`は既存） |
| `wrapper_failed` | session wrapperがagentを動かせなかった | |
| `lease_lost` | supervisorのheartbeatが失敗し、leaseを保てなかった | |
| `receipt_missing` | receiptが書かれていない | |
| `receipt_invalid` | receiptが読めない・別のrunのもの・checkの説明が無い・commitの形式やfollow_upsが不正 | |
| `worker_failed` | receiptがrunを`failed`と報告した | |
| `evidence_failed` | taskが要求していないcheckをreceiptが`failed`と報告した | |
| `evidence_missing` | taskが要求するcheckをreceiptが裏付けていない | （`checks` / `evidence_missing`は既存） |
| `commit_mismatch` | receiptのcommitがrun branchのbaseの上の新しいheadでない（別のbranch、headでない、commitが無い、baseから辿れない） | |
| `worktree_dirty` | worktreeにcommitされていない変更がある | |
| `scope_violation` | 差分がtaskの`--paths`の外を変えた | （`paths` / `scope_violation`は既存） |
| `rebase_conflict` | runがmainと衝突した（着地のrebase、passの後の`git merge-tree`の事前判定） | （`conflicts`は既存） |
| `rebase_empty` | rebaseの後にmainの上にcommitが残らない | |
| `rebase_in_progress` | worktreeに途中のrebaseが残っていたので中止した | |
| `verification_failed` | rebaseの後の検証コマンドが非0で終わった | `index`（1始まり）、（`command` / `exit_code`は既存） |
| `backend_timeout` | cmuxの呼び出しがtimeoutした（adapterの`did not finish within`、cmuxの`Command timed out`） | `op`（`backend_call_failed`は既存の`op`） |
| `backend_failed` | cmuxの呼び出しが失敗した | `op` |
| `job_failed` | headlessのreviewかtriageのjobが失敗した | |
| `sent_back` | 人がreviewのconcernをsessionに差し戻した | |
| `cancelled` | 人が着地をcancelした | |
| `triage_resume` | triage（かその`decide`のaskへの人の回答）がrunをsessionに戻した | |
| `resume_exhausted` | 最後のresumeの後もsessionが要る | |
| `orphaned` | runの登録processが死んでいて`recover`された | |
| `push_failed` | 着地したmainのpushが失敗した | |
| `git_failed` | runtimeのGitコマンドが失敗した（着地したworktreeの削除） | |
| `other` | どれにも当たらない。自由文が理由を持つ。増えたらコードを足す | |

経路とコードの対応（`backend_*`は`application::reason_of_error`がerrorの連鎖から`RecordingBackend`の包んだcmuxの失敗（`BackendFailure`）を見つけたときで、`op`を持つ。見つからなければ表のfallback）:

| イベント | 経路 | code |
| --- | --- | --- |
| `supervision_finished` | sessionの非0終了（`last_error`は`session exited with code N`） | `session_exit_code` / `session_killed`。0終了とliveの受け渡しは持たない |
| `validation_finished` | receiptの照合の拒否（`last_error`） | `receipt_missing` / `receipt_invalid` / `worker_failed` / `evidence_failed` / `commit_mismatch` / `worktree_dirty` / `scope_violation` / `evidence_missing`。受理は持たない |
| `scope_violation` / `evidence_missing` | validationの保留に添えるイベント | `scope_violation` / `evidence_missing`（`validation_finished`と同じコードなので`stats`の`reason_codes`は数えない。`backend_call_failed`も失敗した工程のイベントと重なるので数えない） |
| `integration_deferred` | 着地の保留（`needs_session`） | `commit_mismatch` / `receipt_missing` / `receipt_invalid` / `worker_failed` / `evidence_failed` / `evidence_missing` / `worktree_dirty` / `rebase_conflict` / `rebase_empty` / `scope_violation` / `verification_failed` |
| `integration_failed` | 書き直したreceiptが`failed` | `worker_failed` |
| `integration_error` | mainを進める前のerror（元のstatusに戻す） | `backend_*`、なければ`other` |
| `integration_rebase_aborted` | 残っていたrebaseの中止 | `rebase_in_progress` |
| `runtime_error` | supervisorのabandon（provisioning、監視、adoptやresumeの開始の失敗） | `backend_*`、なければ`other` |
| `runtime_error` | wrapper自身のerror（leaseは残す） | `wrapper_failed` |
| `runtime_error` | supervisorのheartbeatの失敗（各runに書き、leaseは残す） | `lease_lost` |
| `run_recovered` | 孤児runの`recover`（supervisorの自動も手動も） | `orphaned` |
| `resume_finished` | resumeそのもののerror（`outcome: error`、`last_error`は変えない） | `backend_*`、なければ`other` |
| `resume_finished` | 書き直したreceiptが`failed`（`status: failed`） | `worker_failed`。解消・未解消は持たない |
| `triage_finished` | triageのverdictが`resume` | `triage_resume` |
| `triage_finished` | resumeを使い切った（`by: runtime`） | `resume_exhausted` |
| `triage_decided` | triageの`decide`のaskに`resume`と答えた | `triage_resume`。`retry` / `cancel`は持たない |
| `landing_decided` | `approve_landing`のaskの`send_back` / `cancel` | `sent_back` / `cancelled` |
| `cleanup_failed` | workspaceのclose（受理後、resume workspace、triage）の失敗 | `backend_*`（triageは、なければ`other`。triageのものは`by: triage`を持ち`last_error`を変えない） |
| `cleanup_failed` | 着地したworktreeとbranchの削除の失敗 | `git_failed` |
| `exit_request_timed_out` | `/exit`の応答なし | `exit_timeout` |
| `wrapper_heartbeat_expired` | wrapperの沈黙 | `heartbeat_lost` |
| `screen_capture_failed` | 終了後の画面の取得の失敗 | `backend_*` |
| `ask_delivery_failed` | workerの質問への回答の送信の失敗 | `backend_*` |
| `backend_call_failed` | cmuxの呼び出しの失敗 | `backend_timeout` / `backend_failed` |
| `review_failed` / `triage_failed` | headlessのjobの失敗 | `job_failed` |
| `conflict_precheck` | passの後の事前判定がmainとの衝突を見つけた | `rebase_conflict` |
| `revise_receipt_rejected` / `conflict_receipt_rejected` | 生きているsessionが書き直したreceiptが合わない | `commit_mismatch` / `worktree_dirty` / `receipt_invalid` |
| `push_failed` | pushの失敗 | `push_failed` |

コードを持たないもの: `resume_finished`の`resolved` / `unresolved`（runは`needs_session`のまま前の理由を保つ）、`triage_finished`の`retry` / `ask`、`prompt_waiting`（`answer_prompt`のaskが扱う）、`verification_command`（着地の結果は`integration_deferred`が持つ）。

runの`last_error`のコードは列を持たず、`domain::reason::last_error_code`がrunのイベントから導く: `last_error`を書いたか中断したイベント（`supervision_finished`、`validation_finished`、`integration_deferred` / `integration_failed` / `integration_error`、`runtime_error`、`interrupted`にした`run_recovered`、`landing_decided`、triageのもの（`by: triage`、`last_error`を変えない）を除く`cleanup_failed`、コードを持つ`triage_finished` / `triage_decided`、`status: failed`の`resume_finished`）のうち最新のもののコード。そのイベントがコード以前のものならnull。`domain::reason::run_error_code`は`last_error`のあるrunと`interrupted`のrunにだけそれを返す。`status`（attentionと`runs`）と`show`の`last_error_code`、`stats`の`reason_codes`がこれを読む（[supervisor-lifecycle](supervisor-lifecycle.md#status)）。
