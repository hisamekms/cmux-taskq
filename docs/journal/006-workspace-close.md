---
id: journal-006
type: journal
title: Workspace close after validation
status: done
created: 2026-09-22
updated: 2026-09-22
plan_step: 4
queue_task: null
depends_on_journal: [5]
verify:
  - cargo fmt --all --check
  - cargo test --locked
  - cargo clippy --locked --all-targets -- -D warnings
related:
  - design-supervisor-lifecycle
  - adr-0003
---

# 006: Workspace close after validation

## Goal

[plans/current.md](../plans/current.md) ステップ4。検証成功とセッション終了を確認した後にsupervisorがcmux workspaceを閉じ、worktreeとbranchは統合まで残す。

- `WorkspaceBackend`に`close`を追加し、cmuxの応答を検証する。
- close失敗は`cleanup_failed`として記録し、closeされていないworkspaceをcleaned扱いにしない。
- 失敗・中断・heartbeat切れではworkspaceを閉じない。

完了条件: 成功runでworkspaceが閉じられworktreeが残ること、close失敗が記録され状態が進まないことをテストで確認できる。

## Log

### 2026-09-22 claude

- worker として開始。branch `journal/006-workspace-close`、worktree `.worktrees/006-workspace-close`。007-009 が並行しているので `docs/plans/current.md` と `overview.md` は触らず、差分を `WorkspaceBackend::close` 周辺に閉じる
- cmux 0.64.25 の応答を使い捨て workspace で確認: `cmux workspace close <UUID>` は `OK workspace:N`（exit 0）。存在しない UUID や二重 close は `Error: not_found: Workspace not found`（exit 1）。UUID で閉じられるので保存済みの `workspace_id` をそのまま渡し、応答は `workspace_handle` で `OK workspace:N` を検証する
- 決定: 「workspace がまだ開いている」の表現は `task_runs.workspace_closed_at INTEGER`（migration `0003_workspace_close.sql`、schema version 3）にする。理由: (1) 現在状態は `task_runs` の列（`workspace_id` / `result_commit` / `last_error`）、履歴は `run_events` という既存の分担に合わせる。(2) 後続の `doctor` / `recover`（009）や統合確認（008）が「閉じていない workspace」を 1 クエリで拾える。イベント由来だと `workspace_closed` と `cleanup_failed` の最新を run ごとに追う必要があり、再試行で `cleanup_failed` の後に `workspace_closed` が来る順序依存も生む。(3) NULL が「未 close」なので、close 成功の DB 書き込みに失敗しても安全側（開いている扱い）に倒れる
- 決定: close は `supervise` 内で `finish_validation` が `awaiting_integration` を返した直後、同じ lease の下で行う。`failed`（非0終了・検証拒否）、provisioning エラー、wrapper heartbeat 切れでは呼ばない。close 失敗は `cleanup_failed` イベントと `last_error` に残し、run は `awaiting_integration` のまま、lease は解放する（セッションは既に終了しているので保持の理由がない）

- 実装: `WorkspaceBackend::close`（`Cmux` は `cmux workspace close <UUID>` を `output` で呼び、`workspace_handle` で `OK workspace:N` を検証）、migration `0003_workspace_close.sql`（`task_runs.workspace_closed_at INTEGER`）、`SqliteQueue::workspace_closed` / `cleanup_failed`（`awaiting_integration` かつ同じ token かつ `workspace_closed_at IS NULL` の run にだけ書ける。close の記録は一度きり）、`runtime::close_workspace`（`validate` の後に `AwaitingIntegration` のときだけ呼ぶ）
- `cleanup_failed` は `Err` にしない。セッションは終了済みで保持する理由がなく、lease を残すと次の `supervise` が始められないため。run の JSON に `last_error` と `workspace_closed_at: null` が出るので operator はそこで気づく。cmux は閉じたが DB 書き込みが失敗した場合だけ `Err` になり、`runtime_error` と lease 保持の既存経路に乗る（列は null のまま = 開いている扱いで安全側）
- テスト: `run_agent_with` が全ケースで「worktree が残る」「`awaiting_integration` のときだけ close が呼ばれ `workspace_closed` が記録される」「それ以外では close されない」を検証。close 注入失敗（run は `awaiting_integration` のまま、`cleanup_failed` + `last_error`）、非0終了で close されない、store の guard（running 中・failed・二重 close・他 token を拒否、`cleanup_failed` の後の close 成功を許す）を追加。schema version の期待値を 3 に更新。runtime 17件、全体 32件通過。fmt / clippy / `cargo llvm-cov --fail-under-lines 80`（85.99%）通過
- 触らなかった: `docs/plans/current.md`（ステップ4はまだ途中）、`docs/design/overview.md`（「workspace終了は後続実装」の一文が古くなるが、並行 worker との衝突を避けるため SV の merge 時に直してもらう）
- 未検証: 実機（cmux + Claude）で supervise 経由の close はまだ流していない。close コマンド単体の応答は上記の通り確認済み。010 のスモークで通す

## Result

`supervise` が receipt 検証を通った `awaiting_integration` の run に対して `cmux workspace close <workspace_id>` を呼び、`OK workspace:N` を確認して `workspace_closed` イベントと `task_runs.workspace_closed_at`（migration 0003、schema version 3）を記録する。worktree と branch は統合まで残る。close 失敗は `cleanup_failed` イベントと `last_error` に残し、run は `awaiting_integration`、`workspace_closed_at` は null のまま（= 開いている扱い）。`failed`、provisioning エラー、heartbeat 切れでは close を呼ばない。

完了条件（成功 run で workspace が閉じられ worktree が残る、close 失敗が記録され状態が進まない）と、失敗 run で close されないことをテストで確認。

未検証: 実機での supervise 経由の close（010 で行う）。

## Promoted

- [design/supervisor-lifecycle.md](../design/supervisor-lifecycle.md): `supervise` 手順11、Cleanup and recovery 節（close の対象、`workspace_closed_at` の意味、失敗時の扱い）
- [design/persistence.md](../design/persistence.md): schema version 3、`workspace_closed_at` 列、イベント由来にせず列に持つ理由
- [design/domain-model.md](../design/domain-model.md): `Workspace` エンティティ、run の遷移、cleaned 扱いの不変条件
- [README.md](../../README.md): supervise の流れと close 失敗時の挙動
- ADR は追加しない。workspace の終了を supervisor が行うことは ADR-0003 の決定に含まれる
