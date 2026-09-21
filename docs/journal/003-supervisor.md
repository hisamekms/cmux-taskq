---
id: journal-003
type: journal
title: Single-run supervisor
status: done
created: 2026-09-22
updated: 2026-09-22
plan_step: 3
queue_task: null
verify:
  - cargo fmt --all --check
  - cargo test --locked
  - cargo clippy --locked --all-targets -- -D warnings
related:
  - plan-rust-runtime-mvp
  - design-supervisor-lifecycle
---

# 003: Single-run supervisor

## Goal

[plans/current.md](../plans/current.md) ステップ3。CLIで登録した小さなtaskが独立worktree内のClaude Codeで実行され、進行状態とログを確認できる。起動元のClaude Code終了に監視が依存しない。

## Log

### 2026-09-22 07:00 codex

- cmux workspace `TASKQ-CODEX` で実装。`src/runtime.rs`、`src/infrastructure/adapters.rs`、`src/infrastructure/runtime_store.rs`、migration `0002_supervisor.sql`、`tests/runtime.rs`
- Git worktreeを使った正常終了・異常終了のruntimeテストが通過。終了コード0でも`validating`止まりで、成果とworkspaceは保持

### 2026-09-22 07:58 codex

- 使い捨てrepositoryで実機スモーク開始。一時領域 `/private/var/folders/3p/g_cty8k11wqf2pg43pwc05g40000gn/T/taskq-supervisor-smoke-j5if6aru`（`launch-command.txt`、`supervisor.log`、`supervisor-result.json`、`queue.db.runs/`を含む。一時領域なので消える）
- cmux `workspace:9` = supervisor、`workspace:10` = Claude（UUID `AA59D4B4-8781-4BD4-AABB-CA03FD272191`）、run `6083a813-ca27-48f8-a1ba-ac84d81449ae`
- 新規worktreeの信頼確認でClaudeが待機。その間もsupervisorとwrapperのheartbeatは更新され、入力待ちを異常終了と誤判定しない。`cmux send-key Down / Return` で確認を進めた
- Claudeが修正・`python3 -m unittest -v`・commit `15227c6`・receipt提出。receipt後もセッション維持。`cmux send '/exit'` で終了 → `session_exited` → `supervision_finished` → run `validating`
- 「結果を文書に残し、最後のチェック」の直前でCodexの5h制限（12:06復活）。docs未反映、未コミットのまま停止

### 2026-09-22 08:20 claude

- 引き継ぎ。Codexの状態は `cmux read-screen --workspace workspace:3 --scrollback` からしか分からなかった。これがジャーナル導入（004）の動機
- コードは完成しており fmt / clippy / テスト23件は通過済み。スモークのDB・receipt・イベント列を確認し、設計どおりと判断
- `supervise` のオプションに説明を追加。README、plans/current.md、design 5文書を実装に合わせて更新
- commit `8a3c746`、push。workspace 9 / 10 を `cmux workspace close --workspace workspace:N` で閉じた
- 観測: `cmux list-workspaces` はdeprecated alias。`cmux workspace list` が正。`~/.cargo/config` のdeprecated警告はこのrepoと無関係

## Result

commit `8a3c746`。`supervise`がlease取得 → claim → run管理領域とworktree作成 → cmux workspace作成 → `session` wrapper経由でClaude起動 → heartbeat監視 → セッション終了検知までを行う。runtimeテスト8件を追加。実機スモークで信頼確認待ち中のheartbeat継続、receiptとセッション終了の分離、リソース保持を確認。

未検証: Claude異常終了、supervisor再起動、heartbeat切れ後の復旧、cleanup失敗。ステップ4へ。

## Promoted

- [design/supervisor-lifecycle.md](../design/supervisor-lifecycle.md): `supervise` と `session` の手順、失敗時の保持ルール
- [design/persistence.md](../design/persistence.md): schema v2、所有権ルール
- [design/provider-lifecycle.md](../design/provider-lifecycle.md): Claude adapterの引数
- [plans/current.md](../plans/current.md): ステップ3完了、未検証事項をステップ4へ
