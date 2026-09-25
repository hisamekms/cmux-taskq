---
id: adr-0015
type: adr
title: cmux-taskqをdagqに改名する
status: accepted
created: 2026-09-22
updated: 2026-09-22
accepted_on: 2026-09-22
owners:
  - hisamekms
tags:
  - naming
  - distribution
  - repository
related:
  - adr-0005
  - adr-0006
---

# ADR-0015: cmux-taskqをdagqに改名する

## Context

`cmux-taskq`という名前には2つの問題がある。

1つ目は、cmuxへの依存を名前に固定していること。cmuxはworkspaceのbackend（[ADR-0002](0002-cmux-first.md)）であって、この runtime の本質ではない。本質は「依存関係のDAGを持つ開発タスクを、Git worktreeとagent sessionに割り当ててキューで流す」ことで、backendは将来差し替えうる。

2つ目は、`taskq` / `tasq`が2026年時点で「coding agent向けtask queue」の名前として多発していること。調査（2026-09-22、maintainer）では次が見つかった。

- `vmihailenco/taskq`（Go、★1.3k）。crates.ioの`taskq`も2026-07に取得済みで、npm・PyPIも取得済み。
- `version-1/tasq`と`gwendall/tasq`が、いずれもcoding agent向けのtask queueとして2026年に活動中。npm・PyPIの`tasq`も取得済み。

そこで候補を約40件挙げ、crates.io / npm / PyPI / Homebrew / GitHubで調べた。落ちた候補には、`runq` / `qmux` / `worq` / `wq` / `workq`（★数百〜千の既存ツールがある）、`grove` / `copse` / `coppice` / `orchard` / `thicket`（crates.io取得済み）がある。次点は`hatchq`だったが、task orchestration platformのHatchetと音が近いので外した。

`dagq`は全レジストリ（crates.io / npm / PyPI / Homebrew）が空きで、GitHubの同名repositoryも★0が2件だけだった。かつ「依存DAGをキューで流す」というこの runtime の本質をそのまま言う。

## Decision

repository・crate・バイナリ・plugin・skill・環境変数・データディレクトリ・branchとrefの接頭辞・launchd label・cmux workspace名・git trailer・Releaseのarchive名をすべて`dagq`に揃える。対応表を次に固定し、goal 4の全taskはこの表に従う。

| 対象 | 旧 | 新 |
| --- | --- | --- |
| crate / package / バイナリ | `cmux-taskq` | `dagq`（`cargo build`の成果物は`target/*/dagq`、`dagq --version`は`dagq 0.2.0`。testsの`env!("CARGO_BIN_EXE_cmux-taskq")`は`env!("CARGO_BIN_EXE_dagq")`） |
| GitHub repository | `hisamekms/cmux-taskq` | `hisamekms/dagq`（`https://github.com/hisamekms/dagq`、Releaseは`https://github.com/hisamekms/dagq/releases`） |
| Releaseのarchive | `cmux-taskq-v<ver>-aarch64-apple-darwin.tar.gz` | `dagq-v<ver>-aarch64-apple-darwin.tar.gz` |
| pluginディレクトリ | `plugins/claude-taskq` | `plugins/claude-dagq` |
| plugin.jsonの`name` / `displayName` | `claude-taskq` / cmux-taskq | `claude-dagq` / `dagq` |
| marketplace（`.claude-plugin/marketplace.json`）の`name` | `cmux-taskq` | `dagq` |
| pluginのインストール / 更新 | `claude plugin marketplace add hisamekms/cmux-taskq`、`claude plugin install claude-taskq@cmux-taskq` | `claude plugin marketplace add hisamekms/dagq`、`claude plugin install claude-dagq@dagq`、更新は`claude plugin update claude-dagq@dagq` |
| skill（ディレクトリ名とSKILL.mdの`name`） | `taskq` / `taskq-maintain` / `taskq-recover` | `dagq` / `dagq-maintain` / `dagq-recover` |
| launcher | `bin/taskq` | `bin/dagq` |
| 環境変数 | `CMUX_TASKQ_BIN` / `CMUX_TASKQ_DB` / `CMUX_TASKQ_ROLE` / `CMUX_TASKQ_QUEUE` / `CMUX_TASKQ_E2E_CMUX` | `DAGQ_BIN` / `DAGQ_DB` / `DAGQ_ROLE` / `DAGQ_QUEUE` / `DAGQ_E2E_CMUX` |
| データディレクトリ（`DATA_DIR_NAME`） | `$XDG_DATA_HOME/cmux-taskq/<hash>/` | `$XDG_DATA_HOME/dagq/<hash>/`（hashの計算と`queue.db`・`runs`・`logs`・`repository`の名前は変えない） |
| launchd label（`LAUNCH_AGENT_PREFIX`とplist名） | `com.cmux-taskq.<hash>` | `com.dagq.<hash>` |
| run branch | `taskq/<run-id>` | `dagq/<run-id>` |
| 履歴ref | `refs/taskq/runs/<run-id>` | `refs/dagq/runs/<run-id>` |
| squash commitのtrailer | `Taskq-Task` / `Taskq-Run` | `Dagq-Task` / `Dagq-Run` |
| cmux workspace名 | `taskq <repo> maintainer` / `taskq <repo> supervisor` / `taskq <repo> <task> <run>` | `dagq <repo> maintainer` / `dagq <repo> supervisor` / `dagq <repo> <task> <run>` |
| version（`Cargo.toml`と`plugins/claude-dagq/.claude-plugin/plugin.json`の両方） | `0.1.0` | `0.2.0` |

あわせて次を決める。

- エラー文・ログ・コメント・ドキュメントの「cmux-taskq」は「dagq」に、「cmux-taskq queue」は「dagq queue」に置き換える。説明文の「cmuxとGit worktreeで動く」は事実なので残す。
- SQLiteのschemaとAPPLICATION_IDは変えない。migrationを足さない。バイナリ名と環境変数が変わるbreaking changeなのでversionだけ0.2.0に上げる。
- 互換shimは作らない。旧環境変数の読み取り、旧データディレクトリの探索、旧branch接頭辞の認識のいずれも実装しない。利用者はこのrepositoryだけで、切り替えはmaintainerが手で行う。
- `docs/journal/`と既存ADR（0001〜0013）は凍結なので旧名のまま触らない。旧名を書くのはこのADR-0015だけ。

## Alternatives

- `taskq`のまま押し通す: 上のとおり名前が多発していて、crates.io・npm・PyPIが取得済みなので配布名を確保できない。
- `cmux-taskq`のまま、cmux依存だけ将来外す: 名前が実態と食い違い続ける。改名するなら利用者がこのrepositoryだけの今が最も安い。
- `hatchq`: 全レジストリ空きだったが、task orchestration platformのHatchetと音が近い。
- 互換shim（旧バイナリ名のwrapper、旧環境変数のfallback、旧データディレクトリの自動移行）を作る: 利用者が1人で切り替えが一度きりなので、shimの保守コストが手作業のコストを上回る。

## Consequences

### 1. 切り替え手順（maintainerとユーザーの手作業、この順番で）

goal 4の全taskがmainに着地してから行う。

1. 実行中のrunがない状態で`cmux-taskq down`。supervisorとLaunchAgentを止める。
2. ユーザーがGitHub repositoryを`hisamekms/cmux-taskq`から`hisamekms/dagq`にrenameする。ローカルのghqディレクトリを`.../github.com/hisamekms/dagq`へ移し、`git remote set-url origin`を新しいURLに付け替える。
3. queueのディレクトリ名の`<hash>`はcanonicalizeしたGit common directoryのpathから決まる（[ADR-0006](0006-queue-per-repository.md)）。したがってghqのディレクトリを動かすとhashが変わり、`DATA_DIR_NAME`の変更と合わせて**queueのディレクトリは必ず変わる**。新しいバイナリで`dagq locate`を実行して新しいqueueディレクトリを確かめ（`locate`はDBを開かないので、手順6で固定バイナリを置き換える前にbuildした`target/release/dagq`で実行してよい）、旧`$XDG_DATA_HOME/cmux-taskq/<旧hash>/`の`queue.db`・`runs`・`logs`・`repository`をそこへ移す。`repository`ファイルの中身は新しいcommon directoryのpathに書き換える。runtimeは自動移行しない。
4. 移したqueueの`queue_repository`の束縛を新しいcommon directoryに更新する。この束縛は`init`でしか書かれず（`bind_repository`は「rebindingは暗黙に行わない」）、`assert_repository`がrepositoryから解決した全コマンドの冒頭で検査するので、更新しないと移動後の`dagq`は`queue is bound to another Git repository: <旧path>`で全部落ちる。移行のためにここだけDBを直接触る（AGENTS.mdの「DBは手で直さない」の唯一の例外として、この手順でだけ認める）。

   ```sh
   sqlite3 "$(dagq locate | jq -r .db)" \
     "UPDATE queue_repository SET git_common_dir='<新しいcommon directoryのpath>' WHERE singleton=1;"
   ```

   rebind用のサブコマンドを足すなら別taskにする。この切り替えは一度きりなので、ADR-0015ではruntimeを変えない。
5. 旧LaunchAgent`com.cmux-taskq.<旧hash>`のplistを`~/Library/LaunchAgents`から外す（`launchctl bootout`してからplistを削除）。
6. `~/.local/bin/cmux-taskq`を`~/.local/bin/dagq`に置き換える。固定バイナリの更新はAGENTS.mdの規則どおり、maintainerがユーザーに報告してから行う。
7. pluginを入れ直す。`claude plugin marketplace add hisamekms/dagq`と`claude plugin install claude-dagq@dagq`。旧`claude-taskq@cmux-taskq`とその marketplace は外す。
8. `dagq up --plugin-dir plugins/claude-dagq`でsupervisorとmaintainerを復帰させる。
9. ユーザーがtag`v0.2.0`をpushし、Releaseに`dagq-v0.2.0-aarch64-apple-darwin.tar.gz`が付くことを確認する。

### 2. 旧runのbranchとrefは移さない

既存の`taskq/<run-id>` branchと`refs/taskq/runs/<run-id>`は、runtimeが移動も削除もしない。新しいrunだけが`dagq/`接頭辞になる。旧refが要るときはmaintainerが`git for-each-ref refs/taskq/runs`で列挙して手で移す（`git update-ref`で新しい名前を作って旧名を消す）。放置しても新しいruntimeは旧接頭辞を見ないので支障はない。

### 3. mainの履歴に残るtrailerはそのまま

着地済みのsquash commitの`Taskq-Task` / `Taskq-Run` trailerは書き換えない。runtimeはtrailerを書くだけで読まないので、新旧が混在しても動作に支障はない。履歴からtask IDを引くときだけ、ある時点を境に名前が変わることを知っていればよい。

### 4. maintainerのmemoryディレクトリが移る

Claude Codeのmemoryディレクトリはrepositoryのpathから決まるので、ghqのディレクトリを動かすとmaintainerのmemoryは新しいpath側のディレクトリを見るようになる。旧`.../-hisamekms-cmux-taskq/memory/`の内容はユーザーが手で新しい側へ移すか、空から始める。

### 5. 残る懸念

- 検索でDAQ（data acquisition）と混ざる可能性がある。README冒頭で「dependency DAG queue」と明示して緩和する。
- `justinj/dagq`（★0のRust実験repository、`Cargo.toml`の`name`が`dagq`、2026-07で停止）がcrates.ioに先にpublishする可能性がある。crates.ioの`dagq`を先に確保して対処する（実行はユーザー）。
