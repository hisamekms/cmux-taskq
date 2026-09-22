---
id: journal-015
type: journal
title: End-to-end happy path with cmux and a stub agent
status: planned
created: 2026-09-22
updated: 2026-09-22
plan_step: 4
queue_task: null
depends_on_journal: [6, 8]
verify:
  - cargo fmt --all --check
  - cargo test --locked
  - cargo clippy --locked --all-targets -- -D warnings
  - cargo llvm-cov --locked --fail-under-lines 80
  - cargo test --locked --test e2e -- --ignored
related:
  - design-supervisor-lifecycle
  - journal-003
---

# 015: End-to-end happy path with cmux and a stub agent

## Goal

`tests/e2e.rs` に、実バイナリ・実Git・実cmuxを使うハッピーパスのe2eテストを1本置く。Claudeの代わりに、promptを受け取って変更・commit・receipt書き込みを行うstubスクリプトを `--claude` に渡す。

- 使い捨てrepositoryとDBを作り、`init` → `add` → `ready` → `supervise --repo ... --claude <stub>` をバイナリで実行する。
- cmux workspaceが作られ、`session` wrapperがstubを起動し、receipt検証が通って `awaiting_integration` になり、workspaceが閉じられることを `show` のJSONとcmuxの一覧で確認する。
- branchをmainへfast-forwardし、`integrate` で `completed` になることまで含める。
- cmuxが必要なので `#[ignore]`。cmuxの `ping` が失敗したら明確なメッセージでskipではなくfailにする。
- 残ったworkspaceはテストが必ず閉じる。

完了条件: `cargo test --locked --test e2e -- --ignored` がcmux起動環境で通り、AGENTS.mdのe2e制約を満たす。cmux adapterの行カバレッジが上がる。

## Log

## Result

## Promoted
