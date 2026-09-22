---
id: design-overview
type: design
title: System overview
status: current
created: 2026-09-21
updated: 2026-09-22
last_verified: 2026-09-22
scope: system
related:
  - adr-0001
  - adr-0002
  - adr-0003
  - adr-0004
  - adr-0005
---

# System overview

ステップ4の途中時点でRust CLI、SQLiteキュー、1件を実行してreceiptを検証するsupervisor、mainへの統合を確認してtaskを完了する`integrate`、cmux adapter、Claude providerを実装済み。以下の構成図のうち、workspace終了、復旧コマンド、Codex provider、pluginは後続実装。

コードは単一Cargo package内で、`domain`（型と状態遷移）、`application`（キュー・provider・workspaceの契約）、`infrastructure::sqlite`（キューの永続化）、`infrastructure::runtime_store`（lease・process・run状態の永続化）、`infrastructure::adapters`（Git、cmux、Claude Codeの呼び出し）、`infrastructure::location`（cwdからのqueueの解決、[ADR-0006](../adr/0006-queue-per-repository.md)）、`runtime`（supervisorとsession wrapper）、`main`（CLI）に分離している。利用方法は[README](../../README.md)を参照。

cmux-taskqは、依存関係を持つ開発タスクをSQLiteで管理し、着手可能なタスクをcmux workspaceとGit worktreeで実行するRust runtimeである。

```text
CLI / Claude plugin / Codex plugin
                │
                ▼
        SQLite task queue
                │
                ▼
           supervisor
          ┌─────┴─────┐
          ▼           ▼
      cmux adapter  provider
          │       ┌───┴───┐
          ▼       ▼       ▼
       workspace Claude  Codex
          │
          ▼
       Git worktree
```

テストは3層に分ける。`tests/queue.rs`・`tests/cli.rs`・`tests/location.rs`はSQLiteキュー、CLI、cwdからのqueue解決を、`tests/runtime.rs`はproviderとworkspaceをテストダブルに差し替えたsupervisor/wrapperを、cmuxなしで検証する（行カバレッジ80%の対象）。`tests/e2e.rs`は実バイナリ・実Git・実cmuxで、使い捨てrepositoryをcwdにして（`XDG_DATA_HOME`は一時dir）`init → add → ready → supervise → integrate`を通し、Claudeの代わりに、promptからreceipt pathを読み取って変更・commit・receipt提出を行い、Stop hook相当のidle markerを書いてから端末の`/exit`を待つstubスクリプトを`--claude`に渡す。supervisorの`/exit`送信とworkspace closeも実cmuxで通る。cmuxが必要なので`#[ignore]`で、`cargo test --locked --test e2e -- --ignored`で実行する。cmuxが起動する`runner`は`LLVM_PROFILE_FILE`を継承せずworktreeに`.profraw`を書いてclean判定を落とすため、`cargo llvm-cov -- --include-ignored`では通らない。カバレッジは通常のテストだけで測る。

タスクは`draft | ready | in_progress | completed | canceled`を持つ。`ready`で依存先がすべて`completed`のタスクだけがschedulerの起動候補になる。詳細な実行状態はTaskRunに保存する。

supervisorはagentの完了レシート、コミット、テスト、worktreeのclean状態を確認してからworkspaceを削除する。`integrated`運用では、成果がmainへ取り込まれるまでworktreeを残す。
