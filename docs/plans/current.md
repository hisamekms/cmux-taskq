---
id: plan-rust-runtime-mvp
type: plan
title: Rust runtime MVP
status: active
created: 2026-09-22
updated: 2026-09-22
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
---

# Rust runtime MVP

## Goal

最初の到達点を、Claude Codeから登録したタスクをcmux workspaceとGit worktreeで実行し、成果をレビューして手動でmainへ取り込むドッグフーディングとする。まずcmux-taskq自身の小さな改善に使い、その後にCodex対応と配布を進める。

2026-09-22時点でステップ1の[実機検証](../journal/001-claude-lifecycle-spike.md)、ステップ2のRust/SQLiteキュー、ステップ3の1件を実行するsupervisorを実装した。利用可能なCLIは[README](../../README.md)に記載する。receiptの検証、workspaceの終了、`completed`への遷移、復旧コマンド、pluginは未実装。

## First dogfooding scope

- ローカルのmacOS、cmux、認証済みClaude Code、単一repositoryを対象にする。
- 同時実行数は1。タスク登録と実行開始は明示操作とし、常駐サービス化は後回しにする。
- supervisorは起動元のClaude Codeと独立した専用ターミナルで動かす。
- Claude Codeは通常の対話セッションで実行する。権限確認や入力待ちはworkspaceで人が対応できるようにする。
- Claude Codeのローカルpluginからバイナリを呼ぶ。SQLite操作とライフサイクル管理はruntimeに集約する。
- 初期運用は既存設計の`integrated`方式に絞る。実行成功は`awaiting_integration`とし、mainへの取り込み確認後にTaskを`completed`にする。
- mainへの統合は手動。後続taskのworktreeは、先行taskの統合を確認したmainから作る。
- workspaceの終了はsupervisorが行い、成功したworktreeとbranchも統合までは保持する。

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

使い捨てrepositoryでの[実機スモーク](../journal/003-supervisor.md)（cmux 0.64.25、Claude Code 2.1.278）では、専用workspaceのsupervisorからClaudeを起動し、新規worktreeの信頼確認で待機している間もsupervisorとwrapperのheartbeatが継続することを確認した。確認を進めるとClaudeが修正・unit test・commit・receipt提出を行い、receipt受領後もセッションは維持され、operatorの`/exit`で`session_exited`、`supervision_finished`が記録されrunは`validating`になった。ログはworktree外の`<db>.runs/<run-id>/`に保存され、workspace・worktree・branchは保持された。receiptの検証と`awaiting_integration`への遷移はステップ4で行う。

ステップ1の経路をruntimeへ組み込む。claim → worktree作成 → cmux workspace作成 → wrapper/Claude起動 → 監視を実装する。手動で起動し、1件を処理するところから始める。

- キューごとのsupervisor lease、runのheartbeat、ログ保存を最初から入れる。
- 各リソースの識別子を作成の都度保存し、途中失敗や再起動後に追跡できるようにする。
- promptには作業範囲、受け入れ条件、検証方法、コミット、receiptの提出方法を含める。
- **完了条件:** CLIで登録した小さなtaskが独立worktree内のClaude Codeで実行され、進行状態とログを確認できる。起動元のClaude Code終了に監視が依存しない。

### 4. 成功判定・統合待ち・障害時の扱いを完成させる

receiptにはrun ID、結果、commit SHA、実施したunit test/E2E/subagent reviewの結果と証跡を記録する。適用対象外の検証には理由を要求し、taskの受け入れ条件に照らして扱う。

- supervisorはreceiptの整合性、対象branchのcommit、clean worktree、必要な検証結果を確認する。receipt上の自己申告だけで成功にせず、指定された検証コマンドはsupervisor側でも実行する。
- 検証成功とセッション終了を確認してworkspaceを閉じ、`awaiting_integration`にする。cleanup失敗は記録し、閉じられていないworkspaceをcleaned扱いしない。
- 手動merge後、成果commitがmainに含まれることを確認する操作でTaskを`completed`にする。初期版はmerge/fast-forwardを対象とし、squash/cherry-pickの同等性判定は後回しにする。
- 失敗、中断、不正または未提出receipt、heartbeat切れではworkspace/worktreeを保持し、後続taskを解放しない。
- 最小の`doctor`と`recover`を用意する。旧実行が停止したことを確認して明示的に復旧し、再試行は新しいTaskRunにする。孤児runは自動再実行しない。
- **完了条件:** 正常終了、Claude異常終了、supervisor再起動、検証失敗、cleanup失敗を確認でき、二重起動や成果の喪失が起きない。

### 5. Claude Codeから使う薄いローカルpluginを作る

ローカルビルドしたバイナリとClaude Code pluginを接続する。skillはタスク登録、状態確認、実行開始の手順を提供し、エージェント向けに結果を読める形で返す。

- バイナリの場所とバージョンを確認する。
- taskの説明、受け入れ条件、依存、検証方法をCLIへ渡す。
- 完了通知はステップ1で検証した明示的な経路を使い、停止hookだけで成功を決めない。
- **完了条件:** Claude Code内の依頼から登録・実行・結果確認まで操作できる。pluginがDBを直接変更しない。

### 6. cmux-taskq自身でドッグフーディングする

ローカルに固定したビルド済みバイナリを使い、実行中のruntimeを作業成果で置き換えない。最初はドキュメント改善、その次に小さな不具合修正またはテスト追加を流す。

1. 独立taskを1件完走し、差分と証跡をレビューしてmainに取り込む。
2. A → Bの依存taskを登録し、Aの実行成功だけではBが始まらず、Aの統合確認後にBが変更を含むmainから始まることを確認する。
3. 失敗または中断を1件起こし、リソース保持、状態確認、明示復旧、再試行を確認する。

**完了条件:** 上記3シナリオがDBの手修正なしで通る。セットアップ、実行、成果の取り込み、復旧の手順を文書化し、次の小さな開発taskを同じ手順で流せる。

## Ordering

`1 → 2 → 3 → 4 → 5 → 6`。ステップ1の正常系検証、ステップ2の最小キュー、ステップ3の1件を実行するsupervisorは完了。次の着手単位はステップ4の成功判定・統合待ち・障害時の扱いとする。ステップ3で未検証のClaude異常終了、supervisor再起動、heartbeat切れ後の復旧、cleanup失敗はステップ4で扱う。

## After first dogfooding

- 利用で見つかった詰まりを修正し、継続的な実行と復旧を安定させる。
- Codex provider、明示選択、Claude起動不能時のfallbackを追加する。
- バイナリリリース、checksum、pluginとのバージョン互換性、Codex pluginを整備する。
- 既存Pythonキューからtask ID、依存、run履歴、ログ参照を移行する。

## Out of scope for first dogfooding

- Codex対応と自動fallback
- 複数taskの同時実行、複数repository運用
- 自動merge、統合前の先行branchから後続taskを実行する方式
- 公開配布、自動更新、旧Pythonキューの移行
- cmux以外のworkspaceバックエンド、Web UI、本番への自動デプロイ、外部スケジューラー
