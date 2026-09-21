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

ステップ2で`Task`、`TaskDependency`、`TaskRun`、`RunEvent`を実装した。Rustの型と手動遷移規則は`src/domain.rs`、ストレージ契約は`src/application.rs`、永続化は`src/infrastructure/sqlite.rs`にある。`AgentSession`、workspace/processの独立エンティティと実行後の状態遷移はsupervisor実装時に追加する。

## Entities

- `Task`: ユーザーが登録する作業。公開statusを持つ。
- `TaskDependency`: taskからpredecessorへの有向辺。循環は禁止する。
- `TaskRun`: 1回の実行試行。provider、worktree、branch、結果、実行statusを持つ。
- `AgentSession`: providerが起動したセッション。プロセスとprovider固有識別子を持つ。
- `Workspace`: cmux workspace。TaskRunと1対1で関連し、実行後に閉じる。
- `ProcessLease`: supervisor、session wrapper、agentのPIDとheartbeatを追跡する。
- `RunEvent`: 実行中に発生した永続イベント。

`Task.id`はSQLiteの整数ID、`TaskRun.id`はUUID。Taskはtitle、description、acceptance、verification_commandsを保持する。TaskRunはprovider、base commitと、branch/worktree/workspace/receipt/log/result commitの任意参照を持つ。claim時のproviderは`claude`のみで、リソース参照は作成前のためnullになる。

## Current operations

- `add`でdraftを作り、`draft → ready`、`ready → draft`、`draft/ready → canceled`を手動操作できる。
- `claim`だけが`ready → in_progress`へ遷移させる。同じトランザクションでclaimed状態のTaskRunとイベントを作る。
- `in_progress`または終端状態のTaskは手動変更できない。依存の追加・削除もdraft/readyだけに許可する。
- Taskの`completed`への遷移とrunの成功・失敗・回復はまだ公開していない。mainへの統合検証を実装してから接続する。
- `candidates`は全依存がcompletedのready taskをID順で返す。同時実行枠の空きはclaimで再確認する。
- canceled、失敗、中断、統合待ちは依存の完了条件を満たさない。awaiting_integrationのTaskはin_progressのまま保持する。
- `RunEvent.run_id`はtask登録・依存変更などrun作成前のイベントではnullになる。

## Invariants

- Taskは自分自身に依存できない。
- 依存グラフは循環しない。
- `in_progress`はschedulerがclaimしたTaskだけが持つ。
- TaskRunが成功するには完了レシート、コミット、clean worktreeが必要。
- workspaceを閉じる前にTaskRunをcleanedにしない。
- agentの異常終了だけでTaskを自動再実行しない。
- 実装途中のprovider fallbackは行わず、起動不能など安全に判定できる場合だけfallbackする。
