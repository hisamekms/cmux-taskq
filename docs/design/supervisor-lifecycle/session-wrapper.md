---
id: design-supervisor-lifecycle-session-wrapper
type: design
title: "`session` wrapper"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
---

# `session` wrapper

cmux workspaceが起動する隠しコマンド。TTYが必要で、パイプからは起動しない。ユースケースは`src/application/session.rs`の`run_session`で、queue（`Queue`）、agentのコマンド（`AgentProvider`）、その起動（`Spawner`、標準入出力は端末を継承）、`prompt.txt`の読み取り（`RunFiles`）とwrapperのpidを`Session`として受け取る。`runtime::session`はTTYを確かめ、`SqliteQueue`、`ClaudeCode`、`LocalSpawner`、`LocalRunFiles`を渡す入口だけ。

1. `workspace_id`が保存されるまで待ち（45秒以内）、wrapperのPIDを一度だけ登録する。leaseが無効なら登録できない。
2. `prompt.txt`を読み、providerのコマンドでagentを起動して`agent_started`を記録し、runを`running`にする。
3. 1秒ごとにheartbeatを更新しながら子プロセスをwaitする。DB障害中も子プロセスの所有を手放さない。
4. 終了コードを`session_exited`として記録する。agent起動後のエラーでは子プロセスが生きている可能性を考慮し、終了を記録しない。
