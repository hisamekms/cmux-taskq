---
id: design-domain-model
type: design
title: Domain model
status: current
created: 2026-09-21
updated: 2026-09-22
last_verified: 2026-09-22
scope: domain
related:
  - adr-0003
  - adr-0004
  - design-persistence
---

# Domain model

## Implementation status

ステップ2で`Task`、`TaskDependency`、`TaskRun`、`RunEvent`、ステップ3で`RunProcess`と`SupervisorLease`、ステップ4で`Receipt`を実装した。Rustの型と手動遷移規則は`src/domain.rs`、ストレージとprovider/workspaceの契約は`src/application.rs`、永続化は`src/infrastructure/sqlite.rs`と`src/infrastructure/runtime_store.rs`にある。`AgentSession`と`Workspace`は独立エンティティにせず、TaskRunの`id`（Claude session ID）と`workspace_id`で表す。

## Entities

- `Task`: ユーザーが登録する作業。公開statusを持つ。
- `TaskDependency`: taskからpredecessorへの有向辺。循環は禁止する。
- `TaskRun`: 1回の実行試行。provider、worktree、branch、結果、実行statusを持つ。
- `AgentSession`: providerが起動したセッション。プロセスとprovider固有識別子を持つ。
- `Workspace`: cmux workspace。TaskRunと1対1で関連し、receipt検証を通った後にsupervisorが閉じる。閉じたことの確認は`TaskRun.workspace_closed_at`で持つ。
- `SupervisorLease`: キューを所有するsupervisorのPIDとheartbeat。
- `RunProcess`: runごとのsession wrapperとagentのPID、heartbeat、終了コード。
- `RunEvent`: 実行中に発生した永続イベント。
- `Receipt`: agentが提出する完了レシート。run ID、結果、commit、tests/e2e/subagent_reviewの状態と証跡または理由、要約を持つ。構造の整合性は`Receipt::check`、Gitと検証コマンドの確認はsupervisorが行う。

`Task.id`はSQLiteの整数ID、`TaskRun.id`はUUID。Taskはtitle、description、acceptance、verification_commandsを保持する。TaskRunはprovider、base commitと、branch/worktree/workspace/receipt/log/result commitの任意参照を持ち、idle marker `idle.json`のpathは`run_dir`から導出する。claim時のproviderは`claude`のみで、リソース参照は作成前のためnullになる。

## Current operations

- `add`でdraftを作り、`draft → ready`、`ready → draft`、`draft/ready → canceled`を手動操作できる。
- `claim`だけが`ready → in_progress`へ遷移させる。同じトランザクションでclaimed状態のTaskRunとイベントを作る。
- supervisorはrunを`claimed → starting`（path計画）→ `running`（agent起動）→ `validating`または`failed`（wrapper終了）→ `awaiting_integration`または`failed`（receipt検証）へ進め、`awaiting_integration`のworkspaceを閉じて`workspace_closed_at`を記録する。各遷移はleaseまたはwrapperの所有を要求する。
- `in_progress`のTaskは、未完了run（claimed/starting/running/validating/awaiting_integration）がある間は手動変更できない。すべてのrunが`failed`または`interrupted`になった`in_progress`は`ready`/`draft`/`canceled`へ手動で戻せる。再試行は新しいTaskRunになる。終端状態は変更できない。依存の追加・削除はdraft/readyだけに許可する。
- `recover`は未完了runを、登録プロセスとsupervisorが停止していることを確認してから`interrupted`にする。Taskは`in_progress`のままで、`ready`への復帰は別操作。
- Taskの`completed`への遷移と`awaiting_integration`以降のrunの遷移はまだ公開していない。mainへの統合検証を実装してから接続する。
- `candidates`は全依存がcompletedのready taskをID順で返す。同時実行枠の空きはclaimで再確認する。
- canceled、失敗、中断、統合待ちは依存の完了条件を満たさない。awaiting_integrationのTaskはin_progressのまま保持する。
- `RunEvent.run_id`はtask登録・依存変更などrun作成前のイベントではnullになる。

## Invariants

- Taskは自分自身に依存できない。
- 依存グラフは循環しない。
- `in_progress`はschedulerがclaimしたTaskだけが持つ。
- TaskRunが成功するには完了レシート、base commitの上に積まれたbranch headのコミット、clean worktree、supervisorが再実行した検証コマンドの成功が必要。receiptの自己申告だけでは成功しない。
- workspaceを閉じる前にTaskRunをcleanedにしない。閉じたことをcmuxの応答で確認して`workspace_closed_at`に記録するまでは開いている扱いで、close失敗はrun状態を変えない。
- agentの異常終了だけでTaskを自動再実行しない。孤児runの復旧と再試行はどちらも明示操作。
- 実装途中のprovider fallbackは行わず、起動不能など安全に判定できる場合だけfallbackする。
