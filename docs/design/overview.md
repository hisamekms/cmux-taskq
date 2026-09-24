---
id: design-overview
type: design
title: System overview
status: current
created: 2026-09-21
updated: 2026-09-24
last_verified: 2026-09-24
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
  - adr-0022
  - adr-0024
  - adr-0013
  - adr-0028
---

# System overview

ステップ7の時点でRust CLI、SQLiteキュー、依存が解けたtaskを上限付き並列で実行してreceiptを検証する常駐supervisor（[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)）、検証済みrunを最新mainへrebase・再検証して1 commitにsquashしmainへ着地させるmerge queue `integrate`（[ADR-0008](../adr/0008-merge-queue-squash-landing.md)）、`doctor`/`recover`、cmux adapter、Claude provider、Claude Code pluginを実装済み。以下の構成図のうち、Codex providerは後続実装。

コードは単一Cargo package内で、`domain`（型と状態遷移）、`application`（キュー・provider・workspaceの契約）、`infrastructure::sqlite`（キューの永続化）、`infrastructure::runtime_store`（run単位のlease・process・run状態の永続化）、`infrastructure::adapters`（Git、cmux、Claude Codeの呼び出し）、`infrastructure::location`（cwdからのqueueの解決、[ADR-0006](../adr/0006-queue-per-repository.md)）、`runtime`（supervisorとsession wrapper）、`main`（CLI）に分離している。この構成は[ADR-0013](../adr/0013-layered-architecture-and-type-function-style.md)でレイヤー構成を再編中で、domainのカプセル化とnewtype、applicationへのユースケースとportの集約、`runtime`・`lifecycle`・`main`の役割の整理を観点ごとのタスクで段階的に進める（外部公開APIとschemaは変えない）。上の構成は再編前の現状で、タスクが着地するたびにこの段落を更新する。着地済みの観点: domainの業務上の拒否は`DomainError`、task・goal・runのIDとcommitはnewtype（`domain::ids`の`TaskId`・`GoalId`・`RunId`・`CommitSha`。SQLiteとの変換は`infrastructure::sql_ids`、CLIの引数は`main`で変換。[domain-model](domain-model.md)）、TaskとGoalは非公開フィールドの集約（`domain::task` / `domain::goal`。新規作成は`new`、復元は`restore`、状態の変更はdomainのコマンド関数で、`infrastructure::sqlite`はその結果を保存する。[domain-model](domain-model.md#集約-taskとgoal)）。integrateとworker promptの生成はapplicationのユースケース（`application::integrate` / `application::prompt`）で、`application::ports`に置いたport（run storeの`RunStore`、Gitの`Repository`、検証コマンドの`Verifier`と、既存の`TaskStore`・`MainRemote`・`ProcessControl`・`Clock`・`IdGenerator`など）越しに動き、`SqliteQueue`・`GitRepository`・`ShellVerifier`がportを実装する。`runtime::integrate`はportの実装を組み立ててleaseのheartbeatを保つ入口だけになった（[supervisor-lifecycle](supervisor-lifecycle.md#integrate)）。利用方法は[README](../../README.md)を参照。

dagqは、依存関係を持つ開発タスクをSQLiteで管理し、着手可能なタスクをcmux workspaceとGit worktreeで実行するRust runtimeである。

## 用語集

役割は5つで、[ADR-0024](../adr/README.md)の決定1で確定した: supervisor、worker、planner、inbox、observer。このうち`DAGQ_ROLE`で名乗るworkspaceを持つのはsupervisor（in-cmux modeのとき）、worker、planner、inboxで、cmux workspaceのtitleは`[<repo>]<role>`（workerは`[<repo>]worker#<task-id> - <task title>`、[ADR-0028](../adr/0028-workspace-titles-are-repo-and-role.md)）。`up`が開くのはsupervisor（in-cmux mode）、inbox、plannerの3つだけ。observerとreview / triageのjobはworkspaceを持たないheadlessの`claude -p`で、`DAGQ_ROLE`はobserverが`observer`、review / triageのjobが`reviewer`。

| 用語 | 指すもの | 旧称 |
| --- | --- | --- |
| **supervisor** | runtimeの`dagq supervise`プロセス。依存が解けたtaskをclaimし、runごとにworktreeとcmux workspaceを作ってworkerを起動し、receiptを検証し、runごとのheadlessのjob（review、triage）を起動してそのverdictで着地・差し戻し・resume・retryを行い、`needs_session`のrunをresumeし（3回で解消しなければ人へのaskにする）、後始末をする（[ADR-0003](../adr/0003-supervisor-owns-lifecycle.md)、[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)、[ADR-0023](../adr/0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)、[ADR-0027](../adr/0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)、ADR-0024）。launchdのLaunchAgentとして、またはin-cmux modeで常駐する。 | "SV"（supervisorの略） |
| **worker** | runごとにsupervisorが起動するClaude（将来はCodex）のsession。割り当てられたworktreeの中だけで作業し、commitしてreceiptを書く。判断が要るときは`worker_question`のaskを登録して止まる。 | agent session、run session |
| **planner** | 人と対話してgoal / taskを登録し、follow_upのdraft taskとobserverのdraft goalの採否を人と決め、goalをcloseする常駐session（[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)）。`up`が`[<repo>]planner`のworkspaceに`planner_prompt`付きで開く。人の指示で`up` / `down`も打つ。 | （新設） |
| **inbox** | 人に届くものすべての窓口になる常駐session（[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)、ADR-0024の決定6）。openなaskを人に見せてanswerを書き戻し、それ以外のattention（回答済みのask、止まったsupervisor、失敗したreview / triage、pushの失敗）を人に知らせ、人の指示があるときだけ`dagq-recover`の手順（`up` / `down`、手でのreviewと`integrate`、`recover`、run workspaceへのキー送信）を実行する。自分では判断しない。`up`が`[<repo>]inbox`のworkspaceに`inbox_prompt`付きで開く。 | （新設） |
| **observer** | supervisorのtimerで定期起動するheadlessのjob（ADR-0024の決定4）。`stats`と直近のnoteを読み、note・`blocked`のask・draftのgoalだけを書く。状態は変えない。 | （新設） |

**退役した役割。** ADR-0010からADR-0023までの記述とjournalに出てくる常駐のClaude Code session「メンテナー」（英字表記の役割名。`up`が`[<repo>]`＋その名のworkspaceを開き、`DAGQ_ROLE`にその名を持っていた）は、ADR-0024で退役した。既存のADRとjournalは書き換えないので、そこでのメンテナーの仕事は次のとおり読み替える: レビューと着地はsupervisorのreview job、失敗runの扱い（recoverしてready / cancel）はsupervisorのtriage job、3回resumeして解消しない`needs_session`と作業中のdialog（`prompt_waiting`）はinbox宛てのask、継続的な監視と改善提案はobserver、人への相談とanswerに従う操作・`up` / `down` / 固定バイナリの更新はinbox（またはplanner）のsessionから人の指示で行う。`up`はそのworkspaceを開かず、queueに記録されたそのworkspaceの行を忘れる（workspace自体は人が閉じる）。

既存のADR（0001〜0009）とjournalに残る旧称もこの表で読み替える。runtimeのCLI名（`supervise`）と`supervisors`表は変えない。

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
