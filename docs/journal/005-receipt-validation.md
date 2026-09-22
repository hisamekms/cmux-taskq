---
id: journal-005
type: journal
title: Receipt validation
status: done
created: 2026-09-22
updated: 2026-09-22
plan_step: 4
queue_task: null
verify:
  - cargo fmt --all --check
  - cargo test --locked
  - cargo clippy --locked --all-targets -- -D warnings
related:
  - design-supervisor-lifecycle
---

# 005: Receipt validation

## Goal

[plans/current.md](../plans/current.md) ステップ4。wrapper終了後の`validating` runについて、supervisorがreceiptを検証して`awaiting_integration`または`failed`へ遷移させる。

- receiptのrun ID、`result`、commit SHAの整合性を確認する。
- commitがrunのbranchのHEADで、base commitからの履歴に含まれ、worktreeがcleanであることをGitで確認する。
- taskの`verification_commands`をsupervisor側でworktree内で実行し、結果を`run_events`に記録する。receiptの自己申告だけで成功にしない。
- `not_applicable`のtests/e2e/subagent_reviewには理由を要求し、記録する。
- 検証結果とresult commitをTaskRunに保存する。失敗時はworkspaceとworktreeを保持する。

完了条件: 正しいreceiptが`awaiting_integration`になり、commitなし・dirty worktree・検証コマンド失敗・run ID不一致・receipt未提出がそれぞれ`failed`になることをテストで確認できる。

## Log

### 2026-09-22 10:30 claude

- worker として開始。branch `journal/005-receipt-validation`、worktree `.worktrees/005-receipt-validation`
- 決定: 検証は `supervise` の中で `finish_supervision` が `validating` を返した直後に自動実行する。別コマンドにしない理由: (1) lease と heartbeat をそのまま使えるので検証中も所有権が途切れない。(2) `acquire_supervisor` は未完了 run（validating 含む）があると開始を拒むため、別コマンドにするとこの規則に例外を作ることになる。(3) ADR-0003 は完了検証を supervisor の責務としている。再検証・復旧の入口は 009 の `recover` に任せる
- 決定: 新テーブルは作らない。result commit は既存の `task_runs.result_commit`、理由は `last_error`、検証コマンドの結果と receipt の内容は `run_events`（`verification_command`、`validation_finished`）に置く。persistence.md が予告していた `run_artifacts` は不要と判断
- receipt の契約を揃える: `tests` / `e2e` / `subagent_review` を全て `{"status":"passed|failed|not_applicable","evidence_or_reason":"..."}` にする。旧 prompt は `tests` だけ配列だったが、not_applicable の理由を一様に要求できないため変更。prompt も更新

### 2026-09-22 11:30 claude

- 実装: `domain::Receipt` / `Receipt::check`（構造の整合性）、`GitRepository::{current_branch, head, is_ancestor, status}`、`adapters::run_shell_to_log`（検証コマンドを `/bin/sh -c` で実行し `<run-dir>/verify-N.log` へ出力、30分タイムアウト）、`SqliteQueue::finish_validation`（`validating` かつ同じ token のみ遷移）、`runtime::validate` / `check_receipt`
- 判定（拒否 → `failed`）と検証処理のエラー（Git/DB 障害 → `Err`、run は `validating` のまま、lease も保持）を `Result<Result<_, Rejection>>` で分けた。判定はここで打ち切り、最初に外れた理由だけを `last_error` に残す
- 検証コマンドの `adapters::output` は 30 秒で kill するので流用せず、`capture`（status を返す）と `wait_with_deadline` に分解した。`merge-base --is-ancestor` と `symbolic-ref` は exit 1 を「偽」として扱う必要があるため `capture` を使う
- worktree の clean 判定は `--untracked-files=all`。untracked も dirty。テストは untracked ファイルで確認
- `supervise` の JSON は `outcome: "session_exited"` から `"finished"` に変更（検証まで含むため）。README を更新
- テスト: `tests/runtime.rs` の TestProvider を「shell script を渡す」形に変え、prelude で `receipt COMMIT [RUN_ID]` / `commit MSG` を定義。正常・receipt なし・run_id 不一致・commit なし（receipt が base を指す）・branch head でない commit・dirty・検証コマンド失敗（seed.txt を削除して commit）・非0終了時は検証しない、の各ケースと `Receipt::check` の単体ケースを追加。runtime 15件、全体 30件通過。fmt / clippy 通過
- 未検証: 実機（cmux + Claude）での receipt 提出は新形式でまだ試していない。010 のスモークで確認する

## Result

`supervise` が `validating` の run に対して receipt の構造、run branch の HEAD と base からの履歴、clean worktree を Git で確認し、task の `verification_commands` を worktree で再実行してから `awaiting_integration` または `failed` へ遷移させる。`result_commit` と `last_error` を `task_runs` に、検証コマンドの結果と receipt の内容を `run_events`（`verification_command`、`validation_finished`）に保存する。失敗時も workspace / worktree / branch は保持し、lease だけ解放する。migration は不要（schema version 2 のまま）。

完了条件の5ケース（正常 → `awaiting_integration`、commit なし・dirty・検証コマンド失敗・run ID 不一致・receipt 未提出 → `failed`）に加え、branch head でない commit、self-report failed、理由なし not_applicable をテストで確認。

未検証: 新しい receipt 形式での実機スモーク（010 で行う）。検証コマンドのタイムアウト（30分）は実機で未確認。

## Promoted

- [design/supervisor-lifecycle.md](../design/supervisor-lifecycle.md): `supervise` 手順10、receipt 形式、Validation 節（確認順序と保存先）
- [design/persistence.md](../design/persistence.md): `result_commit` / `last_error` の意味、検証イベント、`run_artifacts` を作らない判断
- [design/domain-model.md](../design/domain-model.md): `Receipt` エンティティ、run の遷移、成功の不変条件
- [design/overview.md](../design/overview.md): 実装状況
- [README.md](../../README.md): receipt 形式と検証の流れ
- ADR は追加しない。検証を supervisor が行うことは ADR-0003 の決定に含まれる
