---
id: adr-0013
type: adr
title: domain / application / infrastructureのレイヤーと「型＋関数」でruntimeを構成する
status: accepted
created: 2026-09-22
updated: 2026-09-22
accepted_on: 2026-09-22
owners:
  - hisamekms
tags:
  - architecture
  - domain
  - application
  - infrastructure
related:
  - adr-0001
  - adr-0009
  - design-overview
  - design-domain-model
  - design-persistence
  - design-supervisor-lifecycle
---

# ADR-0013: domain / application / infrastructureのレイヤーと「型＋関数」でruntimeを構成する

## Context

MVP（[plans/current.md](../plans/current.md)のステップ1〜9）はレイヤーの分離より動く経路を優先して積み上げた。その結果、2026-09-22時点のコードは次の状態にある。

- `src/domain.rs`はTask・Goal・TaskRunを**公開フィールドのstruct**として持つ。エラーは`anyhow`、IDは`i64`と`String`の生の値で、`serde`の`Deserialize`により制約検証を迂回して構築できる。
- 状態遷移の一部（runのstatusの更新条件、claimの`in_progress`化、integrateの`completed`化）は`src/infrastructure/sqlite.rs`と`src/infrastructure/runtime_store.rs`のSQLに埋まっている。
- ユースケース（supervise、integrate、status/doctor/recover、session、prompt生成、up/down）は`src/runtime.rs`（約2000行）と`src/lifecycle.rs`にあり、`SqliteQueue`・`GitRepository`・ファイルシステムを直接使う。`runtime_store.rs`のrun操作はportではなく`SqliteQueue`の固有メソッドである。
- 現在時刻（`runtime.rs`の`unix_time()`）とID（`runtime.rs`と`sqlite.rs`の`Uuid::new_v4`）は使う場所で生成している。
- `src/application.rs`にはTaskQueue・AgentProvider・WorkspaceBackend・LaunchAgent・ProcessControlのportがある。

この構成のままだと、業務上の拒否（許可されない状態遷移、不正な値）と外部I/Oの失敗が`anyhow`で一様に混ざり、状態遷移の判断がSQLとRustに二重化し、時刻とIDが固定できないためテストが実プロセス・実時刻に寄る。一方でMVPの外部公開API（CLIのJSON出力、エラーメッセージ、exit code、SQLite schema、`plugins/`）はドッグフーディングで使われていて、壊せない。

## Decision

既存の業務仕様と外部公開APIの振る舞いを維持したまま、過剰な抽象化を避けて段階的に、次の方針へ寄せる。

### 1. レイヤー構成

- **domain**: ドメインモデル、制約、業務判断、状態遷移。
- **application**: ユースケース、アプリケーションサービス、外部操作に必要なport。
- **infrastructure**: DBや外部APIなどのアダプター。portを実装する。
- **起動部分**: 具体的なアダプターを組み立て、依存を注入する。

applicationはdomainに依存する。domainはapplication・infrastructureに依存せず、外部I/Oを行わない。applicationが必要とするportは、原則としてapplicationにtraitで定義する。レイヤーを分けるためだけのcrate分割は必須としない。

### 2. 「型＋関数」を基本とする

集約は「状態を表す型」と「コマンド・クエリ関数」で表す。VOは意味と制約を表す型、ドメインプリミティブは既存の型を包むnewtypeとする。生成や値の取り出しに`impl`を使うことは許容し、通常の関数と関連関数・メソッドの選択を機械的に統一することは目的にしない。

### 3. 集約とカプセル化

集約のフィールドは原則非公開とし、型と操作関数は非公開フィールドにアクセスできる同じモジュールに置く。

- コマンド: `fn command(aggregate: Aggregate, ...) -> Result<Aggregate, DomainError>`
- クエリ: `fn query(aggregate: &Aggregate, ...) -> ...`

コマンドは所有権を受け取り、成功時に更新後の集約を返す。関数内部の`mut`は許容する。失敗時は通常ユースケースを終了するので、元の集約を返す仕組みは標準では導入しない。所有権の回避だけを目的とした`Clone`は足さない。

### 4. VO・ドメインプリミティブ

意味の異なるIDや量はnewtypeで区別する。型エイリアスはこの目的に使わない。内部フィールドは非公開とし、必要な制約は生成関数や`TryFrom`で検証する。生成された値は制約を満たす前提で扱う。共通基底やフレームワークは導入せず、比較・複製などのtraitは意味と用途に応じて実装する。

### 5. 新規作成と復元

新規作成（初期状態と作成時のルール）とDBからの復元（保存済み状態の再現と、復元対象に必要な不変条件の検証）で入口を分ける。DB用の型からドメイン型への変換はinfrastructureで行い、domainが復元用の入口を提供する。復元時に作成時のイベントや処理を再実行しない。デシリアライズや公開フィールドで制約検証を迂回させない。

### 6. エラー

業務上の拒否と外部I/Oの失敗を区別する。domainは不正な値・許可されない状態遷移・業務条件の不成立をenum（`DomainError`）で表し、applicationは対象なし・更新競合・外部操作の失敗をユースケースの境界で扱い、infrastructureはDB・外部API固有の失敗をportのエラーへ変換して原因情報を保持する。DBライブラリ固有の型をdomainに持ち込まない。エラーには集約全体ではなく説明に必要な情報だけを持たせる。

### 7. 時刻・ID生成

applicationには「生成する仕組み」を注入し、domainには「生成済みの値」を渡す。単純な用途では関数・クロージャの注入、複数ユースケースで共有するなら`Clock`・`IdGenerator`のtraitを検討する。1つの業務操作で基準時刻を揃える必要があるときは一度取得して使い回す。テストでは固定時刻・固定IDを注入できるようにする。

### 8. DDDのトリレンマ

ドメインの純粋性を基本とし、第一選択は「applicationで取得 → domainで判断・状態遷移 → applicationで保存」とする。第一選択で足りない箇所は、取得コストや競合の理由をreceiptのsummaryに残したうえでapplicationで判断を補い、domainにI/Oを持ち込まない。

### この repository での適用

**モジュール境界**: crateは1つのまま、`src/`のモジュールで境界を引く。

- `src/domain/`を`task`・`goal`・`run`・`error`・`ids`・`views`に分ける。`task` / `goal` / `run`が集約（Task、Goal、TaskRun）と、そのコマンド・クエリ関数を持つ。`error`が`DomainError`、`ids`が`TaskId`・`GoalId`・`RunId`・`CommitSha`などのnewtype、`views`が読み取り専用のview型（TaskDetail、GoalSummary、GoalDetail、GoalTask、Predecessor、RunEvent、RunProcess、RunLease、SupervisorRegistration、TaskStatusCounts）を持つ。view型は集約ではないので公開フィールドのままでよく、集約のモジュールとは分ける。集約の入口に渡す入力型（`NewTask`、`NewGoal`、`GoalEdit`）は対応する集約のモジュールに置く。`Receipt`と`ReceiptCheck`はagentが書くファイルの形なので`domain::receipt`に分け、`ClaimOutcome`と`IntegrationOutcome`はユースケースの結果なのでapplicationへ移す。
- `src/application/`にユースケース（supervise、integrate、prompt生成、session、status/doctor/recover、up/down）と、そのために必要なportのtraitを置く。既存のTaskQueue・AgentProvider・WorkspaceBackend・LaunchAgent・ProcessControlに、run store（run単位の永続化）、Git、clock、ID生成のportを足す。
- `src/infrastructure/`は変わらずportの実装（`sqlite`、`runtime_store`、`adapters`、`launchd`、`location`）を持ち、行からドメイン型への変換（復元）をここで行う。
- `src/runtime.rs`と`src/lifecycle.rs`はユースケースがapplicationへ移るぶん薄くなる。最終的に残るのは、スレッド・シグナル・ログといったプロセス実行の都合と、applicationのユースケースを駆動する入口だけにする。
- `src/main.rs`はCLIの引数解析、具体アダプターの組み立てと依存注入、結果のJSON出力とexit codeだけを行う。

**状態遷移**: どのstatusからどのstatusへ移れるかの判断はdomainに置く。SQLの`WHERE status IN (...)`は楽観的な競合検出として残してよいが、判断の唯一の場所にはしない。

**外部公開APIの固定**: SQLiteのschemaとmigrationは変えない。移行済みqueueが報告する`user_version`は`SqliteQueue::SCHEMA_VERSION`（`migrations/`の本数）で、2026-09-22時点では8（`0001_queue.sql`〜`0008_goals.sql`）。goalの制約文にある「schema v7」は`0008_goals.sql`を数えていないので、実際の固定先はこの8である。CLIのJSON出力のフィールド名・値、エラーメッセージ、exit code、`plugins/`配下も変えない。`tests/cli.rs`・`tests/plugin.rs`・`tests/e2e.rs`が外部公開APIの回帰テストで、これらは変更なしで通ることを条件にする。

**進め方**: 1タスクは1つの観点（エラー、newtype、カプセル化、時刻/ID、ユースケース移動）に絞り、他の観点の変更を混ぜない。各タスクは前のタスクが着地したmainの上で作業する。実装を変えたタスクは`docs/design/`の該当文書と`updated` / `last_verified`を同じタスクで更新する。

## Alternatives

- **crateを`domain` / `application` / `infrastructure`に分割する**: 依存方向をコンパイラに強制させられるが、workspace化・Cargo.tomlの分割・テストの置き場の変更が一度に必要で、段階的な着地と相性が悪い。1 crate内のモジュール境界で始め、依存方向の違反はレビューと棚卸しの受け入れ条件（`src/domain`配下に`rusqlite`・`std::fs`・`std::process`・`SystemTime::now`・`Uuid::new_v4`・`anyhow`への参照がないこと）で見る。将来crateを分けたくなったとき、モジュール境界がそのまま分割線になる。
- **現状維持（`anyhow`＋公開フィールド）**: 変更コストはゼロだが、業務上の拒否と外部I/Oの失敗が区別できず、状態遷移がSQLとRustに二重化したままになる。ドッグフーディングでtaskの状態をSQLから読み解く手間が増え続けている。
- **状態遷移をSQLに寄せきる**: `WHERE status IN (...)`を唯一の判断にすれば実装は1か所で済むが、判断がRustの型から見えず、単体テストがDB込みになり、schema凍結の制約下では遷移の変更ができない。
- **一度に全面書き換える**: レイヤーと型を同時に入れ替えると、CLIのJSON・exit code・schemaの回帰を1つの巨大な差分でしか確認できない。1タスク1観点にすれば、`tests/cli.rs` / `tests/plugin.rs` / `tests/e2e.rs`がどの観点で壊れたかが分かる。
- **ORM・DDDフレームワークの導入**: rusqliteの直接利用をやめれば変換コードは減るが、schema凍結、`BEGIN IMMEDIATE`によるclaimとleaseの競合制御、`WHERE status IN (...)`の楽観的検出を維持できる保証がない。

## Consequences

- 業務上の拒否が`DomainError`のvariantとして型に現れ、対応の網羅をコンパイラが見る。外部I/Oの失敗はportのエラーとして分かれる。
- 状態遷移の判断がdomainの関数に集まり、DBなしで単体テストできる。SQLの`WHERE status IN (...)`は競合検出として残るので、schemaもトランザクションの安全性も変わらない。
- 時刻とIDがapplicationへ注入されるので、テストで固定でき、1つの業務操作の中で基準時刻を揃えられる。
- `runtime.rs`（約2000行）と`lifecycle.rs`が薄くなり、ユースケースの単体テストがテストダブルのportだけで書ける。
- コストとして、newtypeと復元の入口のぶん変換コードが増える。公開フィールドを前提にしていた呼び出し側（`runtime.rs`、`lifecycle.rs`、`main.rs`、`infrastructure/`、テスト）は観点ごとに追随が要る。
- 段階的に進めるのは、着地単位を小さく保って回帰の原因を特定できるようにするため。1タスク1観点にすると、あるタスクの途中では「newtypeは入ったがカプセル化はまだ」のような中間状態がmainに乗る。これは許容し、ゴールの受け入れ条件は全タスク着地後のmainに対して判定する。
- 外部公開API（CLIのJSON出力、エラーメッセージ、exit code、SQLite schema、`plugins/`）は変えない。したがってリファクタリングの途中でも固定バイナリの入れ替えやpluginの更新は不要で、`tests/cli.rs`・`tests/plugin.rs`・`tests/e2e.rs`が変更なしで通ることが各タスクの回帰条件になる。schemaを現状のまま保つので、`created_at` / `updated_at`のような**SQLiteが`strftime` / `unixepoch`で生成するタイムスタンプ**はDB側に残り、注入した時刻で置き換えはしない。
- `docs/design/overview.md`と`domain-model.md`は、タスクの進行に合わせて新しいレイヤー構成を説明するよう更新する。

## 棚卸し

2026-09-22時点の`src/`を読んで見つけた、方針との差分。各項目に、解消を担当する後続タスクを添える。ADRは追記のみなので、ここに書いた行番号は後続タスクがコードを動かしても更新しない。**この節は着手時点のスナップショット**で、現在のコードの説明は`docs/design/`が持つ。

後続タスクは観点ごとに分かれていて、このgoalのキュー上のIDは次のとおり（この文書ではIDではなく観点の名前で指す）。DomainError（33）、newtype（34）、Task・Goalのカプセル化（35）、TaskRunのカプセル化（36）、時刻/ID（37）、integrate/prompt（38）、supervise/session（39）、status/doctor/recover・up/down（40）。

### 公開フィールドの集約（→ Task・Goalのカプセル化タスク、TaskRunのカプセル化タスク）

- `src/domain.rs:157` `Task`、`:175` `Goal`、`:307` `TaskRun`がすべて`pub`フィールドで、`Serialize` / `Deserialize`も導出している。`serde_json::from_str`で任意の値から直接組み立てられるので、生成時の検証（`NewTask::validate`、`NewGoal::validate`）を迂回できる。→ Task・Goal（`Task`、`Goal`）、TaskRun（`TaskRun`）。
- 新規作成の検証が集約の外にある。`src/domain.rs:133` `NewTask::validate`と`:204` `NewGoal::validate`は呼び出し側（`src/infrastructure/sqlite.rs:138`以降の`TaskQueue for SqliteQueue`の`add` / `add_goal`）が呼ぶ約束で、型では強制されない。→ Task・Goalのカプセル化タスク。
- `src/domain.rs:234` `GoalEdit::apply`は`goal.clone()`してから公開フィールドを書き換える。コマンドの基本形（所有権を受け取り更新後の集約を返す）になっていない。→ Task・Goalのカプセル化タスク。
- `src/domain.rs:189` `Goal::is_closed`は`closed_at.is_some()`を見るだけだが、`closed_at`と`verdict`が別々の公開フィールドなので「閉じているのにverdictがない」状態を型で禁止できていない。→ Task・Goalのカプセル化タスク。
- `src/domain.rs:331` `TaskRun::idle_marker_path`は`run_dir`が`Option<String>`であることを実行時に`context("missing run directory")`で扱う。runの局面（`claimed`と`running`で何が埋まっているか）が型に出ていない。→ TaskRunのカプセル化タスク。
- `src/domain.rs:439` `Receipt`と`:456` `ReceiptCheck`も公開フィールドの`Deserialize`型で、不変条件は`:468` `Receipt::check`が別に検査する。集約ではないが、検証を迂回して構築できる点は同じなので、パース（`:463` `Receipt::parse`）と検査を1つの入口にまとめる。`:120` `NewTask`・`:195` `NewGoal`・`:216` `GoalEdit`（入力型）と`:408` `ClaimOutcome`・`:419` `IntegrationOutcome`（ユースケースの結果）の置き場は上の「この repository での適用」に書いた。→ Task・Goalのカプセル化タスク（入力型）、TaskRunのカプセル化タスク（`Receipt`）。
- 一方、`TaskStatusCounts`・`GoalSummary`・`GoalTask`・`GoalDetail`・`Predecessor`・`RunEvent`・`TaskDetail`・`RunProcess`・`RunLease`・`SupervisorRegistration`（`src/domain.rs`の`:258`、`:282`、`:292`、`:299`、`:344`、`:350`、`:362`、`:371`、`:386`、`:398`）は読み取り専用のviewなので公開フィールドのままでよい。集約とは別モジュール（`domain::views`）へ移す。→ Task・Goalのカプセル化タスク（移設のみ）。

### 生のIDと値（→ newtypeタスク）

- IDが`i64`と`String`のまま: `src/domain.rs:158` `Task::id`、`:164` `Task::goal_id`、`:125` `NewTask::dependencies`（`Vec<i64>`）、`:309` `TaskRun::task_id`、`:308` `TaskRun::id`（`String`）、`:353` / `:354` `RunEvent::task_id` / `goal_id`、`:372` `RunProcess::run_id`、`:387` `RunLease::run_id`。task IDとgoal IDはどちらも`i64`で、取り違えを型が止められない。→ `TaskId`・`GoalId`・`RunId`のnewtype。
- commit SHAが`String`のまま: `src/domain.rs:313` `TaskRun::base_commit`、`:319` `TaskRun::result_commit`、`:442` `Receipt::commit`。形式の検証は`src/domain.rs:505` `validate_base_commit`と`:509` `validate_commit`が値とは別に行うので、検証済みであることが型に残らない。→ `CommitSha`のnewtype（生成関数か`TryFrom`で検証し、生成後は検証済みとして扱う）。
- portと固有メソッドのシグネチャも生の型: `src/application.rs:13` `TaskQueue::show(task_id: i64)`、`:20` `TaskQueue::claim(base_commit: &str)`、`src/infrastructure/runtime_store.rs:70` `claim_for_supervisor(base_commit, token)`、`:430` `plan_run(id: &str, token: &str, ...)`。run IDとsupervisor tokenがどちらも`&str`で並ぶので、引数の順番違いをコンパイラが検出できない。→ newtypeタスクでportのシグネチャも合わせる。

### anyhowを返すdomain関数（→ DomainErrorタスク）

`src/domain.rs`は`anyhow::{Context, Result, bail, ensure}`を使い、業務上の拒否を文字列で表している。

- `src/domain.rs:83` `TaskStatus::transition`（`bail!("cannot apply {action:?} to task in {} state")`、`bail!("task has an unfinished run; ...")`）。許可されない状態遷移そのもので、最初にenumへ移す対象。
- `src/domain.rs:4`の`string_enum!`が生成する`FromStr`（`type Err = anyhow::Error`、`bail!("unknown {}: {value}")`）。`TaskStatus`・`RunStatus`・`Provider`・`GoalVerdict`・`ReceiptResult`・`CheckStatus`の6つ。`src/infrastructure/sqlite.rs:643` `enum_col`が`FromStr<Err = anyhow::Error>`を境界に使っているので、ここを変えるとinfrastructure側の変換も一緒に動く。
- `src/domain.rs:133` `NewTask::validate`、`:204` `NewGoal::validate`、`:234` `GoalEdit::apply`（`ensure!`による不正な値の拒否）。
- `src/domain.rs:463` `Receipt::parse`（`context`）、`:468` `Receipt::check`、`:505` `validate_base_commit`、`:509` `validate_commit`。
- `src/domain.rs:331` `TaskRun::idle_marker_path`（`context("missing run directory")`）。
- エラーメッセージはCLIの外部公開APIなので、`DomainError`の`Display`は現在の文字列をそのまま再現する。`tests/cli.rs`がその回帰テスト。

### SQLに埋まった状態遷移の条件（→ TaskRunのカプセル化タスク、integrate/promptタスク、supervise/sessionタスク、status/doctor/recoverタスク）

`UPDATE ... WHERE status IN (...)`が、競合検出だけでなく「どの状態から動けるか」の唯一の判断になっている箇所。

- `src/infrastructure/sqlite.rs:31` `READY_QUERY`（`:33`の`t.status = 'ready'`と`r.status IN ('claimed','starting',...,'needs_session')`）と`:568` `has_unfinished_run`が、claim可能かと「未完了run 1件」の制約をSQLだけで決めている。→ TaskRunのカプセル化タスク。
- `src/infrastructure/sqlite.rs:540` `claim_task`が`:548`で`UPDATE tasks SET status='in_progress'`、`:550`でrunの`'claimed'`挿入を直接書く。taskの`in_progress`化がdomainの`TaskStatus::transition`を通らない。→ TaskRunのカプセル化タスク。
- `src/infrastructure/runtime_store.rs:430` `plan_run`（`:437`の`SET status='starting'`）、`:458` `workspace_created`（`:466`の`AND status='starting' AND workspace_id IS NULL`）、`:506` `register_wrapper`（`:513`の`AND status='starting' AND workspace_id IS NOT NULL`）、`:528` `register_agent`（`:539`の`SET status='running' WHERE ... status='starting'`）。runの起動手順の順序（planned → workspace作成 → wrapper登録 → agent登録）がSQLの述語だけで表現されている。→ supervise/sessionタスク。
- `src/infrastructure/runtime_store.rs:661` `finish_supervision`が終了コードから`validating` / `failed`をRustで決め、`AND status IN ('starting','running')`で適用範囲を絞る。`:687` `finish_validation`も`validation.accepted`から次のstatusを決める。判断そのものはdomainへ、`status IN`は競合検出として残す。→ TaskRunのカプセル化タスク（遷移）、supervise/sessionタスク（呼び出し側）。
- `src/infrastructure/runtime_store.rs:728` `next_awaiting_integration`（`:732`の`r.status='awaiting_integration'`によるFIFOの選択）、`:747` `begin_integration`が`:753`の`SELECT id FROM task_runs WHERE status='integrating'`で統合スロットの排他を見て、`:773`で`SET status='integrating' ... AND status IN ('awaiting_integration','needs_session')`と書く。`:798` `defer_integration` / `:818` `fail_integration` / `:837` `abort_integration`（いずれも`:854`の私的ヘルパー`leave_integration`に委譲し、`:869`が`AND status='integrating'`で絞る）、`:904` `finish_integration`（`SET status='integrated'`）と`:917`（`UPDATE tasks SET status='completed'`）。taskの`completed`化がdomainを通らない。→ integrate/promptタスク。
- `src/infrastructure/runtime_store.rs:194` `runs_leased_by_others`（`:198`の`r.status IN ('running','validating')`）と`:243` `adopt_run`（`:259`の同じ条件。ADR-0012の引き継ぎ条件(a)）、`:578` `active_runs`（`status IN ('claimed','starting','running','validating','integrating')`）、`:627` `recover_run`（同じ集合＋遷移先を引数で受ける）。→ status/doctor/recoverタスク。
- `src/infrastructure/runtime_store.rs:963` `workspace_closed`（`:970`〜`:971`）と`:989` `cleanup_failed`（`:996`〜`:997`）は`supervisor_token`に加えて`status='awaiting_integration'`と`workspace_id` / `workspace_closed_at`のNULL条件で適用範囲を絞る。「workspaceを閉じてよいのはどのrunか」もdomainの判断で、ADR-0012で`supervisor_token`が「いまこのrunを動かしているsupervisor」になったことと合わせてTaskRunの不変条件としてdomainに書く。→ TaskRunのカプセル化タスク。
- `src/infrastructure/sqlite.rs:203`の`TaskQueue::transition`は`:211`で`TaskStatus::transition`の結果をSQLに書くだけで、既に方針どおり。他の箇所をこの形に寄せる。

### infrastructureのRustに書かれた業務判断（→ Task・Goalのカプセル化タスク）

statusの遷移ではないが、業務上の拒否をinfrastructureが`anyhow`の`ensure!`で決めている箇所。domainへ移す対象。

- `src/infrastructure/sqlite.rs:415` `close_goal`が`:420`で「すでに閉じたgoalは閉じられない」を拒否し、`:425`〜`:443`でtaskの件数を`GoalVerdict::allows`に照らして「achievedは全taskがterminal、abandonedはin_progressがないこと」を判定する。判断の一部（`allows`）はdomainにあるが、入口の判断はinfrastructureにある。→ Task・Goalのカプセル化タスク。
- `src/infrastructure/sqlite.rs:460` `set_goal`が`:464`以降で`task.status.dependencies_editable()`と`:498` `ensure_goal_open`を使い、「draftかreadyのtaskだけがgoalを移せる」「移し先のgoalは開いている」を決める。→ Task・Goalのカプセル化タスク。
- `src/infrastructure/sqlite.rs:584` `insert_dependency`が自己依存の拒否と、再帰CTEによる循環検出（`:601`）を持つ。`:234` `remove_dependency`は`:238`で`dependencies_editable`を見る（`:225` `add_dependency`は同じ判定を`insert_dependency`の中で行う）。依存グラフの不変条件がSQLとinfrastructureに散っている。→ Task・Goalのカプセル化タスク。
- これらは「applicationで取得 → domainで判断 → applicationで保存」の第一選択に載せる。循環検出だけは全依存グラフの取得が要るので、取得コストを理由にSQLの再帰CTEを残してよい（DDDのトリレンマの例外）。判断を残す場合はその理由をタスクのreceiptのsummaryに書く。

### runtime.rsが直接使うinfrastructure型（→ integrate/promptタスク、supervise/sessionタスク、status/doctor/recoverタスク）

- `src/runtime.rs:7`〜`:23`の`use`が`infrastructure`を名指しする: `adapters::{ClaudeCode, GitRepository, path_text, process_alive, run_shell_to_log, shell_join}`、`location::runs_dir`、`runtime_store::{HEARTBEAT_TIMEOUT_SECS, Landing, LeasedRun, RunPlan, Validation, lease_is_stale}`、`sqlite::SqliteQueue`。
- 具体型を引数に取る関数: `src/runtime.rs:179` `supervise`（`db: &Path`から`SqliteQueue::open`、`GitRepository`、`ClaudeCode`を自前で組み立てる）、`:257` `Supervisor`と`:297`の`impl`、`:1045` `integrate`、`:1178` `land`（`&mut SqliteQueue`、`&GitRepository`）、`:1382` `remove_landed_worktree`、`:1668` `status`、`:1699` `doctor`、`:1728` `recover`、`:1882` `session` / `:1893` `session_with_provider`、`:1932` `drive_agent`。
- runの永続化は`SqliteQueue`の固有メソッド（`runtime_store.rs`の`claim_for_supervisor`、`plan_run`、`register_wrapper`、`finish_supervision`、`finish_validation`、`begin_integration`、`finish_integration`ほか）で、applicationのportになっていない。Git操作（`adapters::GitRepository`）とファイルシステム（`runtime.rs`が直接使う`std::fs`）も同様。→ run store portとGit portをapplicationに定義し、integrate（`integrate` / `land` / `commit_message` / `remove_landed_worktree`）とprompt生成（`:1486` `prompt`、`:1417` `PredecessorSummary`、`:1472` `siblings_in_progress`、`:1581` `maintainer_prompt`）を先に移す（integrate/promptタスク）。supervise（`supervise` / `Supervisor` / `Slot` / `SessionWatch` / `check_receipt` / `spawn_validation` / `close_workspace`）とsession wrapper（`session` / `session_with_provider` / `drive_agent`）が次（supervise/sessionタスク）。`status` / `doctor` / `recover`と`src/lifecycle.rs:92` `up` / `:338` `down` / `:291` `open_work`が最後（status/doctor/recover・up/downタスク）。
- `src/lifecycle.rs:14`〜`:27`も同じく`SqliteQueue`・`GitRepository`・`ClaudeCode`・`LaunchAgentSpec`・`QueueLocation`を直接使う。`src/main.rs:17`〜`:20`は`SqliteQueue`と`QueueLocation`を直接使い、CLIの各サブコマンドがqueueを組み立てている。main.rsは最終的に組み立てと依存注入だけにする。→ status/doctor/recover・up/downタスク。
- `src/application.rs:41` `AgentProvider::command`は`std::process::Command`を返す。applicationにOSのプロセス型が出ているので、ユースケース移動のときにportの形を見直す。→ supervise/sessionタスク。

### now()とUuid::new_v4の呼び出し箇所（→ 時刻/IDタスク）

- `src/runtime.rs:41` `unix_time()`が`SystemTime::now()`を呼ぶ唯一の定義。呼び出しは`src/runtime.rs:106`（`SupervisorLog::note`）、`:534`（leaseのstale判定）、`:782`（wrapperのheartbeat確認）、`:1670`（`status`）、`:1701`（`doctor`）、`:1743`（`recover`）、`src/lifecycle.rs:206`（`fresh`）、`:292`（`open_work`）。いずれもユースケースの中で都度取得していて、1回の操作で基準時刻が揃っていない。→ clock portを注入し、`status` / `doctor` / `recover`は1回取得した`now`を使い回す。
- `src/runtime.rs:837` `unix_seconds(SystemTime)`はファイルのmtimeをUNIX秒に直すヘルパーで、`:814` `idle_after_receipt`がreceiptとidle markerの新旧比較に使う。時刻の取得ではなくファイル属性の変換なので、file system portの戻り値として扱う。
- `Uuid::new_v4()`は3か所: `src/runtime.rs:203`（supervisorのtoken）、`src/runtime.rs:1096`（`integrate`のtoken）、`src/infrastructure/sqlite.rs:547`（`claim_task`のrun ID）。→ ID生成をapplicationのportにし、domainには生成済みの`RunId`を渡す。テストで固定IDを注入できるようにする。
- `Instant::now()`（`src/runtime.rs:627`、`:702`、`:767`、`:1900`、`src/lifecycle.rs:214`、`:223`、`src/infrastructure/adapters.rs:85`、`:89`、`:201`、`:206`、`src/infrastructure/launchd.rs:158`）はタイムアウトの計測であって業務上の時刻ではない。clock portの対象にはせず、そのまま残す。
- SQLiteが生成するタイムスタンプ（`sqlite.rs`と`runtime_store.rs`の`strftime('%Y-%m-%dT%H:%M:%fZ','now')`、`unixepoch()`、およびmigrationsの`DEFAULT`）はschemaの一部なので変えない。注入した時刻はこれらを置き換えず、application側で必要な判断にだけ使う。

## 追補（2026-09-22）

この節は2026-09-22の追補である。この文書を書いた時点では、ユーザーの設計方針のうち**8（DDDのトリレンマ）が途中で切れた状態**でしか読めず、9（同時更新・トランザクション）と10（実施方針と検証）は存在しなかった。その後goal 3のconstraintsに8の全文と9・10が追加されたので、ここに書き写す。ADRは追記のみなので上の`## Decision`の8は書き換えず、**この節が8の最新版**とする。あわせて`## Consequences`の記述を1点訂正する。

### 8. DDDのトリレンマ（全文）

ドメインの純粋性を基本とし、次の順で対応する。

第一選択は「applicationで取得 → domainで判断・状態遷移 → applicationで保存」とする。情報量や取得コストが小さい場合は、多少の不要な取得を許容して構造を単純に保つ。

条件付きの取得が必要で、先読みのコストが問題になる箇所では、**domainの処理を段階に分ける**。

1. domainがenumなどで「完了」または「追加情報が必要」を返す。
2. applicationが要求された情報を取得する。
3. domainに取得結果を渡し、処理を続ける。

「追加情報が必要か」という業務判断はdomainに置き、applicationはその結果に従ってI/Oを実行する。必要に応じて**非公開フィールドを持つ中間型**で途中の状態を表す。

この方式は複雑さを増やすため、全ユースケースへ一律に導入しない。段階分けが過度に複雑になる場合に**限り**、applicationへの限定的な業務分岐配置や、I/Oを伴うドメインサービスを検討し、理由を明示する（この repository ではreceiptのsummaryに書く）。

**trait経由でも、domainが外部I/Oを実行する場合は純粋ではない**ことに留意する。domainにport traitの引数を渡して「抽象化したから純粋」とはみなさない。

この repository で既に例外として認めている依存グラフの循環検出（SQLの再帰CTE）は、上の「限定的な例外」に当たる。理由（全依存グラフの取得コスト）を担当タスクのreceiptに書く。

### 9. 同時更新・トランザクション

Rustの所有権と、DB上の同時更新制御は別に扱う。

- 更新競合が発生する箇所では、バージョン照合などの**楽観ロック**を検討する。
- 複数の保存を不可分に扱う必要がある場合は、**applicationがトランザクション境界を制御**し、具体的なDB操作はinfrastructureに置く。
- 照会結果だけで整合性を保証せず、条件付き更新・予約・トランザクションなどを要件に応じて使う。
- **コマンドが集約を消費することは、永続化済みの変更のロールバックを意味しない**。所有権が移ったことと、DBの状態が戻ることは別である。

この repository では、`claim`とleaseの`BEGIN IMMEDIATE`と`UPDATE ... WHERE status IN (...)`が既にこの役割（トランザクション境界と条件付き更新による楽観的な競合検出）を持っている。ユースケースをapplicationへ移すときも、この境界と述語は維持する。

### 10. 実施方針と検証

- まず**既存実装と方針の差分を整理**し、**変更範囲を示した**うえで実装する。この文書の`## 棚卸し`がその差分整理にあたる。
- **既存の仕組みが方針を満たす場合は活用する**。置き換えを目的にしない。
- **ドメインイベント、状態ごとの型分け、汎用的な処理実行フレームワークなどは、具体的な必要性がない限り導入しない**。
- **既存テストを活用**し、変更に伴うリスクに応じて、不変条件、正常・異常な状態遷移、復元時の検証、更新競合などの重要な振る舞いを検証する。
- 完了時には、主な変更、検証結果、方針からの例外とその理由、残る課題を報告する。この repository では**receiptの`summary`**に書く。

### 訂正: SQLが生成するタイムスタンプと注入した時刻

`## Consequences`の「`created_at` / `updated_at`のようなSQLiteが`strftime` / `unixepoch`で生成するタイムスタンプはDB側に残り、注入した時刻で置き換えはしない」と、`## 棚卸し`末尾の同趣旨の記述を、次のとおり訂正する。

- **schemaの`DEFAULT`式はそのまま残す**。`migrations/`の`DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))`と`DEFAULT (unixepoch())`は変えない。schemaを変えないという制約はそのままである。
- **runtimeがSQLの中で`strftime('now')` / `unixepoch()`を呼んで更新している箇所は、方針7に従いapplicationに注入した`Clock`の値をbindする形に変える**。対象は`src/infrastructure/sqlite.rs`の`updated_at`（`transition`、`set_goal`、goalの更新、`claim_task`、依存の更新）と`close_goal`の`closed_at`、`src/infrastructure/runtime_store.rs`の`heartbeat_at`（lease・supervisor・run process）、`exited_at`、`workspace_closed_at`、`finish_integration`がtaskを`completed`にするときの`updated_at`、および`SELECT unixepoch()`とstale判定の`heartbeat_at >= unixepoch()-?`の基準時刻である。担当は時刻/IDタスク（キュー上のID 37）。

理由は3つ。

1. テストで固定時刻を使えるようになり、stale判定や`workspace_closed_at`の検証がDBの現在時刻に依存しなくなる。
2. 1つの業務操作の中で基準時刻を揃えられる。いまは同じ操作の中の複数のSQLがそれぞれ別の`unixepoch()`を読む。
3. bindしても**schemaと保存形式は変わらない**。列の型も値の書式（`%Y-%m-%dT%H:%M:%fZ`のTEXT、UNIX秒のINTEGER）も同じなので、外部公開APIの固定と両立する。

`DEFAULT`式が効くのは`INSERT`でその列を指定しなかったときだけなので、既定値をschemaに残したまま、runtimeの更新側だけをbindへ寄せられる。
