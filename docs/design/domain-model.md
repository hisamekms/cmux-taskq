---
id: design-domain-model
type: design
title: Domain model
status: current
created: 2026-09-21
updated: 2026-09-21
last_verified: 2026-09-21
scope: domain
related:
  - adr-0003
  - adr-0004
  - design-persistence
---

# Domain model

## Entities

- `Task`: ユーザーが登録する作業。公開statusを持つ。
- `TaskDependency`: taskからpredecessorへの有向辺。循環は禁止する。
- `TaskRun`: 1回の実行試行。provider、worktree、branch、結果、実行statusを持つ。
- `AgentSession`: providerが起動したセッション。プロセスとprovider固有識別子を持つ。
- `Workspace`: cmux workspace。TaskRunと1対1で関連し、実行後に閉じる。
- `ProcessLease`: supervisor、session wrapper、agentのPIDとheartbeatを追跡する。
- `RunEvent`: 実行中に発生した永続イベント。

## Invariants

- Taskは自分自身に依存できない。
- 依存グラフは循環しない。
- `in_progress`はschedulerがclaimしたTaskだけが持つ。
- TaskRunが成功するには完了レシート、コミット、clean worktreeが必要。
- workspaceを閉じる前にTaskRunをcleanedにしない。
- agentの異常終了だけでTaskを自動再実行しない。
- 実装途中のprovider fallbackは行わず、起動不能など安全に判定できる場合だけfallbackする。
