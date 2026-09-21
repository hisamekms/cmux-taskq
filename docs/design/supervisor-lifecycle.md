---
id: design-supervisor-lifecycle
type: design
title: Supervisor and workspace lifecycle
status: current
created: 2026-09-21
updated: 2026-09-22
last_verified: 2026-09-22
scope: runtime
related:
  - adr-0002
  - adr-0003
  - design-persistence
  - design-provider-lifecycle
---

# Supervisor and workspace lifecycle

```text
ready task
  → claim TaskRun
  → create Git worktree
  → create cmux workspace
  → start session wrapper
  → start Claude/Codex
  → running
  → completion receipt
  → validate commit, tests, clean state
  → close cmux workspace
  → awaiting_integration / succeeded
```

## Implementation status

ステップ3で`claim`から`running`、セッション終了検知までを`src/runtime.rs`に実装した。receiptの検証、workspaceの終了、`awaiting_integration`への遷移、`doctor`/`recover`はステップ4で追加する。

## `supervise`

`cmux-taskq --db PATH supervise --repo REPO`は専用ターミナルで実行し、1件だけ処理して終了する。

1. DBのpathを正規化し、repositoryのroot、Git common directory、`refs/heads/main`のcommitを取得する。DBはworktree外か、common directory配下に置く。
2. cmux（`ping`）とClaude（`--version`）のpreflightを行う。
3. supervisor leaseを取得する。既存leaseがある、未完了runがある、DBが別repositoryに束縛されている場合は開始しない。staleなleaseも自動では奪わない。
4. 別スレッドで2秒ごとにleaseのheartbeatを更新する。heartbeatの失敗は監視ループで検知し、runを保持したまま終了する。
5. `main`をbase commitとしてclaimする。候補がなければleaseを解放して`no_ready_task`を返す。
6. run管理領域`<db>.runs/<run-id>/`のpath、branch `taskq/<run-id>`、worktree、receipt、logのpathを`run_planned`として先にDBへ保存し、その後にディレクトリ、`prompt.txt`、runtimeバイナリのスナップショット`runner`、worktreeを作る。
7. cmux workspaceを`--cwd worktree --command '<runner> --db ... session --run ... --lease ... --claude ...'`で作成し、`identify`で解決したUUIDを`workspace_created`として保存する。
8. 監視ループで、wrapperの登録（45秒以内）、wrapper heartbeat（30秒以内）、receiptファイルの出現、wrapperの終了を確認する。receiptの出現は`receipt_observed`（`validated: false`）として記録するだけで、セッション終了とは別に扱う。
9. wrapper終了後に画面を`terminal-final.txt`へ保存し、`supervision_finished`でrunを終了コード0なら`validating`、それ以外なら`failed`にしてleaseを解放する。Taskは`in_progress`のまま残す。

作成や通信に失敗した場合は、セッションが生きている可能性があるためleaseを解放せず、リソースも削除しない。`last_error`と`runtime_error`イベントに原因を記録し、`show`と`status`で確認する。

## `session` wrapper

cmux workspaceが起動する隠しコマンド。TTYが必要で、パイプからは起動しない。

1. `workspace_id`が保存されるまで待ち（45秒以内）、wrapperのPIDを一度だけ登録する。leaseが無効なら登録できない。
2. `prompt.txt`を読み、providerのコマンドでagentを起動して`agent_started`を記録し、runを`running`にする。
3. 1秒ごとにheartbeatを更新しながら子プロセスをwaitする。DB障害中も子プロセスの所有を手放さない。
4. 終了コードを`session_exited`として記録する。agent起動後のエラーでは子プロセスが生きている可能性を考慮し、終了を記録しない。

## Receipt and session exit

receiptの受領とセッション終了は別の事象である。agentはreceiptを`<run-dir>/receipt.json`へ一時ファイルからrenameして公開し、応答完了後もセッションを維持する。operatorがClaudeの応答完了を確認して`/exit`を送り、wrapperが終了コードを記録した後にsupervisorが監視を終える。receipt受領後の終了要求の自動化はステップ4以降で扱う。

## Cleanup and recovery

workspace削除はsupervisorが行う。成功時はworkspaceだけを削除し、`integrated`ではworktreeとbranchをmainへの反映まで残す。失敗・中断・heartbeat切れの場合は調査のためworkspaceとworktreeを残す。

supervisorの再起動ではleaseとheartbeatを確認し、孤児プロセスを勝手に再実行しない。ユーザーが`recover`で明示的に復旧した後に新しいTaskRunを作る。
