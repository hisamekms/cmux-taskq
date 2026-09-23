---
id: design-domain-model
type: design
title: Domain model
status: current
created: 2026-09-21
updated: 2026-09-23
last_verified: 2026-09-23
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
  - design-persistence
---

# Domain model

## Implementation status

ステップ2で`Task`、`TaskDependency`、`TaskRun`、`RunEvent`、ステップ3で`RunProcess`と`SupervisorLease`、ステップ4で`Receipt`を実装した。ステップ6（[017](../journal/017-parallel-runs.md)）で`SupervisorLease`を`RunLease`に置き換え、ステップ7（[018](../journal/018-merge-queue.md)）でrunの`integrating`と`needs_session`、`IntegrationOutcome`の`needs_session` / `failed` / `no_run_awaiting`を加えた。[020](../journal/020-goal-groups-task-definitions.md)のT2で`Goal`と`Task.goal_id` / `Task.context`、receiptの`follow_ups`を加えた（[ADR-0009](../adr/0009-goal-groups-tasks.md)）。Rustの型と手動遷移規則は`src/domain.rs`、ストレージとprovider/workspaceの契約は`src/application.rs`、永続化は`src/infrastructure/sqlite.rs`と`src/infrastructure/runtime_store.rs`にある。`AgentSession`と`Workspace`は独立エンティティにせず、TaskRunの`id`（Claude session ID）と`workspace_id`で表す。

## Entities

- `Goal`: 複数のtaskが解く上位の課題。title、description、acceptance、constraints（命名・境界・やらないこと）、doc（repository内の参照文書のpath、任意）を持つ。状態機械もverification_commandsも持たず、進捗は所属taskのstatusから導出する。閉じたことは`closed_at`と`verdict`（`achieved` | `abandoned`）で1回だけ記録する。
- `Task`: ユーザーが登録する作業。公開statusを持つ。`goal_id`（任意）で1つのgoalに属し、`context`（既定は空）に「なぜやるか」と参照文書を持つ。
- `TaskDependency`: taskからpredecessorへの有向辺。循環は禁止する。
- `TaskRun`: 1回の実行試行。provider、worktree、branch、結果、実行statusを持つ。
- `AgentSession`: providerが起動したセッション。プロセスとprovider固有識別子を持つ。
- `Workspace`: cmux workspace。TaskRunと1対1で関連し、receipt検証を通った後にsupervisorが閉じる。閉じたことの確認は`TaskRun.workspace_closed_at`で持つ。
- `RunLease`: 1つのrunを所有するプロセス（実行中はsupervisor、着地中は`integrate`）のPIDとheartbeat。runごとに高々1つで、そのプロセスがrunを扱っている間だけ存在する。
- `RunProcess`: runごとのsession wrapperとagentのPID、heartbeat、終了コード。
- `RunEvent`: 実行中に発生した永続イベント。
- `Receipt`: agentが提出する完了レシート。run ID、結果、commit、tests/e2e/subagent_reviewの状態と証跡または理由、要約と、任意の`follow_ups`（workerが提案する後続task。`{"title", "description"}`の配列）を持つ。構造の整合性は`Receipt::check`、Gitと検証コマンドの確認はsupervisorが行う。`follow_ups`は配列であることだけを確認し、検証には使わない。`integrate`が着地後に各項目（titleのあるもの）を元のtaskと同じgoalのdraft taskとして登録し（goalが閉じていればgoalなし）、runに`follow_up_registered`を記録する（[ADR-0019](../adr/0019-move-routine-maintainer-work-into-the-runtime.md)の決定4）。`ready`にするかは人が決める。`IntegrationOutcome`の`integrated`は登録したtaskを`follow_ups`（`RegisteredFollowUp`: `task_id`、`title`の配列）に持つ。

`Task.id`と`Goal.id`はSQLiteの整数ID、`TaskRun.id`はUUID。Taskはtitle、description、acceptance、verification_commands、goal_id、contextを保持する。TaskRunはprovider、base commitと、branch/worktree/workspace/receipt/log/result commitの任意参照を持ち、idle marker `idle.json`のpathは`run_dir`から導出する。`run_dir`・worktree・receipt・logの配置は`RunPaths`（`<runs dir>/<run-id>/`の`worktree/`、`receipt.json`、`claude.debug.log`）が決め、storeは読み出しのたびに`TaskRun::relocated`でqueueの今の`runs/`から解決し直す（[ADR-0017](../adr/0017-resolve-run-paths-from-the-queue-directory.md)）。claim時のproviderは`claude`のみで、リソース参照は作成前のためnullになる。

## Current operations

- `add`でdraftを作り、`draft → ready`、`ready → draft`、`draft/ready → canceled`を手動操作できる。
- `claim`だけが`ready → in_progress`へ遷移させる。同じトランザクションでclaimed状態のTaskRunとイベントを作り、supervisorからのclaimはそのrunの`RunLease`も作る。キュー全体の実行枠はなく、依存が解けたtaskは`supervise --parallel N`の上限まで同時に実行される。
- supervisorはrunを`claimed → starting`（path計画）→ `running`（agent起動）→ `validating`または`failed`（wrapper終了）→ `awaiting_integration`または`failed`（receipt検証）へ進め、`awaiting_integration`のworkspaceを閉じて`workspace_closed_at`を記録し、休止したrunの`RunLease`を解放する。各遷移はそのrunのleaseまたはwrapperの所有を要求する。runtime errorではsupervisorがそのrunだけを手放す（statusは変えず、`last_error`を書き、leaseを消す）。
- `integrate`だけが`awaiting_integration | needs_session → integrating`と、そこからの`→ integrated`（Taskは`in_progress → completed`、`result_commit`は`main`に積んだsquash commit）、`→ needs_session`（rebaseの衝突、再検証の失敗）、`→ failed`（セッションが書き直したreceiptが`failed`）、`→ 元のstatus`（mainを進める前のerror）を行う。`integrating`はqueue全体で1件。結果は`IntegrationOutcome`（`integrated` / `needs_session` / `failed` / `no_run_awaiting`）で返す。`integrate --next`は`awaiting_integration`のrunを検証完了の古い順に取り、`needs_session`は`integrate ID`で明示的に再開する。
- `in_progress`のTaskは、未完了run（claimed/starting/running/validating/awaiting_integration/integrating/needs_session）がある間は手動変更できない。すべてのrunが`failed`または`interrupted`になった`in_progress`は`ready`/`draft`/`canceled`へ手動で戻せる。再試行は新しいTaskRunになる。終端状態は変更できない。依存の追加・削除はdraft/readyだけに許可する。
- `recover`は未完了runを、そのrunの登録プロセスとleaseの所有者が停止していることを確認してから`interrupted`にする（`integrating`なら`awaiting_integration`へ戻す）。他のrunには触れない。Taskは`in_progress`のままで、`ready`への復帰は別操作。
- attention（maintainerか人の判断で止まっている遷移、[ADR-0016](../adr/0016-maintainer-notification-and-compact-output.md)）の判定はdomainが持つ。`event_attention(kind, payload)`はrun_eventsの1件を、`run_attention(status, exit_pending, prompt_waiting)`はrunの今のstatus（と、`running`なら`/exit`のtimeoutと未解消の`prompt_waiting`の`workspace_id`）を、`supervisor_attention(pulses)`は`supervisors`表から導出した`SupervisorPulse`（token、pid、alive、stale）の並びを判定し、`AttentionNext`（`review and integrate` / `resume session` / `inspect and close workspace` / `send /exit` / `restart supervisor` / `answer the prompt in workspace <id>`）を返す。`AttentionNext`は文字列としてserializeし、workspace idを持つのは`AnswerPrompt`だけ（[ADR-0019](../adr/0019-move-routine-maintainer-work-into-the-runtime.md)）。staleの規則`heartbeat_stale(alive, age)`と`HEARTBEAT_TIMEOUT_SECS`（30秒）もdomainにある。supervisorの起動・停止はrun_eventsに載せない。読み口は[supervisor-lifecycle](supervisor-lifecycle.md#events--watch)の`status` / `events` / `watch`。
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
- `goal list`はgoalごとに`closed`、`verdict`、所属taskのstatus別件数（`TaskStatusCounts`）を返し、`goal show`はgoal、所属taskのid/title/status、goalのイベントを返す。
- `show`と`goal show`の既定出力は`src/view.rs`が`TaskDetail` / `GoalDetail`から作る圧縮形で、全文は`--full`（[ADR-0016](../adr/0016-maintainer-notification-and-compact-output.md)の決定4）。キー名は全文と同じで、省くか切り詰めるだけ。長い文字列（taskの`description`/`acceptance`/`context`、goalの`title`/`description`/`acceptance`/`constraints`、runの`last_error`、eventの要点の値）は300文字で切って`…`を付け、それを持つobjectに`truncated: true`を足す。`show`は最新runの`id`/`status`/`branch`/`result_commit`/`last_error`/`worktree_path`/`workspace_id`だけを`runs`に1件、そのrunの`processes`、直近10件（`--events N`で変更）のイベントを`id`/`kind`/`created_at`、runに属するイベントなら`run_id`、payloadの`status`/`reason`/`last_error`/`from`/`to`だけで返し、pathは出さない。`goal show`はイベントを直近10件の`kind`/`created_at`だけにする。どちらも全件数を`runs_total` / `events_total`で添える。
- scheduling（`candidates`、`claim`）はgoalを見ない。supervisorのclaim順は解放数とIDだけで決まり、goalをまたぐ依存も許す。

## DomainError

domainの関数は業務上の拒否を`DomainError`（`src/domain.rs`）で返す。`std::error::Error`と`Display`を実装し、`anyhow`・`rusqlite`などI/OやDBのライブラリには依存しない。I/Oを行うapplication / infrastructure / runtimeは境界で`?`により`anyhow::Error`へ変換し、原因の説明が要る場所だけ`context`を足す。`Display`はCLIが`{"error": ...}`に出す文、runtimeが`last_error`に書く文そのもので、`DomainError`の導入前の文字列と一致する。variantは業務上の拒否だけで、汎用の`Other(String)`は持たない。

| variant | 返す関数 | 持つ情報 | `Display` |
| --- | --- | --- | --- |
| `UnknownValue` | `string_enum!`の`FromStr`（`TaskStatus`、`RunStatus`、`Provider`、`SupervisorMode`、`GoalVerdict`、`ReceiptResult`、`CheckStatus`） | enum名、値 | `unknown <Enum>: <value>` |
| `TaskHasUnfinishedRun` | `TaskStatus::transition` | action | `task has an unfinished run; recover or integrate it before applying <Action>` |
| `TransitionNotAllowed` | `TaskStatus::transition` | 現在のstatus、action | `cannot apply <Action> to task in <status> state` |
| `Blank` | `NewTask::validate`、`NewGoal::validate`、`GoalEdit::apply` | field名（`task title`、`verification commands`、`goal title`） | `<field> must not be blank` |
| `NonPositiveId` | `NewTask::validate` | field名（`dependency IDs`、`goal ID`） | `<field> must be positive` |
| `GoalAlreadyClosed` | `GoalVerdict::check_close` | goal ID、記録済みのverdict | `goal <id> is already closed as <verdict>` |
| `GoalCloseBlocked` | `GoalVerdict::check_close` | goal ID、verdict、verdictを許さないstatusと件数 | `goal <id> cannot be closed as <verdict>: <n> task(s) <status>, ...` |
| `MalformedReceipt` | `Receipt::parse` | パーサーの理由 | `receipt is not a valid completion receipt: <reason>` |
| `ReceiptRunMismatch` | `Receipt::check` | receiptのrun_id、runのID | `receipt run_id <a> does not match run <b>` |
| `AgentReportedResult` | `Receipt::check` | result、summary | `agent reported result <result>: <summary>` |
| `ReceiptCheckFailed` | `Receipt::check` | check名、evidence_or_reason | `receipt reports <check> as failed: <evidence>` |
| `ReceiptCheckUnexplained` | `Receipt::check` | check名、status | `receipt <check> is <status> without evidence or reason` |
| `InvalidCommit` | `Receipt::check`、`validate_base_commit` | field名（`receipt commit`、`base commit`） | `<field>: must be a full 40- or 64-character hexadecimal Git object ID` |
| `FollowUpsNotArray` | `Receipt::check` | なし | `receipt follow_ups must be an array` |
| `MissingRunDirectory` | `TaskRun::idle_marker_path` | なし | `missing run directory` |

`goal close`の判断（閉じたgoalは閉じられない、verdictが所属taskのstatusを許すか）は`GoalVerdict::check_close`が持ち、`SqliteQueue::close_goal`はgoalとstatus別件数を読んで渡し、結果を書くだけである。DBに保存された文字列が既知のenum値でないときは、`enum_col`が`UnknownValue`を`rusqlite`の変換エラーの原因として包む。

## Invariants

- Taskは自分自身に依存できない。
- Taskは高々1つのgoalに属し、閉じたgoalには属せるtaskが増えない。goalのverdictは1回だけ記録され、`closed_at`と`verdict`は同時にnullか同時に非nullである。
- 依存グラフは循環しない。
- `in_progress`はschedulerがclaimしたTaskだけが持つ。
- TaskRunが成功するには完了レシート、base commitの上に積まれたbranch headのコミット、clean worktree、supervisorが再実行した検証コマンドの成功が必要。receiptの自己申告だけでは成功しない。
- Taskが`completed`になるのは、その`integrated` runを`integrate`がmainへ着地させたときだけ。着地commitのtreeは再検証したworktreeのtreeに等しく、messageは`Dagq-Task` / `Dagq-Run` trailerでrunに結び付く。`integrated` runはTaskごとに1件、`integrating` runはqueueごとに1件。
- mainはtaskごとに1つのsquash commitの直線で、merge commitとrun branchのfast-forwardは作らない。runの詳細履歴は`refs/dagq/runs/<run-id>`に残る。
- `needs_session`のrunはruntimeが変更しない。解消・検証コマンドの再実行・receiptの書き直しはセッションが行い、`integrate ID`が同じ手順で再検証する。
- workspaceを閉じる前にTaskRunをcleanedにしない。閉じたことをcmuxの応答で確認して`workspace_closed_at`に記録するまでは開いている扱いで、close失敗はrun状態を変えない。
- agentの異常終了だけでTaskを自動再実行しない。孤児runの復旧と再試行はどちらも明示操作。
- 実装途中のprovider fallbackは行わず、起動不能など安全に判定できる場合だけfallbackする。
- 1 runの失敗・中断・復旧は他のrunの状態、lease、プロセス、リソースを変えない。

### `TaskRun.last_error`

`last_error`は「そのrunを止めた、または人の確認が要る最新の理由」を1つだけ持つ。書き込みは後のものが前のものを上書きし、着地（`integrated`）でnullになる。理由の全履歴は`run_events`が持つ。書くのは次の場面で、それぞれ対応するイベントと組で残る。

| 場面 | status | 書く文 | イベント |
| --- | --- | --- | --- |
| 非0終了 | `starting`/`running` → `failed` | `session exited with code N`（Nはwrapperが報告した終了コード。signalで終わったセッションは128） | `supervision_finished` |
| receipt検証の拒否 | `validating` → `failed` | 最初に外れた項目の理由をそのまま。`receipt was not submitted at <path>`、receiptの構造や`run_id`の不一致のエラー文、`worktree is on <ref> instead of refs/heads/<branch>`、`receipt commit <sha> is not the head of <branch> (<head>)`、`no commit was made on top of base <sha>`、`commit <sha> does not descend from base <sha>`、`worktree is not clean:` に続く`git status`、`verification command "<cmd>" exited with <code>; see <run-dir>/verify-N.log` | `validation_finished` |
| runtime error / provisioning error | 変えない | errorの文をそのまま。worktree・workspaceの作成失敗は`run <run-id> provisioning failed: <error>`、監視中は`wrapper heartbeat expired; session may still be alive`、検証処理そのもの（Git・DB）のerror文。supervisorはそのrunのleaseを消して手放す（abandon）。wrapper自身のerrorとsupervisor heartbeatの失敗も同じ列に書くがleaseは残す（wrapperは子が死んでいれば続けて終了コード127を報告し、runは`session exited with code 127`で`failed`になる） | `runtime_error` |
| cleanup失敗 | 変えない | 受け入れたrunのworkspace closeの失敗は`workspace <workspace-id> could not be closed: <error>`、着地後のworktree/branch削除の失敗は`landed worktree <path> could not be removed: <error>` | `cleanup_failed` |
| 着地の保留・中断・失敗 | `integrating` → `needs_session` / 元のstatus / `failed` | rebaseの衝突や再検証の失敗の理由、mainを進める前のGit/DB errorは`integration stopped before main moved: <error>`、セッションが書き直した`failed` receiptの理由（[supervisor-lifecycle](supervisor-lifecycle.md#integrate)） | `integration_deferred` / `integration_error` / `integration_failed` |

`last_error`はstatusと最後のイベントに合わせて読む。`failed`なら非0終了・検証拒否・着地時の`failed` receipt、`claimed`/`starting`/`running`/`validating`で`last_error`があればsupervisorが手放したrun（`doctor`にleaseなしで出る）、`awaiting_integration`で`last_error`があればclose失敗（`cleanup_failed`、`workspace_closed_at`はnull）かmainを進める前に止まった着地（`integration_error`）、`needs_session`なら着地の衝突である。`show`・`doctor`・superviseの結果の`errors`に出て、`list`には出ない。`recover`は`last_error`を上書きしない。
