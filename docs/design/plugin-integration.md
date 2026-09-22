---
id: design-plugin-integration
type: design
title: Claude Code and Codex plugin integration
status: current
created: 2026-09-21
updated: 2026-09-22
last_verified: 2026-09-22
scope: distribution
related:
  - adr-0005
  - adr-0006
  - adr-0010
---

# Claude Code and Codex plugin integration

runtimeとpluginを分離する。pluginはskill、hook、provider設定を配布し、SQLite・supervisor・cmux操作は`cmux-taskq`バイナリが担当する。

```text
cmux-taskq repository
  ├── runtime binary
  ├── .claude-plugin/marketplace.json  (Claude Code の marketplace としての自己申告)
  ├── plugins/claude-taskq
  └── plugins/codex-taskq (未着手)
```

Claude Code pluginは`.claude-plugin/plugin.json`と`skills/`を持ち、Codex pluginは`.codex-plugin/plugin.json`とskills/hooks/scriptsを持つ。共通の手順はshared skillから生成または同期する。pluginのskillからPATH上の`cmux-taskq`を呼び出し、見つからなければインストール方法を案内する。

platformごとのbinary release、checksum、version compatibilityはruntime側で管理する（[配布](#配布)）。pluginはruntimeのDB schemaを直接操作しない。

## 配布

binary releaseとchecksumはruntime側の責務なので（[ADR-0005](../adr/0005-binary-and-plugin-distribution.md)）、releaseはGitHub Actionsの`.github/workflows/release.yml`が作る。trigger は `v*` の tag push、runnerは`macos-14`、targetは`aarch64-apple-darwin`だけ（他のplatformは未対応）。

### tagとversionの一致規則

tagは`v<version>`で、`<version>`は`Cargo.toml`の`[package].version`と完全に一致する（例: version `0.1.0` には tag `v0.1.0`）。workflowはcheckoutの直後、buildより前のstepで`GITHUB_REF_NAME`から先頭の`v`を外したものと`Cargo.toml`の値を比べ、違えばそこでfailする。pluginの`.claude-plugin/plugin.json`のversionもクレートと同じ値にそろえる。

### artifact

Releaseには2つのファイルを添付する。

| ファイル | 中身 |
| --- | --- |
| `cmux-taskq-v<version>-aarch64-apple-darwin.tar.gz` | `cmux-taskq`バイナリ（`cargo build --release --locked --target aarch64-apple-darwin`）、`LICENSE`、`README.md`をアーカイブ直下に平置き |
| `SHA256SUMS` | 上のtar.gzの`shasum -a 256`出力1行 |

release notes は tag からの自動生成（`gh release create --generate-notes`）でよい。

### checksumの検証とインストール

同じdirectoryに両方を置いて検証する。

```sh
VERSION=0.1.0
gh release download "v$VERSION" --repo hisamekms/cmux-taskq
shasum -a 256 -c SHA256SUMS
tar -xzf "cmux-taskq-v$VERSION-aarch64-apple-darwin.tar.gz"
mkdir -p ~/.local/bin
install -m 755 cmux-taskq ~/.local/bin/cmux-taskq
```

`shasum -a 256 -c SHA256SUMS`が`OK`を出さないarchiveは展開しない。`~/.local/bin`をPATHに入れておくと、pluginのlauncherもsupervisorの起動も同じバイナリを解決する。

## Claude Code plugin (`plugins/claude-taskq`)

[011](../journal/011-claude-code-plugin.md)で実装。Claude Code 2.1.278のplugin形式（`.claude-plugin/plugin.json`、`skills/<name>/SKILL.md`、`bin/`）に従い、hook・agent・MCPは持たない。

```text
plugins/claude-taskq/
  .claude-plugin/plugin.json   name "claude-taskq"、version はクレートと同じ
  bin/taskq                    launcher（POSIX sh）
  skills/taskq/SKILL.md        バイナリと DB の解決、goal の登録と task への分解（goal add → add --goal → ready）、goal list / show / edit / close と set-goal、一覧・詳細・候補・status・doctor、結果の読み方、goal の close
  skills/taskq-maintain/SKILL.md maintainer の手順: 用語（supervisor / maintainer / worker）、up による起動、status の読み方（stale なら up をやり直す）、show による監視、trust / 権限 prompt への応答、レビューと integrate（receipt の follow_ups の報告を含む）、needs_session の resume、failed / interrupted の workspace close、down、log の場所
  skills/taskq-recover/SKILL.md doctor、recover、再試行
```

[ADR-0010](../adr/0010-maintainer-and-resident-supervisor.md)は、maintainerが使うCLIの手順（起動・監視・レビュー・着地・停止）をskill `taskq-maintain`に集め、AGENTS.mdにはrepository固有の注意だけを残すことを決めた。task 16で`taskq-run`を`taskq-maintain`に置き換え（`taskq-run`は削除）、AGENTS.mdからCLIのコマンド表を落としてcold startを`cmux-taskq up`の1行にした。maintainerの初期promptはruntimeが生成し（[supervisor-lifecycle](supervisor-lifecycle.md#maintainer-prompt)）、`taskq-maintain` skillで`status`と`doctor`を見るよう指示する。

### launcher

skillはすべて`${CLAUDE_PLUGIN_ROOT}/bin/taskq`を呼ぶ。launcherはバイナリを解決してcwdのまま`cmux-taskq <args>`を`exec`するだけで、DBのpathを計算せず、DBも開かない（[016](../journal/016-queue-per-repository.md)、[ADR-0006](../adr/0006-queue-per-repository.md)）。

- バイナリ: `CMUX_TASKQ_BIN`、なければPATHの`cmux-taskq`。どちらもなければ`{"error": ...}`をstderrに出し、GitHub Release（<https://github.com/hisamekms/cmux-taskq/releases>）の`cmux-taskq-v<plugin_version>-aarch64-apple-darwin.tar.gz`を`SHA256SUMS`で検証して`~/.local/bin`に置く手順と、開発時の`cargo build --locked`＋`CMUX_TASKQ_BIN`を案内する（CLI本体のエラー形式と同じ）。
- queue: バイナリがcwdのrepositoryから`$XDG_DATA_HOME/cmux-taskq/<hash>/queue.db`に解決する。`CMUX_TASKQ_DB`が設定されているときだけ`--db "$CMUX_TASKQ_DB"`を前置する。dirの作成と束縛は`init`が行う。
- `--resolve`: `cmux-taskq locate`のJSON（`db`、`db_exists`、`queue_dir`、`runs_dir`、`source`、`git_common_dir`）に`binary`、`binary_version`（`cmux-taskq --version`の数字部分）、`plugin_version`（launcherの隣の`.claude-plugin/plugin.json`をsedで読む）、`repo`（`git rev-parse --show-toplevel`、repository外は空文字）を加えた1つのobjectを返す。skillはこれをユーザーへの報告と、cmux workspaceへ渡す絶対pathの取得に使う。`--version` / `--help`はそのままバイナリに渡す。
- version不一致: `plugin_version`と`binary_version`のmajor.minorが違うとき、stdoutの解決結果はそのまま出したうえでstderrに`{"warning": ...}`を1行出し、exitは0のまま（解決自体は正しく、CLIの差だけが不明）。skillは止まらずユーザーに報告し、古い方の更新（pluginは`claude plugin update claude-taskq@cmux-taskq`、バイナリはRelease）を案内する。

### skillの契約

- 完了はStop hookやreceiptファイルの存在ではなく、`show`のrun `status`（`awaiting_integration` / `needs_session` / `integrated`）、`result_commit`、`last_error`、`validation_finished`イベントで判定する。
- 登録の標準手順は「課題を聞く → `goal add`で登録 → taskに分解して`add --goal`で登録 → `ready`」（[ADR-0009](../adr/0009-goal-groups-tasks.md)）。goalなしを許すのは一発task（typo修正、clippy警告の解消など、1 taskで終わり判断を揃える相手がいないもの）だけで、判断基準は「2つ目のtaskが存在する、または後のtaskがこのtaskの決定（名前・境界・形式）を知る必要があるならgoalを作る」。`goal add`はtitle、description、acceptance（全task着地後にmaintainerがgoalの達成を判定する基準）、constraints（命名・境界・やらないこと）、doc（repository内の参照文書のパス。workerはworktreeで読むのでcommit済みであること）を集め、`add`は`--goal`と`--context`（goalの記述で足りないときの背景と最初に読むもの）を足す。`goal list` / `goal show`はInspectの表にあり、`set-goal`はdraft / readyのtaskだけ、`goal edit`は`goal_updated`イベントに新旧を残しclaim済みのrunには届かない、と書く。
- goalのcloseはmaintainerがskillの手順で行い、runtimeは閉じない。`goal show`で全taskが`completed`（または`canceled`）になったら、各taskの`show`にある最後の`integration_receipt`イベント（着地前は`validation_finished`）のreceiptから`summary`と`follow_ups`を読み、goalのacceptanceに照らして未達があれば同じgoalに`add --goal`で後続taskを登録してから（閉じたgoalはtaskを拒否する）、なければ`goal close ID --verdict achieved`を呼ぶ。`abandoned`はdraft / readyのtaskをcancelしない（`in_progress`があるときだけ拒否する）ので、先にcancelする。`taskq-maintain` skillは、runのreceiptに`follow_ups`があれば`integrate`の前後でmaintainerがユーザーに報告し、`taskq` skillの`add --goal`で登録させる。
- supervisorの起動は`taskq-maintain` skillが`"$TASKQ" up --plugin-dir "$CLAUDE_PLUGIN_ROOT"`（必要なら`--parallel N`）をlauncher経由で呼ぶ（[021](../journal/021-maintainer-up-down.md)、[supervisor-lifecycle](supervisor-lifecycle.md#up--down)）。`up`がlaunchdのLaunchAgentとしてsupervisorを常駐させ、maintainer workspaceの有無を判定し、生きているsupervisorがあれば`reused`を返すので、skillは重複起動の判定もworkspaceの作成も自分では行わず、`cmux workspace create`で`supervise`を起動する手順も持たない。maintainer session（`CMUX_TASKQ_ROLE=maintainer`）の中から呼ぶと`maintainer`は`skipped`になり、これはerrorではないとskillに明記する。`status`のsupervisorが`stale`なら`up`を叩き直す（死んだ登録をpruneして起動し直す）。停止は`cmux-taskq down [--wait] [--force]`で、`--force`は実行中のrunを捨てるのでユーザーの同意が要る。supervisorのlogは`locate`の`log_dir`（`supervisor-<started_at>-<pid>.log`と`launchd.log`）。
- mainへの着地はruntimeの`integrate ID` / `integrate --next`が行う（rebase → 再検証 → squash、[ADR-0008](../adr/0008-merge-queue-squash-landing.md)）。skillはmaintainerのレビュー後にこれを呼び、`outcome`（`integrated` / `needs_session` / `failed` / `no_run_awaiting`）を読んで結果を伝える。`needs_session`のrunはmaintainerが`claude --resume <run-id>`でworktreeに開き直すセッションが解消し、解消後は着地がworktreeを消すので`/exit`で終えてから`integrate`し直す。runのworkspace名は`taskq <repo> <task-id> <run-id>`で、`failed` / `interrupted`のworkspaceはruntimeが閉じないのでmaintainerが`cmux workspace close`する。
- `recover`はバイナリが拒否条件を判定する。skillはプロセスをkillせず、`doctor`の`blockers`をユーザーに示す。

### marketplaceとinstall

repository rootの`.claude-plugin/marketplace.json`がこのrepository自身をmarketplaceにする（marketplace名`cmux-taskq`、`owner.name` `hisamekms`、`plugins`は`claude-taskq`の1件で`source`はrepository相対の`./plugins/claude-taskq`）。ユーザーの導線は2行。

```sh
claude plugin marketplace add hisamekms/cmux-taskq
claude plugin install claude-taskq@cmux-taskq
```

`add`はGitHubのrepositoryをcloneし、`install`はそのcloneの`./plugins/claude-taskq`からuser scopeに入れる。更新は`claude plugin marketplace update cmux-taskq`と`claude plugin update claude-taskq@cmux-taskq`。pluginはskillだけなのでinstallに`-y`を要する宣言commandはなく、runtimeバイナリは同梱しない（Releaseから別に入れる。launcherのエラー文がその手順を持つ）。

### 読み込みと検証

- 検証: `claude plugin validate plugins/claude-taskq`と`claude plugin validate .claude-plugin/marketplace.json`（`--strict`も通る）、inventory: `claude --plugin-dir plugins/claude-taskq plugin details claude-taskq`。
- 開発中の読み込み: `claude --plugin-dir /path/to/cmux-taskq/plugins/claude-taskq`（そのsessionのみ）。supervisorがworkerに渡すのもこの形（`up --plugin-dir`）。
- marketplace経由のinstallは、使い捨ての`HOME` / `CLAUDE_CONFIG_DIR`でローカルpathを`marketplace add`して`install`し、`plugin list`と`plugin details`でskill 3件が載ることを確認する（task 29、Claude Code 2.1.278で確認）。
- `tests/plugin.rs`がmanifest（name、versionの一致）、marketplace manifest（marketplace名、pluginのnameとrepository相対の`source`がpluginのdirectoryを指すこと）、launcherのversion比較（`--version`と`locate`だけ答えるfake binaryを`CMUX_TASKQ_BIN`にして、major.minorが同じならstderrが空、1 minor違えばstderrに`{"warning": ...}`が出てexit 0）、skill一覧（`taskq` / `taskq-maintain` / `taskq-recover`）、frontmatter（先頭行`---`、`name`がdirectory名、`description`）、launcherの解決（`XDG_DATA_HOME`配下、worktreeからの共有、`CMUX_TASKQ_DB`の優先、`binary_version`と`plugin_version`）・エラー（Release URL、tarball名、`SHA256SUMS`、`~/.local/bin`、`cargo build --locked`を含むこと）・`init`・登録・`show`を実バイナリで確認する。テストは`XDG_DATA_HOME`を一時dirに向け、開発者の実queueに触れない。skillに書いたコマンド列のうち自動テストにしないもの（goal系の`goal add` → `add --goal` → `ready` → `goal show` → `goal close`、runtime系の`up` → `status` → `down`）は、skillを変えたtaskのrun sessionが`cargo build --locked`したバイナリと使い捨てrepository・使い捨てqueue（`--db`）で実行し、その実行ログをreceiptのevidenceに残す（task 13、task 16）。
