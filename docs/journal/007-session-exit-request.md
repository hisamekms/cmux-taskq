---
id: journal-007
type: journal
title: Session exit request after receipt
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
  - journal-001
  - design-supervisor-lifecycle
---

# 007: Session exit request after receipt

## Goal

[plans/current.md](../plans/current.md) ステップ4。receipt受領後、operatorの手動`/exit`に頼らずにセッション終了を要求する。

- receipt受領後にClaudeが応答完了・idleであることを画面文言以外の根拠（hookまたはプロセス状態）で確認する方法を決める。
- 終了要求を送り、wrapperの終了コードで終了を確認する。応答しない場合は強制終了せず、runを保持して人に知らせる。
- 手動`/exit`の経路は残す。

完了条件: 使い捨てrepositoryで、receipt提出から人の操作なしに`session_exited`まで進み、idle判定の根拠がイベントに記録される。

## Log

### 2026-09-22 09:10 claude

- worker として開始。branch `journal/007-session-exit-request`、worktree `.worktrees/007-session-exit-request`
- idle 信号の候補を比較した:
  1. **Claude Code の `Stop` hook**（採用）: 応答完了ごとに発火する。run 専用の settings JSON `<run-dir>/claude-settings.json` を Claude adapter が書き、`--settings <path>` で渡す。hook command は stdin の JSON を `<run-dir>/idle.json` へ一時ファイル + rename で書く。supervisor は receipt を観測した後、`idle.json` の mtime が `receipt.json` の mtime 以上なら「receipt 提出後に応答が完了した」と判定する。receipt より古い marker（質問で止まった turn など）は無視する
  2. `SessionEnd` hook: セッション終了の通知であり idle の根拠にならない。終了確認は wrapper の終了コード（`session_exited`）で足りる
  3. プロセス状態（CPU 使用率、子プロセスの有無）: 応答待ちと入力待ちを区別できず、権限確認中も idle に見える
  4. 画面文言（`❯` プロンプトの検出）: 指示どおり不採用。UI 変更で壊れる
- 実機確認: Claude Code 2.1.278 で `claude -p --settings <file> --session-id <uuid>` に Stop hook を渡すと、hook の stdin に `{"session_id":"<uuid>","hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"ok","cwd":...}` が来て marker が書けた。`--settings` の hook はユーザー承認なしで有効
- 制限: 権限確認や質問で止まっている turn では Stop は発火しない（turn の途中）ため、receipt 後に権限待ちになると終了要求は送らない。ユーザー settings の `disableAllHooks` は marker を止め、その場合は従来どおり手動 `/exit` 待ちになる
- 終了要求は `WorkspaceBackend::send_exit(workspace_id)`。cmux は `send --workspace <uuid> '/exit'` の後 `send-key --workspace <uuid> enter`。1 回だけ送り、再送や kill はしない
- タイムアウト: 要求後 120 秒（`WorkspaceBackend::exit_timeout`、テストでは短縮）で wrapper が終了しなければ `exit_request_timed_out` を記録し、runtime error として supervise を終える。run は `running`、lease も保持し、人が `/exit` するか 009 の recover で扱う
- schema 変更なし。idle marker の path は `run_dir` から導出し DB 列は増やさない

### 2026-09-22 09:35 claude

- 実装: `WorkspaceBackend::{send_exit, exit_timeout}`（application）、`Cmux::send_exit`（`send -- /exit` → `send-key -- enter`）、`ClaudeCode::command` が `<run-dir>/claude-settings.json` を書いて `--settings` で渡す、`adapters::stop_hook_settings`、`TaskRun::idle_marker_path`（`<run-dir>/idle.json`）、`runtime::idle_after_receipt`（marker mtime ≥ receipt mtime）。監視ループは receipt 観測後に idle を見つけたら `session_idle_observed` → `send_exit` → `exit_requested`、要求後 `exit_timeout` 経過で `exit_request_timed_out` を記録して `bail!`（run/lease/リソースは保持）。prompt の末尾を「/exit は自分で打たない。idle になれば supervisor が終了する」に変更
- テスト（`tests/runtime.rs`）: TestWorkspace に `send_exit`（`<run-dir>/exit-requested` を置く）と `exit_timeout` を追加。prelude に `idle`（Stop hook の模倣）と `await_exit`。追加ケース: idle marker → 終了要求 → `awaiting_integration`（イベント順序と根拠を確認）、marker なし / receipt より古い marker → 要求しない、要求無視 → 2 秒でタイムアウト、run は `running` のまま lease 保持、後から `session_exited` が記録されても再実行しない、Claude の settings ファイルと hook command の実行（apostrophe 入り path）。runtime 19 件、全体 34 件通過。fmt / clippy 通過、llvm-cov 行カバレッジ 86.5%
- 実機スモーク（使い捨て repository、cmux 0.64.25、Claude Code 2.1.278、auto mode）: 一時領域 `<scratchpad>/smoke`（消える）。workspace `C0130DD6-3B89-4502-8537-429A8BC4061A`（workspace:38、確認後に閉じた）、run `6b6c33f6-36b5-4f55-a3cc-ba671d840a15`。worktree の信頼確認だけ `send-key down / enter` で通し、以後は人の操作なしに `receipt_observed` 00:30:12 → `session_idle_observed` 00:30:17（`hook_event_name: Stop`、`session_id` = run ID、marker mtime ≥ receipt mtime）→ `exit_requested` → `session_exited` 00:30:18（exit 0）→ `validation_finished` `awaiting_integration`。`terminal-final.txt` に Claude の `/exit` 後の "Resume this session with" と wrapper の JSON が残った
- 観測: 実機では receipt 提出から Stop 発火まで約 5 秒（receipt 後に短い報告を出してから turn が終わる）。`--settings` の hook は信頼確認とは別で、承認ダイアログは出なかった
- 未実装・未検証: タイムアウト経路の実機（テストのみ）。timeout 中に operator が `/exit` した場合、wrapper は `session_exited` を記録するが supervisor は既に終了しているため `supervision_finished` は 009 の `recover` で扱う必要がある。006 の close は `exit_request_timed_out` の run を閉じてはいけない（run は `running` のまま）

## Result

receipt受領後、Claude adapterが`--settings`で渡す`Stop` hookの書く`<run-dir>/idle.json`がreceiptより新しいことをidleの根拠として`session_idle_observed`に記録し、`WorkspaceBackend::send_exit`（cmuxは`send -- /exit` + `send-key -- enter`）で一度だけ終了を要求して`exit_requested`を記録する。終了はwrapperの`session_exited`で確認する。120秒（`exit_timeout`）で終了しなければ`exit_request_timed_out`を記録し、runを`running`、lease・workspace・worktreeを保持したまま`supervise`をエラー終了して人に委ねる。強制終了はしない。手動`/exit`はいつでも有効で、markerがなければ従来どおり手動終了を待つ。migrationは不要（schema version 2のまま）。

テストで idle marker → 終了要求 → `awaiting_integration`、marker なし / 古い marker → 要求なし、タイムアウト → 記録と run 保持、hook settings の内容と実行を確認。使い捨てrepositoryの実機スモークで、receipt提出から人の操作なしに`session_exited`まで進み、idle判定の根拠がイベントに残ることを確認した。

未検証: タイムアウト経路の実機。timeout後の手動`/exit`で`session_exited`は記録されるが`supervision_finished`は記録されないため、009の`recover`で扱う。

## Promoted

- [design/supervisor-lifecycle.md](../design/supervisor-lifecycle.md): 監視ループ手順8、Receipt and session exit 節（idle判定・終了要求・タイムアウト）
- [design/provider-lifecycle.md](../design/provider-lifecycle.md): Claude adapterの`--settings`とStop hook、他providerの契約
- [design/persistence.md](../design/persistence.md): idle markerとsettingsは列を持たない、終了要求のイベント
- [design/domain-model.md](../design/domain-model.md): `TaskRun::idle_marker_path`
- [README.md](../../README.md): 自動終了要求とタイムアウト時の扱い
- ADRは追加しない。終了要求をsupervisorが行うことはADR-0003の決定に含まれ、hookの選択は設計文書で足りる
