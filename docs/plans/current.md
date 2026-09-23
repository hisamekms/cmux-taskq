---
id: plan-rust-runtime-mvp
type: plan
title: Rust runtime MVP
status: completed
created: 2026-09-22
updated: 2026-09-23
milestone: mvp
target: 2026-10-31
owners:
  - hisamekms
depends_on:
  - adr-0001
  - adr-0002
  - adr-0003
  - adr-0004
  - adr-0005
  - adr-0006
  - adr-0007
  - adr-0008
  - adr-0010
  - adr-0012
  - adr-0016
  - adr-0019
---

# Rust runtime MVP

## Goal

最初の到達点を、Claude Codeから登録したタスクをcmux workspaceとGit worktreeで実行し、成果をレビューしてmainへ取り込むドッグフーディングとする。まずdagq自身の小さな改善に使い、その後にCodex対応と配布を進める。

2026-09-22時点でステップ1の[実機検証](../journal/001-claude-lifecycle-spike.md)、ステップ2のRust/SQLiteキュー、ステップ3の1件を実行するsupervisor、ステップ4のreceipt検証・workspace終了・`integrate`・`doctor`/`recover`、ステップ5〜7のrepositoryごとのqueue・並列実行・merge queue、ステップ8のClaude Code pluginを実装し、ステップ4の実機の異常系確認を[010](../journal/010-failure-path-smoke.md)で終えた。利用可能なCLIは[README](../../README.md)に記載する。

同日、運用方針を次のように改めた。1 repositoryに1 queueをユーザーDIRに置く（ステップ5）。依存が解けたtaskは上限まで並列に実行する（ステップ6）。統合はruntimeのmerge queueが行い、最新mainへrebase・再検証のうえ1 task = 1 commitにsquashしてmainに直線の履歴を積む（ステップ7）。maintainer（常駐のClaude Code session。呼称は[ADR-0010](../adr/0010-maintainer-and-resident-supervisor.md)で統一した）が完了確認・レビュー・`integrate`の呼び出し・衝突時のセッションへの指示を行う。maintainerの操作は最初は`read-screen`起点でよく、CLIコマンド単位で切っておき、順次runtimeへ移す。ADRは各ステップの実装taskで追加する。

## First dogfooding scope

- ローカルのmacOS、cmux、認証済みClaude Code、単一repositoryを対象にする。
- queueはrepositoryごとに1つ、ユーザーDIR配下に置き、cwdから解決する。
- 同時実行は`supervise --parallel N`の上限付き並列（既定4）。supervisorは常駐ループで、maintainer sessionとは別の専用ターミナルで動かす。
- Claude Codeは通常の対話セッションで実行する。権限確認や入力待ちはworkspaceで人（maintainer）が対応できるようにする。
- Claude Codeのローカルpluginからバイナリを呼ぶ。SQLite操作とライフサイクル管理はruntimeに集約する。
- 実行成功は`awaiting_integration`。統合はruntimeの`integrate`が1件ずつ、最新mainへのrebase → 再検証 → squash着地で行い、Taskを`completed`にする。fast-forwardやmerge commitは使わない。
- 初期の`integrate`はmaintainerがレビュー後に呼ぶ承認制。承認なしの自動着地は後回し。pushはmaintainerが行う（のちにtask 70で`integrate`が着地後にoriginへpushするようにした。[ADR-0019](../adr/0019-move-routine-maintainer-work-into-the-runtime.md)の決定3）。
- 衝突したrunは`needs_session`で止め、maintainerが`claude --resume <run-id>`でworkspaceを開き直してセッションに解消させる。maintainerは原則コードを変更しない。
- workspaceの終了はsupervisorが行い、成功したworktreeとbranchは着地までは保持する。

## Steps and exit criteria

### 1. 実機で起動と完了通知の経路を検証する

状態: 完了（2026-09-22）。[結果と再現手順](../journal/001-claude-lifecycle-spike.md)。入力待ちと異常系は未検証で、ステップ3・4へ引き継ぐ。

最も不確実なcmuxとClaude Codeの接続を先に確認する。使い捨てrepositoryでworktreeとworkspaceを作り、通常のClaude Codeセッションへ作業指示とrun識別子を渡す。小さな変更、テスト、コミット、完了receiptの出力まで試す。

- インストール済みcmux/Claude Codeのバージョン、起動方法、workspace識別、終了確認、入力待ちの挙動を記録する。
- receiptはrunごとの管理領域へ書き、worktreeを汚さない。途中書き込みを完了と扱わない受け渡し方法を決める。
- receipt到着とセッション終了は別の事象として扱い、実行中のworkspaceを早まって閉じない終了手順を確認する。
- **完了条件:** 人がworkspaceを観察しながら、起動から成果とreceiptの回収まで1件を通せる。ここで決めた契約を後続の実装に使う。

### 2. RustとSQLiteで最小のキューを作る

状態: 完了（2026-09-22）。登録・一覧・詳細・ready/draft/cancel・依存追加/削除・候補確認をCLIとして実装。claimは後続supervisor向けのライブラリAPIとして実装し、DB再open、同時claim、依存変更との競合、イベント保存失敗時のrollbackをテストで確認した。CLIとキューの計15テスト、fmt、Clippyを通過。

Rustプロジェクト、migration、Task/TaskRun、依存関係、イベントを実装する。CLIは登録、一覧、詳細、ready化、実行候補の確認を優先する。

- 自己依存と循環を拒否する。
- 依存がすべて`completed`のready taskだけをトランザクションでclaimし、TaskRunを作る。
- TaskRunにprovider、base commit、branch、worktree、workspace、成果物の参照を保持する。providerはclaudeのみを記録し、provider固有の起動処理はステップ3のadapterへ閉じ込める。
- **完了条件:** DBを開き直して状態が復元でき、競合するclaimでも同じtaskに二つのactive runができないことをテストで確認する。

### 3. 1件を実行するsupervisorを作る

状態: 完了（2026-09-22）。`supervise`がlease取得 → claim → run管理領域とworktree作成 → cmux workspace作成 → 隠しコマンド`session`のwrapper経由でClaude起動 → heartbeat監視 → セッション終了検知までを1件分行う。実装は`src/runtime.rs`、adapterは`src/infrastructure/adapters.rs`、永続化は`src/infrastructure/runtime_store.rs`とmigration `0002_supervisor.sql`。テスト用providerとworkspaceを差し替えたruntimeテスト8件を追加し、正常終了・異常終了・作成失敗時の保持・stale leaseの不奪取・v1からのmigrationを確認した。

使い捨てrepositoryでの[実機スモーク](../journal/003-supervisor.md)（cmux 0.64.25、Claude Code 2.1.278）では、専用workspaceのsupervisorからClaudeを起動し、新規worktreeの信頼確認で待機している間もsupervisorとwrapperのheartbeatが継続することを確認した。確認を進めるとClaudeが修正・unit test・commit・receipt提出を行い、receipt受領後もセッションは維持され、maintainerの`/exit`で`session_exited`、`supervision_finished`が記録されrunは`validating`になった。ログはworktree外の`<db>.runs/<run-id>/`に保存され、workspace・worktree・branchは保持された。

ステップ1の経路をruntimeへ組み込む。claim → worktree作成 → cmux workspace作成 → wrapper/Claude起動 → 監視を実装する。手動で起動し、1件を処理するところから始める。

- キューごとのsupervisor lease、runのheartbeat、ログ保存を最初から入れる。
- 各リソースの識別子を作成の都度保存し、途中失敗や再起動後に追跡できるようにする。
- promptには作業範囲、受け入れ条件、検証方法、コミット、receiptの提出方法を含める。
- **完了条件:** CLIで登録した小さなtaskが独立worktree内のClaude Codeで実行され、進行状態とログを確認できる。起動元のClaude Code終了に監視が依存しない。

### 4. 成功判定・統合待ち・障害時の扱いを完成させる

状態: 完了（2026-09-22）。実装は[005](../journal/005-receipt-validation.md)、[006](../journal/006-workspace-close.md)、[007](../journal/007-session-exit-request.md)、[008](../journal/008-integration-confirm.md)、[009](../journal/009-doctor-recover.md)、[015](../journal/015-e2e-happy-path.md)。008の「手動mergeを確認する`integrate`」はステップ7で置き換えた。実機の異常系は[010](../journal/010-failure-path-smoke.md)で、使い捨てrepositoryの`supervise --parallel 2`（cmux 0.64.25、Claude Code 2.1.278）に対してClaude異常終了、supervisor再起動（`doctor`→`recover`→`ready`）、検証コマンド失敗、cleanup失敗、並列中の1 runの異常とrecover、merge queueの衝突の`needs_session`からの`claude --resume`による解消を確認し、二重起動と成果の喪失がないことを記録した。010が挙げた改善候補（非0終了時の`last_error`、active runのない常駐supervisorの可視化、信頼確認の自動化、resume中の`integrate`、失敗runのworkspaceの後始末）は後続タスクで扱う。

receiptにはrun ID、結果、commit SHA、実施したunit test/E2E/subagent reviewの結果と証跡を記録する。適用対象外の検証には理由を要求し、taskの受け入れ条件に照らして扱う。

- supervisorはreceiptの整合性、対象branchのcommit、clean worktree、必要な検証結果を確認する。receipt上の自己申告だけで成功にせず、指定された検証コマンドはsupervisor側でも実行する。
- 検証成功とセッション終了を確認してworkspaceを閉じ、`awaiting_integration`にする。cleanup失敗は記録し、閉じられていないworkspaceをcleaned扱いしない。
- 失敗、中断、不正または未提出receipt、heartbeat切れではworkspace/worktreeを保持し、後続taskを解放しない。
- 最小の`doctor`と`recover`を用意する。旧実行が停止したことを確認して明示的に復旧し、再試行は新しいTaskRunにする。孤児runは自動再実行しない。
- **完了条件:** 正常終了、Claude異常終了、supervisor再起動、検証失敗、cleanup失敗を確認でき、二重起動や成果の喪失が起きない。

### 5. queueをrepositoryごとにユーザーDIRへ置く

状態: 実装済み（2026-09-22、[016](../journal/016-queue-per-repository.md)、[ADR-0006](../adr/0006-queue-per-repository.md)）。`--db`なしでcwdのrepositoryから`$XDG_DATA_HOME/dagq/<hash>/queue.db`に解決し、run dirは`runs/<run-id>/`。`supervise --repo`と`integrate --repo`は任意のoverrideに変わり、pluginは`--db`/`--repo`を渡さない。1 repositoryに1 queue。

- DBは`~/.local/share/dagq/<Git common directoryの正規化パスのhash>/queue.db`。run dir・worktree・ログも同じ配下。
- CLIはcwdから`git rev-parse --git-common-dir`で解決する。`--db`は使い捨てrepositoryとテスト用のoverrideとして残す。`supervise --repo`と`integrate --repo`は不要になる。
- **完了条件:** repository内の任意のworktreeから`--db`なしで同じqueueが使え、別repositoryからは別queueになることをテストで確認できる。

### 6. 依存が解けたtaskを並列に実行する

状態: 実装済み（2026-09-22、[017](../journal/017-parallel-runs.md)、[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)）。leaseは`run_leases`でrun単位（schema v5）、queue全体の実行枠は廃止。`supervise --parallel N`（既定4）は常駐ループで、`--once`で1 batch。1 runのruntime errorはそのrunだけをabandonし、`doctor`/`recover`はrunごと。2件同時と依存taskの後追い、1 run失敗の非波及、1 runだけのrecoverをunit testとe2eで確認した。

- queue全体の実行枠をやめ、Taskごとの未完了run 1件の制約だけ残す。leaseはrun単位にし、`doctor`/`recover`はrunごとに動く。
- `supervise --parallel N`（既定4）は常駐ループで、候補を上限までclaim → 起動 → 各runの監視 → 検証を繰り返す。
- 1 runの異常が他のrunに波及しない。
- **完了条件:** 依存のないtaskが同時に走り、依存のあるtaskは先行taskの`completed`まで待ち、1 runの失敗とrecoverが他に影響しないことをテストで確認できる。

### 7. merge queueでmainに直線の履歴を積む

状態: 実装済み（2026-09-22、[018](../journal/018-merge-queue.md)、[ADR-0008](../adr/0008-merge-queue-squash-landing.md)）。`integrate ID` / `integrate --next`がスロット（`integrating`、schema v6）を取り、worktreeを最新mainへrebase → 再検証（receiptがHEADを指す、mainの子孫、clean、検証コマンド）→ `commit-tree`で1 commitにsquash → mainをfast-forward（checkoutがあればそこで`merge --ff-only`）→ `integrated`/`completed` → worktreeとbranch削除（履歴は`refs/dagq/runs/<run-id>`）。衝突と再検証失敗は`needs_session`で止め、`failed` receiptはrunを`failed`にする。衝突なし・FIFO・衝突後のセッション解消・rebase後の検証失敗・スロットの排他と`recover`をunit testで、1件の着地と2件同時からの`needs_session`解消をe2eで確認した。着地はruntimeの`integrate`が行う。

- 統合スロットは1つ、検証完了の古い順。`integrating` → 最新mainへrebase → 再検証（親がmain head、clean、検証コマンド）→ treeを1 commitにsquash（trailer `Dagq-Task` / `Dagq-Run`）してmainを進める → `integrated`。
- 衝突は`rebase --abort`して`needs_session`で止め、maintainerがresumeしたセッションが解消・再検証・receiptを書き直す。
- run branchは`refs/dagq/runs/<run-id>`に残す。初期はmaintainerが`integrate`を呼ぶ承認制。
- **完了条件:** 衝突なしのrunがClaudeなしで着地し、衝突したrunがセッションでの解消後に着地し、mainが直線で1 task = 1 commitになることをテスト（e2e含む）で確認できる。

### 8. Claude Codeから使う薄いローカルpluginを作る

状態: 完了（2026-09-22、[011](../journal/011-claude-code-plugin.md)、ステップ9で確認）。`plugins/claude-dagq/` にlauncherと3つのskill（登録・確認 / 実行・統合 / 復旧）を置き、`claude --plugin-dir` で読み込んだセッションから登録と確認を実機確認した。実行・統合・復旧をClaude Codeから通す確認はステップ9で行い、その時点で完了にする。ステップ5〜7でCLIの引数（`--db`、`--repo`、`--parallel`、`integrate`の意味）が変わるので、skillの追従は016・017・018の各taskに含める。

ローカルビルドしたバイナリとClaude Code pluginを接続する。skillはタスク登録、状態確認、実行開始の手順を提供し、エージェント向けに結果を読める形で返す。

- バイナリの場所とバージョンを確認する。
- taskの説明、受け入れ条件、依存、検証方法をCLIへ渡す。
- 完了通知はステップ1で検証した明示的な経路を使い、停止hookだけで成功を決めない。
- **完了条件:** Claude Code内の依頼から登録・実行・結果確認まで操作できる。pluginがDBを直接変更しない。

### 9. dagq自身でドッグフーディングする

状態: 完了（2026-09-22）。固定バイナリ `18800cd` と常駐 `supervise --parallel 4` で、[012](../journal/012-dogfood-independent-task.md)（独立task）、[019](../journal/019-replace-interim-workflow.md)（AGENTS.mdの運用置き換え）、[013](../journal/013-dogfood-dependent-tasks.md)（A・C並列、BはAの着地commitから）、[014](../journal/014-dogfood-failure-recovery.md)（agent killからの再試行）を6 task・7 runで通し、6件を `integrate` で着地させた。DBの手修正なし。Claude Codeからの登録・実行・着地・復旧はmaintainer sessionがpluginと同じCLIで行い、ステップ8も完了とする。

ローカルに固定したビルド済みバイナリを使い、実行中のruntimeを作業成果で置き換えない。maintainerは常駐のClaude Code sessionで、`read-screen`で完了を確認し、差分をレビューして`integrate`を呼び、`needs_session`のrunにはresumeで指示する。

1. [012](../journal/012-dogfood-independent-task.md): 独立taskを1件完走し、差分と証跡をレビューして着地させる。ここでopenなジャーナルをqueueへ移行する。
2. [019](../journal/019-replace-interim-workflow.md): AGENTS.mdのmaintainer/worker運用をdagq前提に置き換える。以降のtaskは新しい手順で流す。
3. [013](../journal/013-dogfood-dependent-tasks.md): A → Bの依存taskと独立したCを登録し、AとCが並列に走り、Aの着地後にBがAの変更を含むmainから始まることを確認する。
4. [014](../journal/014-dogfood-failure-recovery.md): 失敗または中断を1件起こし、リソース保持、状態確認、明示復旧、再試行を確認する。

**完了条件:** 上記シナリオがDBの手修正なしで通る。セットアップ、実行、成果の取り込み、復旧の手順を文書化し、次の小さな開発taskを同じ手順で流せる。

## Ordering

`1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9`。全ステップ完了（2026-09-22）。M1達成。以降の開発taskはAGENTS.mdの手順でdagqに登録して流す。次はAfter first dogfoodingの項目をtaskに割る。

## After first dogfooding

- 利用で見つかった詰まりを修正し、継続的な実行と復旧を安定させる。task 15の実行中にsupervisorを止めたらworkerの成果が捨てられた件は、supervisorが死んだ後もwrapperが生きている`running` / `validating`のrunを次のsupervisorが引き継ぐ（adopt）ことで直した（task 24、[ADR-0012](../adr/0012-adopt-stale-lease-of-live-wrapper.md)、2026-09-22）。`recover`はwrapperが死んだrunと`claimed` / `starting` / `integrating`のための経路として残る。
- 複数のtaskが解く上位の課題を`Goal`として表現し、依存元のreceipt summary・result commit、同時実行中の兄弟、goalの記述と制約をworkerのpromptに流す（[ADR-0009](../adr/0009-goal-groups-tasks.md)、proposed）。段階1（依存元の情報をpromptへ）、goalエンティティ、prompt拡張、plugin skillの4 taskとして014の後に登録し、この4件を最初のgoalの実例にする。
- 役割名をsupervisor / maintainer / workerに統一し、[ADR-0010](../adr/0010-maintainer-and-resident-supervisor.md)を追加する（journal 021のT1。docs/design・docs/plans・docs/READMEの旧称とmaintainerを指すoperatorをmaintainerにし、[overview](../design/overview.md)に用語集を足す）。
- `dagq up` / `down`とlaunchd常駐（journal 021のT2）。supervisorをLaunchAgent（`KeepAlive`）として常駐させ、`up`がmaintainer workspace（`dagq <repo> maintainer`、`DAGQ_ROLE` / `DAGQ_QUEUE`付き）を初期prompt付きの`claude`で作り、PIDの死んだ`supervisors`登録を消してから起動する。`down`はbootoutしてdrain（`--wait` / `--force`）。worker workspace名を`dagq <repo> <task-id> <run-id>`にし、supervisor logを`<queue dir>/logs/supervisor-<started_at>.log`に書いて`locate`にlog dirを足す。launchd起動のsupervisorがcmuxに接続するにはcmuxのsocket passwordが要り、`up`のpreflightでの確認（task 21）とlaunchdなしの`up --in-cmux`（task 22）を[ADR-0011](../adr/0011-cmux-socket-password-and-in-cmux-fallback.md)で決めた。task 21とtask 22は着地済みで、modeは`supervisors.mode`（schema v9）に記録して`status` / `doctor` / `down`が読む。task 30（[ADR-0014](../adr/0014-up-replaces-a-supervisor-of-another-binary-version.md)）で`supervisors.binary_version`（schema v10）を足し、`up`がversionの違うliveなsupervisorをdrainして入れ替えるようにした（`--no-wait`は走行中のrunがあれば入れ替えない）。固定バイナリの更新は「ファイルを置き換えて`up`」になった。
- plugin skillのmaintainer化（journal 021のT3）。CLIの使い方をskill `dagq-maintain`へ集め、maintainerの初期promptをruntimeが生成し、AGENTS.mdをrepository固有の注意だけにする。
- maintainerの操作をruntimeへ移す。次の3つに分ける。
  - 通知経路と圧縮出力（goal 6、[ADR-0016](../adr/0016-maintainer-notification-and-compact-output.md)）。maintainerを使い捨てのsessionにし、`status` / `watch` / `doctor`の3入口、run_eventsのkind名の公開契約とdomainでのattention判定、`cmux notify`、既定の圧縮出力と`--full`、`review`による`review.md`、pluginの`SessionStart` hookとskill分割、`maintainer_prompt`の縮約を実装する。`status`のattentionとcursor、`events --after`、`watch`、domainのattention判定はtask 57で実装済み。`WorkspaceBackend::notify`（`cmux notify`）はtask 75で実装済みで、supervisorはまだ送らない（送る条件と宛先はADR-0022でgoal 10へ）。pluginの`SessionStart` hook（matcher `compact` / `clear`、`DAGQ_ROLE=maintainer`の時だけ`status`を出力）、skillの分割（`dagq` / `dagq-maintain` / `dagq-land` / `dagq-session` / `dagq-recover`、参照情報は各skillの`reference/`）、5行以内の`maintainer_prompt`はtask 65で実装済み。
  - `ask` / `answer`による相談経路（goal 10、[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)）。maintainerとworkerの相談をqueueのask（`asks`表、`ask` / `answer` / `asks`、`ask_opened` / `ask_answered`、`watch --role`）にし、`up`が`[<repo>]inbox`と`[<repo>]planner`を開く。着地はsubagentレビューが通ればmaintainerが`integrate`を呼び、疑義のあるときだけ`approve_landing`のaskで人に聞く。`cmux notify`は`ask_opened`のときだけinbox宛て。
  - maintainerの定型作業のruntimeへの移管（goal 8、[ADR-0019](../adr/0019-move-routine-maintainer-work-into-the-runtime.md)）。`needs_session`のrunをsupervisorがresumeして定型の解消依頼を送り、`integrate`を呼び済みのrunは着地まで進める。`exit_request_timed_out`でleaseを手放さない、`integrate`の着地後のpush（task 70で実装: `--no-push`、`push_finished` / `push_skipped` / `push_failed`、`push main`のattention）、receiptの`follow_ups`のdraft登録、taskが要求するevidence（`add --evidence`）の検証、workerのダイアログ待ち（`prompt_waiting`）の検知を加える。supervisorが手放した（leaseの無い）未完了runを`recover run`のattentionにすることはtask 79で実装済み（[ADR-0025](../adr/0025-leaseless-unfinished-run-is-a-recover-run-attention.md)）。承認なしの自動着地はfirst dogfoodingの対象外のまま（のちに[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)で、レビュー通過のrunはmaintainerが着地させ、疑義のあるときだけ人に聞くと改めた）。
  - 実行効率（goal 11、[ADR-0023](../adr/0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)）。`verification_commands`は`integrate`のrebase後の1回だけにし（`validating`はreceipt・commit・clean・evidenceの照合だけ、task 48のskip判定は廃止）、`awaiting_integration`のrunはsupervisorがheadlessのClaudeでreviewして（`review_started` / `review_finished`）passなら着地、concernなら`approve_landing`のask、失敗は`review_failed`。repository rootの`dagq.toml`の`[run.env]`をworkerと検証に渡し、この repositoryでは`CARGO_TARGET_DIR`をqueue配下で共有する。`graph`で依存木と解放数を出し、claim順を解放数の多い順にする。`stats`でrunとgoalの時間と閾値超えを返す。`stats`はtask 94で実装済み。workerのsessionはreviewの後まで残し、機械的な指摘は`revise`のverdictで生きているworkerに返し（runごとに2回まで、`revise_requested` / `revise_finished`）、passなら`/exit`の前に`git merge-tree`で衝突を事前判定する（[ADR-0027](../adr/0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)、未実装）。workerの起動コストの削減（決定4）はtask 92で実装済み: run設定の`autoMode.environment`で「Teach auto mode」を抑止し、`up`がClaude Codeに未信頼のrepository rootをpreflightで拒み、worker promptの読む範囲を限定し、supervisorが`first_commit_observed`を記録して`stats`の`startup`が埋まる。LSP pluginの推奨はsettingsでもflagでも抑止できない（[provider-lifecycle](../design/provider-lifecycle.md#起動時のダイアログ)）。
  - maintainerの退役（goal 12、[ADR-0024](../adr/0024-retire-maintainer-into-jobs-and-observer.md)）。役割をsupervisor / worker / planner / inbox / observerの5つにし、maintainerの記述はoverviewの用語集で読み替える。失敗runはsupervisorが起動するheadlessのtriage job（`retry` / `resume` / `ask`）が扱い、wrapperが死んだrunの`recover`とtriage済みrunのworkspaceのcloseはruntimeが行う。observerはsupervisorのtimer（既定1時間、`--observe-interval`）で起動するjobで、`observation`のnote、`blocked`のask、draftのgoalだけを書き、run / task / goalの状態は変えない。goalに`goal add --draft` / `goal ready`のdraft状態を足す。`up`はmaintainerを開かず、常駐はsupervisor / inbox / planner。skillは`dagq-maintain` / `dagq-land` / `dagq-session`を消して4本にする。
- cmux workspaceの識別を名前からUUIDに移す（goal 9、[ADR-0026](../adr/0026-identify-workspaces-by-uuid-env-and-queue-group.md)）。task 76で、`up`がmaintainerとin-cmux supervisorのworkspace UUIDを`session_workspaces`（schema v11）に記録して再入判定・reuseをそのUUIDで行い（`find_named`を廃止）、`up`とsupervisorが作るworkspaceに`--env DAGQ_ROLE` / `DAGQ_QUEUE`と機械可読のdescriptionを付け、queueごとのworkspace group（external IDはqueue hash）に入れるようにした。titleを`[<repo>]<role>`形式に改めるのは後続task。
- Codex provider、明示選択、Claude起動不能時のfallbackを追加する。
- バイナリリリース、checksum、pluginとのバージョン互換性はgoal 2「claude-taskqを配布可能なMVP (v0.1.0)にする」（当時の名前。現`claude-dagq`）で実装した（2026-09-22）。`v*`のtag pushで`aarch64-apple-darwin`のtar.gzと`SHA256SUMS`をGitHub Releaseに添付する`.github/workflows/release.yml`（task 28）、repository rootの`.claude-plugin/marketplace.json`による`claude plugin marketplace add` / `install`とlauncherのmajor.minor不一致警告（task 29）、`up`がversionの違うsupervisorをdrainして入れ替える更新手順（task 30、[ADR-0014](../adr/0014-up-replaces-a-supervisor-of-another-binary-version.md)）、READMEのGetting startedとUpgrade（task 31）。LICENSE（MIT）とmainのCIも同じgoalで入れた。Codex pluginは未着手で、goal 2の制約でも対象外。
- 既存Pythonキューからtask ID、依存、run履歴、ログ参照を移行する。

## Out of scope for first dogfooding

- Codex対応と自動fallback
- 複数repositoryをまたぐ依存
- 承認なしの自動着地、統合前の先行branchから後続taskを実行する方式
- 公開配布、自動更新、旧Pythonキューの移行
- cmux以外のworkspaceバックエンド、Web UI、本番への自動デプロイ、外部スケジューラー
