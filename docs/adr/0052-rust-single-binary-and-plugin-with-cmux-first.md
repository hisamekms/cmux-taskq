---
id: adr-0052
type: adr
title: runtimeをRustの単一バイナリdagqとpluginで配り、cmuxを最初のworkspace backendにする
status: accepted
created: 2026-09-26
updated: 2026-09-26
accepted_on: 2026-09-26
supersedes:
  - adr-0001
  - adr-0002
  - adr-0005
  - adr-0015
owners:
  - hisamekms
tags:
  - runtime
  - rust
  - cmux
  - distribution
  - plugins
  - naming
related:
  - adr-0001
  - adr-0002
  - adr-0005
  - adr-0013
  - adr-0015
  - adr-0026
  - adr-0028
  - adr-0030
  - adr-0042
  - adr-0045
  - adr-0047
  - design-overview
  - design-plugin-integration
---

# ADR-0052: runtimeをRustの単一バイナリdagqとpluginで配り、cmuxを最初のworkspace backendにする

## Context

dagqはSQLiteのqueue、プロセスの監視、cmuxの操作、Claude / Codexの起動を行うruntimeで、複数のrepositoryで使える開発基盤にする。基盤の選択は4本のADRに分かれていた。

- [ADR-0001](0001-rust-runtime.md)（2026-09-22）: runtimeとsupervisorをRustで実装し、単一のバイナリとして配り、domain / application / infrastructureを分ける。Pythonのスクリプト集合は配布と長いプロセス管理に追加の前提が要り、TypeScript / Node.jsは利用者にNodeの環境を要求するので採らなかった。
- [ADR-0002](0002-cmux-first.md)（2026-09-22）: cmuxを必須のworkspace backendにし、domain / applicationはportを介して使う。
- [ADR-0005](0005-binary-and-plugin-distribution.md)（2026-09-22）: runtimeをバイナリ、agent integrationをpluginとして配る。
- [ADR-0015](0015-rename-to-dagq.md)（2026-09-22）: 改名前の名前（`cmux-taskq`）がcmuxへの依存を名前に固定し、`taskq` / `tasq`が同種のツールの名前として多発していたので、`dagq`（全レジストリが空きで、「依存DAGをキューで流す」という本質を言う）に揃えた。

ADR-0001・0002・0005は改名前の名前をバイナリ名・プロダクト名として書いており、ADR-0015の対応表がそれを上書きした。ADR-0015の対応表のうちcmux workspace名の行は[ADR-0026](0026-identify-workspaces-by-uuid-env-and-queue-group.md)・[ADR-0028](0028-workspace-titles-are-repo-and-role.md)などに、skillの行は後の役割の再編（今は[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定1）に上書きされ、versionの行はその時点の記録で、付け方は[ADR-0045](0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)の決定1が決める。ADR-0015のschemaを変えない決定（決定3）と、文書と既存ADRを凍結する決定（決定5）、切り替え手順は一度きりか失効している。

[ADR-0042](0042-adr-is-superseded-whole-and-deprecation-date-is-deprecated-on.md)の規則に従い、このADRはADR 0001・0002・0005・0015を丸ごと置き換える（[ADRの棚卸し](../plans/adr-inventory.md)の組A）。4本の生きている決定を今の名前と役割で書き直して引き継ぎ、上書きされた決定は今の形で書くか、今それを持つADRを参照する。新しい決定は足さない。

## Decision

### 実装と構成

1. **Rustの単一バイナリ。** runtimeとsupervisorはRustで実装し、単一のバイナリ`dagq`として配る。SQLite・プロセス・ファイル・CLIを利用者が別の実行環境を用意せずに使えるようにするため。
2. **レイヤーの分離。** domain / application / infrastructureを分ける。境界の引き方（1 crateのmodule境界、「型＋関数」）は[ADR-0013](0013-layered-architecture-and-type-function-style.md)が決める。

### workspace backend

3. **cmuxを必須のworkspace backendにする。** runtimeはcmuxでworkspaceを作り、その中でGit worktreeとagent sessionを動かす。cmuxのCLI / socketは実行環境の前提になる。backendを最初から複数持つことはしない。
4. **portを介す。** domain / applicationはcmuxのAPIを直接参照せず、workspaceのport（`WorkspaceBackend`、[overview](../design/overview.md)）を介して使う。別のbackendを足すときはadapterとpluginの設定を足す。workspaceの識別とtitleはADR-0026とADR-0028が決める。

### 配布

5. **runtimeはバイナリ、agent integrationはplugin。** runtimeをバイナリ`dagq`として配り、Claude CodeとCodexのpluginはskill・hook・adapterの設定からそのバイナリを呼ぶ。pluginはruntimeを同梱しない。今あるのはClaude Codeのplugin（`plugins/claude-dagq`）で、Codexのpluginは未着手（決定は変わらない）。
6. **共通のrepositoryから出す。** このrepository（`hisamekms/dagq`）から、各ecosystem向けのmanifest（marketplace）とplugin packageを出す。GitHub Releaseのバイナリに加える配布経路（crates.io）は[ADR-0030](0030-publish-to-crates-io-on-tag-push-with-trusted-publishing.md)が決める。

### 名前

7. **名前の対応表。** repository・crate・バイナリ・plugin・環境変数・データディレクトリ・branchとrefの接頭辞・launchd label・git trailer・Releaseのarchive名は`dagq`に揃え、次の表に固定する。

   | 対象 | 名前 |
   | --- | --- |
   | crate / package / バイナリ | `dagq`（`cargo build`の成果物は`target/*/dagq`、testsは`env!("CARGO_BIN_EXE_dagq")`） |
   | GitHub repository | `hisamekms/dagq`（`https://github.com/hisamekms/dagq`、Releaseは`https://github.com/hisamekms/dagq/releases`） |
   | Releaseのarchive | `dagq-v<ver>-aarch64-apple-darwin.tar.gz` |
   | pluginディレクトリ | `plugins/claude-dagq` |
   | plugin.jsonの`name` / `displayName` | `claude-dagq` / `dagq` |
   | marketplace（`.claude-plugin/marketplace.json`）の`name` | `dagq` |
   | pluginのインストール / 更新 | `claude plugin marketplace add hisamekms/dagq`、`claude plugin install claude-dagq@dagq`、更新は`claude plugin update claude-dagq@dagq` |
   | launcher | `plugins/claude-dagq/bin/dagq` |
   | 環境変数 | `DAGQ_`で始める（`DAGQ_BIN` / `DAGQ_DB` / `DAGQ_ROLE` / `DAGQ_QUEUE` / `DAGQ_E2E_CMUX`など） |
   | データディレクトリ（`DATA_DIR_NAME`） | `$XDG_DATA_HOME/dagq/<hash>/`（hashの計算と`queue.db`・`runs`・`logs`・`repository`の名前はqueueの場所を決めるADRに従う） |
   | launchd label（`LAUNCH_AGENT_PREFIX`とplist名） | `com.dagq.<hash>` |
   | run branch | `dagq/<run-id>` |
   | 履歴ref | `refs/dagq/runs/<run-id>` |
   | squash commitのtrailer | `Dagq-Task` / `Dagq-Run` |

   表に入れないもの:
   - skill（ディレクトリ名とSKILL.mdの`name`）は今の役割に合わせた`dagq` / `dagq-inbox` / `dagq-planner` / `dagq-recover`で、役割はADR-0047の決定1が決める。
   - cmux workspaceのtitleはADR-0028、識別はADR-0026が決める。
   - version（`Cargo.toml`と`plugins/claude-dagq/.claude-plugin/plugin.json`）の付け方はADR-0045の決定1が決める。
8. **文言。** エラー文・ログ・コメント・ドキュメントではプロダクトを「dagq」、queueを「dagq queue」と書く。「cmuxとGit worktreeで動く」という説明は事実なので残す。
9. **互換shimを作らない。** 改名前の名前（ADR-0015に記録がある）の環境変数の読み取り、データディレクトリの探索、branch接頭辞の認識はどれも実装しない。改名前の名前を書いた旧いbranch・ref・着地済みcommitのtrailerは移さず書き換えず、runtimeはそれらを見ない。

### 旧ADRの決定からの対応

ADR 0001・0002・0005は決定に番号が無いので、Decisionの箇条を上から数えた番号（[ADRの棚卸し](../plans/adr-inventory.md)の「箇条N」）で示す。後のADR（0036以降）で、この4本の決定を番号で参照するものは無い（2026-09-26にgrepで確認）。

| 旧ADRの決定 | このADR |
| --- | --- |
| ADR-0001 箇条1（Rustで実装し単一のバイナリで配る） | 決定1（バイナリ名は`dagq`） |
| ADR-0001 箇条2（domain / application / infrastructureを分ける） | 決定2 |
| ADR-0002 箇条1（cmuxを必須のbackendにする） | 決定3（プロダクト名は決定7の`dagq`） |
| ADR-0002 箇条2（portを介す） | 決定4 |
| ADR-0005 箇条1（バイナリとplugin） | 決定5 |
| ADR-0005 箇条2（共通repoからmanifestとpackage） | 決定6 |
| ADR-0015 決定1（対応表） | 決定7（workspace名はADR-0026・ADR-0028、skillはADR-0047 決定1、versionはADR-0045 決定1を参照） |
| ADR-0015 決定2（文言の置き換え） | 決定8 |
| ADR-0015 決定3（schemaとAPPLICATION_IDを変えない、versionを0.2.0に） | 引き継がない（一度きりの記録） |
| ADR-0015 決定4（互換shimを作らない） | 決定9 |
| ADR-0015 決定5（文書と既存ADRの凍結） | 引き継がない（失効。journalはADR-0036が削除し、ADRの本文を変えない規則はADR-0042 決定6） |
| ADR-0015 Consequences 1（切り替え手順） | 引き継がない（一度きり。queueの束縛の付け替えは`rebind`、ADR-0020） |

## Alternatives

- Pythonのスクリプト集合でruntimeを書く: 試作は速いが、配布と長いプロセス管理に追加の前提が要り、利用者にPythonの環境を要求する。
- TypeScript / Node.jsで書く: アプリとの共有はあるが、利用者にNodeの環境が残る。
- backendを最初から複数持つ / tmuxを使う: 初期の設計とtestの範囲が広がる。tmuxは今のworkspaceの運用と合わない。
- pluginにruntimeを同梱する: platformごとのバイナリと実行環境の扱いが複雑になる。
- 改名前の名前のままにする / 別の候補（`hatchq`など）にする: ADR-0015のContextのとおり、配布名を確保できないか、既存の製品と音が近い。
- 互換shim（旧名のwrapper、旧環境変数のfallback、旧データディレクトリの自動移行）を作る: 利用者が1人で切り替えが一度きりだったので、shimの保守の費用が手作業の費用を上回る。
- ADR 0001・0002・0005・0015を置き換えずに残す: 旧い名前をバイナリ名として書いたADRと、上書きされた対応表の行が`accepted`のまま残り、単独で開いた読み手が今も有効と読む（ADR-0042）。

## Consequences

- Rustのbuildとplatformごとの配布が要る。今の対応platformはmacOS Apple Silicon（`aarch64-apple-darwin`）で、release artifact・checksum（`SHA256SUMS`）・インストールの手順を持つ。一方、SQLite・プロセス・ファイル・CLIを自己完結したバイナリで提供できる。
- pluginのlauncherは、バイナリを`DAGQ_BIN`、次にPATHから解決し、見つからなければReleaseからの入れ方を示して止まり、バイナリとの互換を`X.Y`で見る（ADR-0045 決定2）。
- cmuxのCLI / socketが実行環境の前提になる。別のbackendを足すときはportのadapterとpluginの設定を足す。
- 検索でDAQ（data acquisition）と混ざる可能性があるので、READMEの冒頭で「dependency DAG queue」と明示して緩和している。crates.ioの`dagq`の確保はADR-0030で行った。
- 着地済みの古いcommitには改名前のtrailerが残る。runtimeが読むのは`Dagq-Task`だけ（あるcommit以降に着地したtaskを数えるとき）なので、改名前のtrailerのcommitは読み飛ばされ、動作に支障はない。人が履歴からtask IDを引くときだけ、ある時点を境に名前が変わることを知っていればよい。
- ADR 0001・0002・0005・0015は`superseded`になり、`superseded_by: adr-0052`を持つ。
