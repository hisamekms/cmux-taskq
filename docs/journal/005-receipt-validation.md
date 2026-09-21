---
id: journal-005
type: journal
title: Receipt validation
status: planned
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

## Result

## Promoted
