---
id: design-overview
type: design
title: System overview
status: current
created: 2026-09-21
updated: 2026-09-23
last_verified: 2026-09-23
scope: system
related:
  - adr-0001
  - adr-0002
  - adr-0003
  - adr-0004
  - adr-0005
  - adr-0006
  - adr-0007
  - adr-0008
  - adr-0010
  - adr-0013
  - adr-0028
---

# System overview

ステップ7の時点でRust CLI、SQLiteキュー、依存が解けたtaskを上限付き並列で実行してreceiptを検証する常駐supervisor（[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)）、検証済みrunを最新mainへrebase・再検証して1 commitにsquashしmainへ着地させるmerge queue `integrate`（[ADR-0008](../adr/0008-merge-queue-squash-landing.md)）、`doctor`/`recover`、cmux adapter、Claude provider、Claude Code pluginを実装済み。以下の構成図のうち、Codex providerは後続実装。

コードは単一Cargo package内で、`domain`（型と状態遷移）、`application`（キュー・provider・workspaceの契約）、`infrastructure::sqlite`（キューの永続化）、`infrastructure::runtime_store`（run単位のlease・process・run状態の永続化）、`infrastructure::adapters`（Git、cmux、Claude Codeの呼び出し）、`infrastructure::location`（cwdからのqueueの解決、[ADR-0006](../adr/0006-queue-per-repository.md)）、`runtime`（supervisorとsession wrapper）、`main`（CLI）に分離している。この構成は[ADR-0013](../adr/0013-layered-architecture-and-type-function-style.md)でレイヤー構成を再編中で、domainのカプセル化とnewtype、applicationへのユースケースとportの集約、`runtime`・`lifecycle`・`main`の役割の整理を観点ごとのタスクで段階的に進める（外部公開APIとschemaは変えない）。上の構成は再編前の現状で、タスクが着地するたびにこの段落を更新する。利用方法は[README](../../README.md)を参照。

dagqは、依存関係を持つ開発タスクをSQLiteで管理し、着手可能なタスクをcmux workspaceとGit worktreeで実行するRust runtimeである。

## 用語集

役割は3つで、名前は[ADR-0010](../adr/0010-maintainer-and-resident-supervisor.md)で統一した。後続goalで`up`が開く予定のplannerとinboxを加えた5つが`DAGQ_ROLE`の値で、cmux workspaceのtitleは`[<repo>]<role>`（workerは`[<repo>]worker#<task-id> - <task title>`、[ADR-0028](../adr/0028-workspace-titles-are-repo-and-role.md)）。

| 用語 | 指すもの | 旧称 |
| --- | --- | --- |
| **supervisor** | runtimeの`dagq supervise`プロセス。依存が解けたtaskをclaimし、runごとにworktreeとcmux workspaceを作ってworkerを起動し、receiptを検証してworkspaceを閉じる（[ADR-0003](../adr/0003-supervisor-owns-lifecycle.md)、[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)）。ADR-0010以降はlaunchdのLaunchAgentとして常駐する予定（T2で実装）。 | （変更なし） |
| **maintainer** | 1 repositoryに1つ常駐する対話モードのClaude Code session。taskの登録、runの監視と権限確認・質問への応答、差分と証跡のレビュー、`integrate`の呼び出し、`needs_session`のrunへの指示、`doctor`/`recover`、`push_failed`のときの手動pushを行う（pushそのものは`integrate`が行う）。人が同じ操作をしてもよい。 | "SV"（supervisorの略。旧称）、operator（designとcodeでこの役割を指していたもの）、main session（[ADR-0003](../adr/0003-supervisor-owns-lifecycle.md)） |
| **worker** | runごとにsupervisorが起動するClaude（将来はCodex）のsession。割り当てられたworktreeの中だけで作業し、commitしてreceiptを書く。 | agent session、run session |
| **planner** | 人と対話してgoal / taskを登録するsession（[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)）。後続goalで`up`が`[<repo>]planner`に開く。いまは名前とrole値の定義だけ。 | （新設） |
| **inbox** | 人がmaintainerからのaskに答えるsession（[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)）。後続goalで`up`が`[<repo>]inbox`に開く。いまは名前とrole値の定義だけ。 | （新設） |

既存のADR（0001〜0009）とjournalは書き換えないので、そこに残る旧称はこの表で読み替える。runtimeのCLI名（`supervise`）と`supervisors`表は変えない。

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

テストは3層に分ける。`tests/queue.rs`・`tests/cli.rs`・`tests/location.rs`はSQLiteキュー、CLI、cwdからのqueue解決を、`tests/runtime.rs`はproviderとworkspaceをテストダブルに差し替えたsupervisor/wrapperを、cmuxなしで検証する（行カバレッジ80%の対象）。`tests/e2e.rs`は実バイナリ・実Git・実cmuxで、使い捨てrepositoryをcwdにして（`XDG_DATA_HOME`は一時dir）`init → add → ready → supervise --once → integrate`（squash着地、worktree削除）を1件と、2件同時（`--parallel 2`、依存taskは着地後の2回目のpassで新しいmainから、`integrate --next`のFIFO、同じファイルを書いた2件目の`needs_session`とテストがセッション役で解消してからの着地）で通し、Claudeの代わりに、promptからreceipt pathを読み取って変更・commit・receipt提出を行い、Stop hook相当のidle markerを書いてから端末の`/exit`を待つstubスクリプトを`--claude`に渡す。supervisorの`/exit`送信とworkspace closeも実cmuxで通る。cmuxが必要なので`#[ignore]`で、`cargo test --locked --test e2e -- --ignored`で実行する。cmuxが起動する`runner`は`LLVM_PROFILE_FILE`を継承せずworktreeに`.profraw`を書いてclean判定を落とすため、`cargo llvm-cov -- --include-ignored`では通らない。カバレッジは通常のテストだけで測る。

タスクは`draft | ready | in_progress | completed | canceled`を持つ。`ready`で依存先がすべて`completed`のタスクだけがschedulerの起動候補になる。詳細な実行状態はTaskRunに保存する。

supervisorはagentの完了レシート、コミット、テスト、worktreeのclean状態を確認してからworkspaceを削除する。worktreeとbranchは`integrate`がmainへ着地させるまで残し、着地後に`integrate`が削除する。
