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
- `Receipt`: agentが提出する完了レシート。run ID、結果、commit、tests/e2e/subagent_reviewの状態と証跡または理由、要約と、任意の`follow_ups`（workerが提案する後続task。`{"title", "description"}`の配列）を持つ。構造の整合性は`Receipt::check`、Gitと検証コマンドの確認はsupervisorが行う。`follow_ups`は配列であることだけを確認し、検証には使わない。maintainerが`show`で読んでgoalへ登録するかを判断する。

`Task.id`と`Goal.id`はSQLiteの整数ID、`TaskRun.id`はUUID。Taskはtitle、description、acceptance、verification_commands、goal_id、contextを保持する。TaskRunはprovider、base commitと、branch/worktree/workspace/receipt/log/result commitの任意参照を持ち、idle marker `idle.json`のpathは`run_dir`から導出する。claim時のproviderは`claude`のみで、リソース参照は作成前のためnullになる。

## Current operations

- `add`でdraftを作り、`draft → ready`、`ready → draft`、`draft/ready → canceled`を手動操作できる。
- `claim`だけが`ready → in_progress`へ遷移させる。同じトランザクションでclaimed状態のTaskRunとイベントを作り、supervisorからのclaimはそのrunの`RunLease`も作る。キュー全体の実行枠はなく、依存が解けたtaskは`supervise --parallel N`の上限まで同時に実行される。
- supervisorはrunを`claimed → starting`（path計画）→ `running`（agent起動）→ `validating`または`failed`（wrapper終了）→ `awaiting_integration`または`failed`（receipt検証）へ進め、`awaiting_integration`のworkspaceを閉じて`workspace_closed_at`を記録し、休止したrunの`RunLease`を解放する。各遷移はそのrunのleaseまたはwrapperの所有を要求する。runtime errorではsupervisorがそのrunだけを手放す（statusは変えず、`last_error`を書き、leaseを消す）。
- `integrate`だけが`awaiting_integration | needs_session → integrating`と、そこからの`→ integrated`（Taskは`in_progress → completed`、`result_commit`は`main`に積んだsquash commit）、`→ needs_session`（rebaseの衝突、再検証の失敗）、`→ failed`（セッションが書き直したreceiptが`failed`）、`→ 元のstatus`（mainを進める前のerror）を行う。`integrating`はqueue全体で1件。結果は`IntegrationOutcome`（`integrated` / `needs_session` / `failed` / `no_run_awaiting`）で返す。`integrate --next`は`awaiting_integration`のrunを検証完了の古い順に取り、`needs_session`は`integrate ID`で明示的に再開する。
- `in_progress`のTaskは、未完了run（claimed/starting/running/validating/awaiting_integration/integrating/needs_session）がある間は手動変更できない。すべてのrunが`failed`または`interrupted`になった`in_progress`は`ready`/`draft`/`canceled`へ手動で戻せる。再試行は新しいTaskRunになる。終端状態は変更できない。依存の追加・削除はdraft/readyだけに許可する。
- `recover`は未完了runを、そのrunの登録プロセスとleaseの所有者が停止していることを確認してから`interrupted`にする（`integrating`なら`awaiting_integration`へ戻す）。他のrunには触れない。Taskは`in_progress`のままで、`ready`への復帰は別操作。
- `succeeded`（統合を待たずに成功とする運用）への遷移はまだ公開していない。
- `candidates`は全依存がcompletedのready taskをID順で返す。claimはTaskごとの未完了run 1件の制約だけを再確認する。
- canceled、失敗、中断、統合待ち、着地中、セッション待ちは依存の完了条件を満たさない。awaiting_integration / integrating / needs_sessionのTaskはin_progressのまま保持し、integratedになった時点でcompletedになる。
- `RunEvent.run_id`はtask登録・依存変更などrun作成前のイベントではnullになる。goal単位のイベント（`goal_created`、`goal_updated`、`goal_closed`）は`task_id`がnullで`goal_id`を持ち、`goal show`に並ぶ。`task_goal_changed`はtaskのイベントで、`from`と`to`にgoal IDを持つ。
- `goal add`でgoalを作り、`add --goal ID`と`set-goal TASK GOAL`でtaskを所属させ、`set-goal TASK --none`で外す。所属の変更は依存の追加・削除と同じくdraft/readyのtaskだけに許し、閉じたgoalへの追加と付け替えは拒否する。`task_created`のpayloadは`goal_id`を持ち、`set-goal`は変化があったときだけ`task_goal_changed`を記録する。
- `goal edit`はtitle、description、acceptance、constraints、docを差し替え、`goal_updated`のpayloadに`old`と`new`のgoal全体を残す。閉じたgoalも編集できる（記録の訂正のため）。走行中のrunは`prompt.txt`のスナップショットのままで、次のclaimから新しい記述が使われる。
- `goal close ID --verdict achieved|abandoned`はverdictを1回だけ記録する。`achieved`は所属taskに`completed` / `canceled`以外があれば拒否し、`abandoned`は`in_progress`があれば拒否する（`GoalVerdict::allows`）。閉じたgoalを再び閉じることはできず、続きは新しいgoalに登録する。`goal_closed`のpayloadはverdictとclose時点のstatus別件数を持つ。
- `goal list`はgoalごとに`closed`、`verdict`、所属taskのstatus別件数（`TaskStatusCounts`）を返し、`goal show`はgoal、所属taskのid/title/status、goalのイベントを返す。
- scheduling（`candidates`、`claim`）はgoalを見ない。ID順のまま、goalをまたぐ依存も許す。

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
| runtime error / provisioning error | 変えない | errorの文をそのまま。worktree・workspaceの作成失敗は`run <run-id> provisioning failed: <error>`、監視中は`wrapper heartbeat expired; session may still be alive`、`session did not exit within 120s of the exit request; ...`、検証処理そのもの（Git・DB）のerror文。supervisorはそのrunのleaseを消して手放す（abandon）。wrapper自身のerrorとsupervisor heartbeatの失敗も同じ列に書くがleaseは残す（wrapperは子が死んでいれば続けて終了コード127を報告し、runは`session exited with code 127`で`failed`になる） | `runtime_error` |
| cleanup失敗 | 変えない | 受け入れたrunのworkspace closeの失敗は`workspace <workspace-id> could not be closed: <error>`、着地後のworktree/branch削除の失敗は`landed worktree <path> could not be removed: <error>` | `cleanup_failed` |
| 着地の保留・中断・失敗 | `integrating` → `needs_session` / 元のstatus / `failed` | rebaseの衝突や再検証の失敗の理由、mainを進める前のGit/DB errorは`integration stopped before main moved: <error>`、セッションが書き直した`failed` receiptの理由（[supervisor-lifecycle](supervisor-lifecycle.md#integrate)） | `integration_deferred` / `integration_error` / `integration_failed` |

`last_error`はstatusと最後のイベントに合わせて読む。`failed`なら非0終了・検証拒否・着地時の`failed` receipt、`claimed`/`starting`/`running`/`validating`で`last_error`があればsupervisorが手放したrun（`doctor`にleaseなしで出る）、`awaiting_integration`で`last_error`があればclose失敗（`cleanup_failed`、`workspace_closed_at`はnull）かmainを進める前に止まった着地（`integration_error`）、`needs_session`なら着地の衝突である。`show`・`doctor`・superviseの結果の`errors`に出て、`list`には出ない。`recover`は`last_error`を上書きしない。
