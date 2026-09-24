---
id: design-domain-model
type: design
title: Domain model
status: current
created: 2026-09-21
updated: 2026-09-24
last_verified: 2026-09-24
scope: domain
related:
  - adr-0003
  - adr-0004
  - adr-0007
  - adr-0008
  - adr-0009
  - adr-0016
  - adr-0019
  - adr-0023
  - adr-0024
  - adr-0029
  - design-persistence
---

# Domain model

## Implementation status

ステップ2で`Task`、`TaskDependency`、`TaskRun`、`RunEvent`、ステップ3で`RunProcess`と`SupervisorLease`、ステップ4で`Receipt`を実装した。ステップ6（[017](../journal/017-parallel-runs.md)）で`SupervisorLease`を`RunLease`に置き換え、ステップ7（[018](../journal/018-merge-queue.md)）でrunの`integrating`と`needs_session`、`IntegrationOutcome`の`needs_session` / `failed` / `no_run_awaiting`を加えた。[020](../journal/020-goal-groups-task-definitions.md)のT2で`Goal`と`Task.goal_id` / `Task.context`、receiptの`follow_ups`を加えた（[ADR-0009](../adr/0009-goal-groups-tasks.md)）。goal 12でgoalのdraft状態（`GoalStatus`）と、run_eventsのkind `observation`で表すnote（`NewNote`）を加えた（ADR-0024の決定4、5）。goal 8のtask 73で`Task.required_evidence`（`EvidenceCheck`）と`Receipt::missing_evidence`を加えた（ADR-0019の決定5）。Rustの型と手動遷移規則は`src/domain.rs`、ストレージとprovider/workspaceの契約は`src/application.rs`、永続化は`src/infrastructure/sqlite.rs`と`src/infrastructure/runtime_store.rs`にある。`AgentSession`と`Workspace`は独立エンティティにせず、TaskRunの`id`（Claude session ID）と`workspace_id`で表す。

## Entities

- `Goal`: 複数のtaskが解く上位の課題。title、description、acceptance、constraints（命名・境界・やらないこと）、doc（repository内の参照文書のpath、任意）を持つ。verification_commandsは持たず、進捗は所属taskのstatusから導出する。状態は`status`（`GoalStatus`: `draft` | `open`）の1点だけで（ADR-0024の決定5がADR-0009の「状態機械を持たない」をこの1点に限って改めた）、`draft`のgoalのtaskは`candidates`に出ずclaimされない。閉じたことは`status`と独立に`closed_at`と`verdict`（`achieved` | `abandoned`）で1回だけ記録する。
- note（observation）: task / run / goalのいずれかに紐づく自由記述。独立のentityにせず、`RunEvent`のkind `observation`（payload `{text, kind, by}`）で表す。`kind`は小文字・数字・`-`・`_`のslug（既定`note`）、`by`は書いた環境の`DAGQ_ROLE`で、無ければ`human`。
- `Task`: ユーザーが登録する作業。公開statusを持つ。`goal_id`（任意）で1つのgoalに属し、`context`（既定は空）に「なぜやるか」と参照文書を持つ。
- `TaskDependency`: taskからpredecessorへの有向辺。循環は禁止する。
- `TaskRun`: 1回の実行試行。provider、worktree、branch、結果、実行statusを持つ。
- `AgentSession`: providerが起動したセッション。プロセスとprovider固有識別子を持つ。
- `Workspace`: cmux workspace。TaskRunと1対1で関連し、receipt検証を通った後にsupervisorが閉じる。閉じたことの確認は`TaskRun.workspace_closed_at`で持つ。
- `RunLease`: 1つのrunを所有するプロセス（実行中はsupervisor、着地中は`integrate`）のPIDとheartbeat。runごとに高々1つで、そのプロセスがrunを扱っている間だけ存在する。
- `RunProcess`: runごとのsession wrapperとagentのPID、heartbeat、終了コード。
- `RunEvent`: 実行中に発生した永続イベント。
- `Receipt`: agentが提出する完了レシート。run ID、結果、commit、tests/e2e/subagent_reviewの状態と証跡または理由、要約と、任意の`follow_ups`（workerが提案する後続task。`{"title", "description"}`の配列）を持つ。構造の整合性は`Receipt::check`、Gitと検証コマンドの確認はsupervisorが行う。`follow_ups`は配列であることだけを確認し、検証には使わない。`integrate`が着地後に各項目（titleのあるもの）を元のtaskと同じgoalのdraft taskとして登録し（goalが閉じていればgoalなし）、runに`follow_up_registered`を記録する（ADR-0019の決定4）。`ready`にするかは人が決める。`IntegrationOutcome`の`integrated`は登録したtaskを`follow_ups`（`RegisteredFollowUp`: `task_id`、`title`の配列）に持つ。

`Task.id`と`Goal.id`はSQLiteの整数ID、`TaskRun.id`はUUID。Taskはtitle、description、acceptance、verification_commands、required_evidence、paths、goal_id、contextを保持する。`paths`はrunが変えてよいパスのglobの配列（[ADR-0029](../adr/0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)）で、`add --paths`で与え、draft / readyの間は`set-paths TASK --paths GLOB... | --none`で置き換える（変化があったときだけ`task_paths_changed`（`from`、`to`）を記録する）。空なら制限しない。globの規則と判定は`domain::scope`が持つ: repository root起点のパス全体に合わせ、`*`と`?`は1つのsegmentの中、segment全体が`**`なら0個以上のsegmentに合い、ほかは字義どおり。`validate_path_globs`が空・`/`始まり・`.` / `..` / 空のsegmentを拒否し（`DomainError::InvalidPathGlob`）、`out_of_scope(globs, changed)`がどのglobにも合わない変更パスを返し、`scope_violation_reason`が`changed paths outside the task's --paths: <paths>`の形にする。`required_evidence`はvalidationがreceiptに要求するcheckの配列（`EvidenceCheck`: `tests` | `e2e` | `subagent_review`。receiptのcheck名と同じ）で、`add --evidence`で与え、`NewTask::required_evidence()`が与えた順に重複を除いて保存する。無ければ空配列で、従来どおり何も要求しない。`Receipt::missing_evidence(required)`は要求されたcheckのうち`status`が`passed`でないか`evidence_or_reason`が空白のものを要求の順に返し、`evidence_missing_reason`がそれを`evidence missing: e2e`（複数は`, `区切り）の形にする。TaskRunはprovider、base commitと、branch/worktree/workspace/receipt/log/result commitの任意参照を持ち、idle marker `idle.json`のpathは`run_dir`から導出する。`run_dir`・worktree・receipt・logの配置は`RunPaths`（`<runs dir>/<run-id>/`の`worktree/`、`receipt.json`、`claude.debug.log`）が決め、storeは読み出しのたびに`TaskRun::relocated`でqueueの今の`runs/`から解決し直す（[ADR-0017](../adr/0017-resolve-run-paths-from-the-queue-directory.md)）。claim時のproviderは`claude`のみで、リソース参照は作成前のためnullになる。

## Current operations

- `add`でdraftを作り、`draft → ready`、`ready → draft`、`draft/ready → canceled`を手動操作できる。
- `claim`だけが`ready → in_progress`へ遷移させる。同じトランザクションでclaimed状態のTaskRunとイベントを作り、supervisorからのclaimはそのrunの`RunLease`も作る。キュー全体の実行枠はなく、依存が解けたtaskは`supervise --parallel N`の上限まで同時に実行される。
- supervisorはrunを`claimed → starting`（path計画）→ `running`（agent起動）→ `validating`または`failed`（wrapper終了）→ `awaiting_integration`または`failed`（receipt検証）へ進め、`awaiting_integration`のworkspaceを閉じて`workspace_closed_at`を記録し、休止したrunの`RunLease`を解放する。各遷移はそのrunのleaseまたはwrapperの所有を要求する。runtime errorではsupervisorがそのrunだけを手放す（statusは変えず、`last_error`を書き、leaseを消す）。
- `integrate`だけが`awaiting_integration | needs_session → integrating`と、そこからの`→ integrated`（Taskは`in_progress → completed`、`result_commit`は`main`に積んだsquash commit）、`→ needs_session`（rebaseの衝突、再検証の失敗）、`→ failed`（セッションが書き直したreceiptが`failed`）、`→ 元のstatus`（mainを進める前のerror）を行う。`integrating`はqueue全体で1件。結果は`IntegrationOutcome`（`integrated` / `needs_session` / `failed` / `no_run_awaiting`）で返す。`integrate --next`は`awaiting_integration`のrunを検証完了の古い順に取り、`needs_session`は`integrate ID`で明示的に再開する。
- `in_progress`のTaskは、未完了run（claimed/starting/running/validating/awaiting_integration/integrating/needs_session）がある間は手動変更できない。すべてのrunが`failed`または`interrupted`になった`in_progress`は`ready`/`draft`/`canceled`へ手動で戻せる。再試行は新しいTaskRunになる。終端状態は変更できない。依存の追加・削除はdraft/readyだけに許可する。
- `recover`は未完了runを、そのrunの登録プロセスとleaseの所有者が停止していることを確認してから`interrupted`にする（`integrating`なら`awaiting_integration`へ戻す）。他のrunには触れない。Taskは`in_progress`のままで、`ready`への復帰は別操作。
- attention（人の判断で止まっている遷移か、supervisorが動かしている遷移。ADR-0016）の判定はdomainが持つ。`event_attention(kind, payload)`はrun_eventsの1件を、`run_attention(status, exit_pending, push_pending, leased)`はrunの今のstatus（と、`/exit`のtimeout、pushの失敗、lease行の有無）を、`supervisor_attention(pulses)`は`supervisors`表から導出した`SupervisorPulse`（token、pid、alive、stale）の並びを判定し、`AttentionNext`（`review and integrate` / `resuming (runtime)` / `triaging (runtime)` / `triage by hand` / `restart supervisor`など）を返す（`send /exit`はtask 104で消え、`/exit`のtimeoutはsupervisorの`stuck_exit`のaskになった。`inspect and close workspace`はtask 98で消え、`failed` / `interrupted`のrunはsupervisorのtriageになった。`resume session`と`answer the prompt in workspace <id>`はtask 100で消え、3回resumeして解消しない`needs_session`はsupervisorが`failed`にしてtriageの`decide`のask（`retry` / `cancel`）に、`running`のrunのdialog（`prompt_waiting`）は`answer_prompt`のaskになった。`needs_session`のrunはどの場合も`resuming (runtime)`。ADR-0024のConsequences）。`AttentionNext`は文字列としてserializeする。attentionはすべてinboxのもの（`ATTENTION_ROLE`、ADR-0024の決定6）で、plannerのものは無い。staleの規則`heartbeat_stale(alive, age)`と`HEARTBEAT_TIMEOUT_SECS`（30秒）もdomainにある。supervisorの起動・停止はrun_eventsに載せない。読み口は[supervisor-lifecycle](supervisor-lifecycle.md#events--watch)の`status` / `events` / `watch`。
- `succeeded`（統合を待たずに成功とする運用）への遷移はまだ公開していない。
- `candidates`は全依存がcompletedのready taskをID順で返す。claimはTaskごとの未完了run 1件の制約だけを再確認する。
- `graph [--goal ID]`は未完了（draft / ready / in_progress）のtaskの依存の見取り図を返す（[ADR-0023](../adr/0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)の決定4）。storeの`graph_input`が未完了taskと直接の依存元（完了済みも含む）、`candidates`のIDを1つのsnapshotで読み、application層の純粋関数`dependency_graph`が`DependencyGraph`を作る。taskごとに`depends_on`（直接の依存元すべて）、`ready_after`（そのうち未完了のもの）、`blocks`（直接依存している未完了task）、`unblocks`（推移閉包で依存している未完了taskの数。canceledと完了済みは数えない）を持つ。`candidates`はclaim順（`unblocks`の多い順、同数ならID昇順）、`critical`は`unblocks`が最大のtask（同数なら小さいID）から`blocks`のうち`unblocks`が最大のものを辿った鎖で、どのtaskも他を塞いでいなければ空。`--goal`は`tasks`・`candidates`・`critical`の起点をそのgoalに絞るだけで、数は全goalの未完了taskで数える（鎖はgoalの外へ出てよい）。循環は依存の追加時に拒否済みなので前提にする。
- supervisorの`fill_slots`はclaimのたびに`dependency_graph`の`candidates`の順を作り、`claim_for_supervisor_in_order`に渡す。claimのトランザクションはその順で最初にまだclaim可能なtaskを取り（どれも取れなければID順の先頭）、SQLの`READY_QUERY`の`ORDER BY`は変えない。順序は`graph`で再現できるので`claim_reordered`は記録しない。storeの`claim`（lease無し）は従来どおりID順。
- canceled、失敗、中断、統合待ち、着地中、セッション待ちは依存の完了条件を満たさない。awaiting_integration / integrating / needs_sessionのTaskはin_progressのまま保持し、integratedになった時点でcompletedになる。
- `RunEvent.run_id`はtask登録・依存変更などrun作成前のイベントではnullになる。goal単位のイベント（`goal_created`、`goal_updated`、`goal_closed`）は`task_id`がnullで`goal_id`を持ち、`goal show`に並ぶ。`task_goal_changed`はtaskのイベントで、`from`と`to`にgoal IDを持つ。
- `goal add`でgoalを作り、`add --goal ID`と`set-goal TASK GOAL`でtaskを所属させ、`set-goal TASK --none`で外す。所属の変更は依存の追加・削除と同じくdraft/readyのtaskだけに許し、閉じたgoalへの追加と付け替えは拒否する。`task_created`のpayloadは`goal_id`を持ち、`set-goal`は変化があったときだけ`task_goal_changed`を記録する。
- `goal edit`はtitle、description、acceptance、constraints、docを差し替え、`goal_updated`のpayloadに`old`と`new`のgoal全体を残す。閉じたgoalも編集できる（記録の訂正のため）。走行中のrunは`prompt.txt`のスナップショットのままで、次のclaimから新しい記述が使われる。
- `goal close ID --verdict achieved|abandoned`はverdictを1回だけ記録する。`achieved`は所属taskに`completed` / `canceled`以外があれば拒否し、`abandoned`は`in_progress`があれば拒否する（`GoalVerdict::allows`、判断の入口は`GoalVerdict::check_close`）。閉じたgoalを再び閉じることはできず、続きは新しいgoalに登録する。`goal_closed`のpayloadはverdictとclose時点のstatus別件数を持つ。
- `list`は既定で終端状態（`TaskStatus::is_terminal`）でないtaskを新しい順（ID降順）に最大20件、`{"tasks", "next", "total"}`で返す。要素はid/status/title/goal_id/dependencies/latest_run（最新runのidとstatus）に縮約し、`--full`で残りの全項目を足す。`next`は次ページの先頭task ID（`--before`に渡す。`--before ID`はID以下のtaskを返す）で、続きがなければnull。`total`はフィルタ後の件数。フィルタとページの条件はapplication層の`TaskQuery`、出力は`TaskPage` / `TaskListItem`で、domainの`Task`は変えない。
- `goal list`はgoalごとに`status`（`draft` | `open`）、`closed`、`verdict`、所属taskのstatus別件数（`TaskStatusCounts`）を返し、`goal show`はgoal（`status`を含む）、所属taskのid/title/status、goalのイベントを返す。
- `goal add --draft`はdraftのgoalを作り、`goal ready ID`がdraftを`open`にして`goal_status_changed`（`from: draft`、`to: open`）を記録する。`goal ready`は閉じていないdraftにだけ許す（`Goal::check_ready`）。draftのgoalにも`add --goal`と`set-goal`でtaskを所属させられ、`goal close`もできる（採らない提案は`abandoned`で閉じる）。draftのgoalのtaskは`ready`にしても`candidates`・`graph`の`candidates`・supervisorのclaimに出ない（`READY_QUERY`がgoalの`status = 'draft'`を除く）。`graph`は所属goalのあるtaskに`goal_status`を添える。
- `note --task ID | --run RUN_ID | --goal ID --text TEXT [--kind SLUG]`はkind `observation`のrun_eventを1件書いて返す。`--task`はtaskのイベント、`--run`はそのrunのtaskとrunのイベント、`--goal`はgoal単位のイベントになる。`notes [--goal ID] [--task ID] [--since CURSOR] [--limit N]`はobservationだけを古い順に返し（`{"notes", "cursor"}`）、`--since`なしは直近`--limit`件（既定20）、`--since`ありはcursorより後の最初の`--limit`件。`--goal`はgoalのnoteに加えて所属taskとそのrunのnoteを、`--task`はtaskとそのrunのnoteを含む。`cursor`は最後のnoteのイベントID（空なら`--since`の値、それも無ければ0）。
- `DAGQ_ROLE=observer`の環境からは、CLIの入口（`main.rs`の`observer_access`）が許可の一覧にないコマンドを`{"error": "observer may not change queue state"}`で拒否する（ADR-0024の決定4）。許すのは読み取り（`locate` / `list` / `show` / `candidates` / `graph` / `status` / `events` / `watch` / `stats` / `doctor` / `goal list` / `goal show` / `notes`）、`note`、`goal add --draft`、そしてdraftで閉じていないgoalへの`add --goal`（taskはdraftで登録される）だけ。`ready` / `draft` / `cancel` / `integrate` / `recover` / `goal ready` / `goal close` / `goal edit` / 通常の`goal add` / goalなしかopenなgoalへの`add` / `dependency` / `set-goal` / `review` / `init` / `up` / `down`などは拒否する。一覧に載せない限り後から足したコマンドも拒否される。task 99で`ask --kind blocked`（observerが閾値超えをinboxに上げるask。taskにもrunにも紐づかなくてよい唯一のkind）を一覧に足した。ほかのkindの`ask`、`answer`、`ask close`、`observe`、`supervise`は拒否する。判定は`DAGQ_ROLE`の申告に依存する柵で、悪意ある実行は防がない。
- `show`と`goal show`の既定出力は`src/view.rs`が`TaskDetail` / `GoalDetail`から作る圧縮形で、全文は`--full`（ADR-0016の決定4）。キー名は全文と同じで、省くか切り詰めるだけ。長い文字列（taskの`description`/`acceptance`/`context`、goalの`title`/`description`/`acceptance`/`constraints`、runの`last_error`、eventの要点の値）は300文字で切って`…`を付け、それを持つobjectに`truncated: true`を足す。`show`は最新runの`id`/`status`/`branch`/`result_commit`/`last_error`/`worktree_path`/`workspace_id`だけを`runs`に1件、そのrunの`processes`、直近10件（`--events N`で変更）のイベントを`id`/`kind`/`created_at`、runに属するイベントなら`run_id`、payloadの`status`/`reason`/`last_error`/`from`/`to`だけで返し、pathは出さない。`goal show`はイベントを直近10件の`kind`/`created_at`だけにする。どちらも全件数を`runs_total` / `events_total`で添える。どちらも`observations`に、自分に紐づくnote（`show`はtaskとそのrun、`goal show`はgoal自身）の直近5件を`id`/`created_at`/`run_id`（あれば）/`text`（300文字で切る）/`kind`/`by`で古い順に添える。
- scheduling（`candidates`、`claim`）が見るgoalの性質は、draftのgoalのtaskを除くことだけ。supervisorのclaim順は解放数とIDだけで決まり、goalをまたぐ依存も許す。

## IDとcommitのnewtype

IDとcommitはドメインプリミティブのnewtype（`src/domain/ids.rs`、`domain`から再公開。[ADR-0013](../adr/0013-layered-architecture-and-type-function-style.md)の決定4）で、task IDとgoal IDのように意味の違う値を型で区別する。内部のフィールドは非公開で、生成は下の入口だけ、値の取り出しは`as_i64` / `as_str` / `into_string`で行う。型エイリアスは使わない。

| 型 | 包む値 | 生成と検証 | trait | 使う場所 |
| --- | --- | --- | --- | --- |
| `TaskId` | `i64`（`tasks.id`） | `TaskId::new`（検証なし。正であることは`NewTask::validate`などが`NonPositiveId`で確かめる） | `Copy`、`Eq`、`Ord`、`Hash`、`Display` | `Task.id`、`TaskRun.task_id`、`GoalTask.id`、`RunEvent.task_id`、`Ask` / `NewAsk`の`task_id`、`NoteTarget::Task`、`NoteQuery.task_id`、`TaskDetail.dependencies`、`NewTask.dependencies`、`RegisteredFollowUp.task_id`、`Attention.task_id`、`stats`の`RunStats` / `Alert`、`TaskStore`の引数、`TaskQuery.before` / `TaskPage.next`、graphの型 |
| `GoalId` | `i64`（`goals.id`） | `GoalId::new`（検証なし） | `TaskId`と同じ | `Goal.id`、`GoalSummary.id`、`Task.goal_id`、`NewTask.goal_id`、`RunEvent.goal_id`、`NoteTarget::Goal`、`NoteQuery` / `StatsQuery` / `TaskQuery`の`goal_id`、`DomainError`のgoal ID、`TaskStore`の引数 |
| `RunId` | `String`（UUID） | `RunId::new` / `TryFrom<String>` / `TryFrom<&str>`。空白だけの値は`Blank { field: "run ID" }` | `Clone`、`Eq`、`Ord`、`Hash`、`Display`、`AsRef<str>`、文字列との`PartialEq` | `TaskRun.id`、`RunEvent` / `Ask` / `NewAsk` / `Attention`の`run_id`、`RunLease.run_id`、`RunProcess.run_id`、`NoteTarget::Run`、`RunPaths::new`、`Receipt::check`の引数、`runtime_store` / `asks`のrun ID引数 |
| `CommitSha` | `String`（40桁か64桁の16進） | `CommitSha::parse(value, field)` / `TryFrom<String>` / `TryFrom<&str>`（field名`commit`）。外れれば`InvalidCommit { field }` | `RunId`と同じ | `TaskRun.base_commit` / `result_commit`、`IntegrationOutcome::NeedsSession.main`、`Validation.result_commit`、`Landing`の`commit` / `source_commit` / `main_before`、`TaskStore::claim`と`claim_for_supervisor`・`begin_integration`・`begin_resume`・`skip_resume`の引数、`GitRepository`の`main_head` / `head` / `merge_base` / `commit_tree`の戻り値 |

- serdeでは`#[serde(transparent)]`で素の値として出るので、CLIのJSON出力は変わらない。`RunId`と`CommitSha`の`Deserialize`は生成と同じ検証を通す。
- SQLiteとの変換（`ToSql` / `FromSql`）はinfrastructure（`src/infrastructure/sql_ids.rs`）にある。bindは素の値で、読み出しは生成と同じ検証を通すので、空のrun IDや不正なcommitを持つ行は変換エラーになる。`params_from_iter`に渡す`Value`だけは`as_i64()`で素の値にする。
- CLIの引数はclapでは`i64` / `String`のまま受け、`main.rs`がnewtypeに変えてからapplicationに渡す。`--run`に空白だけを渡すと`run ID must not be blank`になる。
- 例外: `Receipt`の`run_id`と`commit`はagentが書くファイルの形のまま`String`で持つ。`Receipt::check`が決まった順で検証し（run_idの一致、result、各check、commitの形式）、最初に外れた項目のエラー文を`last_error`に書くため、パースの時点では検証しない。`GitRepository`の`rebase` / `is_ancestor` / `diff_*` / `changed_paths` / `tree_of`などの引数は`main`やref、`<commit>^{tree}`も受けるGitのrevisionなので`&str`のまま。`supervisor_token`とcmuxの`workspace_id`は対象外。

## DomainError

domainの関数は業務上の拒否を`DomainError`（`src/domain.rs`）で返す。`std::error::Error`と`Display`を実装し、`anyhow`・`rusqlite`などI/OやDBのライブラリには依存しない。I/Oを行うapplication / infrastructure / runtimeは境界で`?`により`anyhow::Error`へ変換し、原因の説明が要る場所だけ`context`を足す。`Display`はCLIが`{"error": ...}`に出す文、runtimeが`last_error`に書く文そのもので、`DomainError`の導入前の文字列と一致する。variantは業務上の拒否だけで、汎用の`Other(String)`は持たない。

| variant | 返す関数 | 持つ情報 | `Display` |
| --- | --- | --- | --- |
| `UnknownValue` | `string_enum!`の`FromStr`（`TaskStatus`、`RunStatus`、`Provider`、`SupervisorMode`、`SessionRole`、`GoalStatus`、`GoalVerdict`、`ReceiptResult`、`CheckStatus`） | enum名、値 | `unknown <Enum>: <value>` |
| `TaskHasUnfinishedRun` | `TaskStatus::transition` | action | `task has an unfinished run; recover or integrate it before applying <Action>` |
| `TransitionNotAllowed` | `TaskStatus::transition` | 現在のstatus、action | `cannot apply <Action> to task in <status> state` |
| `Blank` | `NewTask::validate`、`NewGoal::validate`、`GoalEdit::apply`、`NewNote::validate`、`RunId::new` | field名（`task title`、`verification commands`、`goal title`、`note text`、`run ID`） | `<field> must not be blank` |
| `NonPositiveId` | `NewTask::validate` | field名（`dependency IDs`、`goal ID`） | `<field> must be positive` |
| `GoalAlreadyClosed` | `GoalVerdict::check_close`、`Goal::check_ready` | goal ID、記録済みのverdict | `goal <id> is already closed as <verdict>` |
| `GoalCloseBlocked` | `GoalVerdict::check_close` | goal ID、verdict、verdictを許さないstatusと件数 | `goal <id> cannot be closed as <verdict>: <n> task(s) <status>, ...` |
| `MalformedReceipt` | `Receipt::parse` | パーサーの理由 | `receipt is not a valid completion receipt: <reason>` |
| `ReceiptRunMismatch` | `Receipt::check` | receiptのrun_id、runのID | `receipt run_id <a> does not match run <b>` |
| `AgentReportedResult` | `Receipt::check` | result、summary | `agent reported result <result>: <summary>` |
| `ReceiptCheckFailed` | `Receipt::check` | check名、evidence_or_reason | `receipt reports <check> as failed: <evidence>` |
| `ReceiptCheckUnexplained` | `Receipt::check` | check名、status | `receipt <check> is <status> without evidence or reason` |
| `InvalidCommit` | `Receipt::check`、`CommitSha::parse` / `TryFrom` | field名（`receipt commit`、`base commit`、`commit`、Gitの出力なら`HEAD`・`main commit`など） | `<field>: must be a full 40- or 64-character hexadecimal Git object ID` |
| `FollowUpsNotArray` | `Receipt::check` | なし | `receipt follow_ups must be an array` |
| `MissingRunDirectory` | `TaskRun::idle_marker_path` | なし | `missing run directory` |
| `GoalNotDraft` | `Goal::check_ready` | goal ID | `goal <id> is not a draft` |
| `InvalidNoteKind` | `NewNote::validate` | kind | `note kind "<kind>" must be a slug of lowercase letters, digits, '-' and '_'` |

`goal close`の判断（閉じたgoalは閉じられない、verdictが所属taskのstatusを許すか）は`GoalVerdict::check_close`が持ち、`SqliteQueue::close_goal`はgoalとstatus別件数を読んで渡し、結果を書くだけである。DBに保存された文字列が既知のenum値でないときは、`enum_col`が`UnknownValue`を`rusqlite`の変換エラーの原因として包む。

## Invariants

- Taskは自分自身に依存できない。
- Taskは高々1つのgoalに属し、閉じたgoalには属せるtaskが増えない。goalのverdictは1回だけ記録され、`closed_at`と`verdict`は同時にnullか同時に非nullである。
- 依存グラフは循環しない。
- `in_progress`はschedulerがclaimしたTaskだけが持つ。
- TaskRunが成功するには完了レシート、base commitの上に積まれたbranch headのコミット、clean worktreeが必要で、Taskが`completed`になるにはさらに`integrate`がrebase後に1回だけ実行する検証コマンドの成功が必要（ADR-0023の決定1）。receiptの自己申告だけでは成功しない。
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
| receipt検証の拒否 | `validating` → `failed` | 最初に外れた項目の理由をそのまま。`receipt was not submitted at <path>`、receiptの構造や`run_id`の不一致のエラー文、`worktree is on <ref> instead of refs/heads/<branch>`、`receipt commit <sha> is not the head of <branch> (<head>)`、`no commit was made on top of base <sha>`、`commit <sha> does not descend from base <sha>`、`worktree is not clean:` に続く`git status`（検証コマンドはvalidatingでは実行しない。ADR-0023の決定1） | `validation_finished` |
| runtime error / provisioning error | 変えない | errorの文をそのまま。worktree・workspaceの作成失敗は`run <run-id> provisioning failed: <error>`、監視中は`wrapper heartbeat expired; session may still be alive`、検証処理そのもの（Git・DB）のerror文。supervisorはそのrunのleaseを消して手放す（abandon）。wrapper自身のerrorとsupervisor heartbeatの失敗も同じ列に書くがleaseは残す（wrapperは子が死んでいれば続けて終了コード127を報告し、runは`session exited with code 127`で`failed`になる） | `runtime_error` |
| cleanup失敗 | 変えない | 受け入れたrunのworkspace closeの失敗は`workspace <workspace-id> could not be closed: <error>`、着地後のworktree/branch削除の失敗は`landed worktree <path> could not be removed: <error>` | `cleanup_failed` |
| 着地の保留・中断・失敗 | `integrating` → `needs_session` / 元のstatus / `failed` | rebaseの衝突や再検証の失敗の理由（検証コマンドの失敗は`verification command "<cmd>" exited with <code> after the rebase onto <main>; see <run-dir>/integrate-verify-N.log`）、mainを進める前のGit/DB errorは`integration stopped before main moved: <error>`、セッションが書き直した`failed` receiptの理由（[supervisor-lifecycle](supervisor-lifecycle.md#integrate)） | `integration_deferred` / `integration_error` / `integration_failed` |

`last_error`はstatusと最後のイベントに合わせて読む。`failed`なら非0終了・検証拒否・着地時の`failed` receipt、`claimed`/`starting`/`running`/`validating`で`last_error`があればsupervisorが手放したrun（`doctor`にleaseなしで出る）、`awaiting_integration`で`last_error`があればclose失敗（`cleanup_failed`、`workspace_closed_at`はnull）かmainを進める前に止まった着地（`integration_error`）、`needs_session`なら着地の衝突である。`show`・`doctor`・superviseの結果の`errors`に出て、`list`には出ない。`recover`は`last_error`を上書きしない。
