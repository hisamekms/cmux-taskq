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
  ├── plugins/claude-taskq
  └── plugins/codex-taskq (未着手)
```

Claude Code pluginは`.claude-plugin/plugin.json`と`skills/`を持ち、Codex pluginは`.codex-plugin/plugin.json`とskills/hooks/scriptsを持つ。共通の手順はshared skillから生成または同期する。pluginのskillからPATH上の`cmux-taskq`を呼び出し、見つからなければインストール方法を案内する。

platformごとのbinary release、checksum、version compatibilityをruntime側で管理する。pluginはruntimeのDB schemaを直接操作しない。

## Claude Code plugin (`plugins/claude-taskq`)

[011](../journal/011-claude-code-plugin.md)で実装。Claude Code 2.1.278のplugin形式（`.claude-plugin/plugin.json`、`skills/<name>/SKILL.md`、`bin/`）に従い、hook・agent・MCPは持たない。

```text
plugins/claude-taskq/
  .claude-plugin/plugin.json   name "claude-taskq"、version はクレートと同じ
  bin/taskq                    launcher（POSIX sh）
  skills/taskq/SKILL.md        バイナリと DB の解決、goal の登録と task への分解（goal add → add --goal → ready）、goal list / show / edit / close と set-goal、一覧・詳細・候補・status・doctor、結果の読み方、goal の close
  skills/taskq-run/SKILL.md    supervise の起動（専用 cmux workspace）、show による監視、完了判定、integrate（receipt の follow_ups の報告を含む）
  skills/taskq-recover/SKILL.md doctor、recover、再試行
```

[ADR-0010](../adr/0010-maintainer-and-resident-supervisor.md)は、maintainerが使うCLIの手順（登録・監視・レビュー・着地・復旧）をskill `taskq-maintain`に集め、AGENTS.mdにはrepository固有の注意だけを残すことを決めた。maintainerの初期promptはruntimeが生成する。いずれもT3で実装予定で、現在のskill構成は上記のまま。

### launcher

skillはすべて`${CLAUDE_PLUGIN_ROOT}/bin/taskq`を呼ぶ。launcherはバイナリを解決してcwdのまま`cmux-taskq <args>`を`exec`するだけで、DBのpathを計算せず、DBも開かない（[016](../journal/016-queue-per-repository.md)、[ADR-0006](../adr/0006-queue-per-repository.md)）。

- バイナリ: `CMUX_TASKQ_BIN`、なければPATHの`cmux-taskq`。どちらもなければ`{"error": ...}`をstderrに出し、`cargo build --locked`と`CMUX_TASKQ_BIN`の設定を案内する（CLI本体のエラー形式と同じ）。
- queue: バイナリがcwdのrepositoryから`$XDG_DATA_HOME/cmux-taskq/<hash>/queue.db`に解決する。`CMUX_TASKQ_DB`が設定されているときだけ`--db "$CMUX_TASKQ_DB"`を前置する。dirの作成と束縛は`init`が行う。
- `--resolve`: `cmux-taskq locate`のJSON（`db`、`db_exists`、`queue_dir`、`runs_dir`、`source`、`git_common_dir`）に`binary`、`version`、`repo`（`git rev-parse --show-toplevel`、repository外は空文字）を加えた1つのobjectを返す。skillはこれをユーザーへの報告と、cmux workspaceへ渡す絶対pathの取得に使う。`--version` / `--help`はそのままバイナリに渡す。

### skillの契約

- 完了はStop hookやreceiptファイルの存在ではなく、`show`のrun `status`（`awaiting_integration` / `needs_session` / `integrated`）、`result_commit`、`last_error`、`validation_finished`イベントで判定する。
- 登録の標準手順は「課題を聞く → `goal add`で登録 → taskに分解して`add --goal`で登録 → `ready`」（[ADR-0009](../adr/0009-goal-groups-tasks.md)）。goalなしを許すのは一発task（typo修正、clippy警告の解消など、1 taskで終わり判断を揃える相手がいないもの）だけで、判断基準は「2つ目のtaskが存在する、または後のtaskがこのtaskの決定（名前・境界・形式）を知る必要があるならgoalを作る」。`goal add`はtitle、description、acceptance（全task着地後にmaintainerがgoalの達成を判定する基準）、constraints（命名・境界・やらないこと）、doc（repository内の参照文書のパス。workerはworktreeで読むのでcommit済みであること）を集め、`add`は`--goal`と`--context`（goalの記述で足りないときの背景と最初に読むもの）を足す。`goal list` / `goal show`はInspectの表にあり、`set-goal`はdraft / readyのtaskだけ、`goal edit`は`goal_updated`イベントに新旧を残しclaim済みのrunには届かない、と書く。
- goalのcloseはmaintainerがskillの手順で行い、runtimeは閉じない。`goal show`で全taskが`completed`（または`canceled`）になったら、各taskの`show`にある最後の`integration_receipt`イベント（着地前は`validation_finished`）のreceiptから`summary`と`follow_ups`を読み、goalのacceptanceに照らして未達があれば同じgoalに`add --goal`で後続taskを登録してから（閉じたgoalはtaskを拒否する）、なければ`goal close ID --verdict achieved`を呼ぶ。`abandoned`はdraft / readyのtaskをcancelしない（`in_progress`があるときだけ拒否する）ので、先にcancelする。`taskq-run` skillは、runのreceiptに`follow_ups`があれば`integrate`の前後でmaintainerがユーザーに報告し、`taskq` skillの`add --goal`で登録させる。
- supervisorの起動はskillが`cmux-taskq up [--parallel N] [--plugin-dir PATH]`を呼ぶ（[021](../journal/021-maintainer-up-down.md)、[supervisor-lifecycle](supervisor-lifecycle.md#up--down)）。`up`がlaunchdのLaunchAgentとしてsupervisorを常駐させ、maintainer workspaceの有無を判定し、生きているsupervisorがあれば`reused`を返すので、skillは重複起動の判定もworkspaceの作成も自分では行わない。maintainer session（`CMUX_TASKQ_ROLE=maintainer`）の中から呼ぶと`maintainer`は`skipped`になる。停止は`cmux-taskq down [--wait] [--force]`。現在の`taskq-run` skillはまだ`cmux workspace create --command "<bin> supervise"`で起動する旧手順を書いており、T3（task 16、`taskq-maintain` skillとAGENTS.mdの縮小）で`up` / `down`に書き換える予定。
- mainへの着地はruntimeの`integrate ID` / `integrate --next`が行う（rebase → 再検証 → squash、[ADR-0008](../adr/0008-merge-queue-squash-landing.md)）。skillはmaintainerのレビュー後にこれを呼び、`outcome`（`integrated` / `needs_session` / `failed` / `no_run_awaiting`）を読んで結果を伝える。`needs_session`のrunはmaintainerが`claude --resume <run-id>`でworktreeに開き直すセッションが解消する。
- `recover`はバイナリが拒否条件を判定する。skillはプロセスをkillせず、`doctor`の`blockers`をユーザーに示す。

### 読み込みと検証

- 検証: `claude plugin validate plugins/claude-taskq`、inventory: `claude --plugin-dir plugins/claude-taskq plugin details claude-taskq`。
- 利用: `claude --plugin-dir /path/to/cmux-taskq/plugins/claude-taskq`（そのsessionのみ）。恒久化するにはsettingsのmarketplaceにローカルpathを登録して`claude plugin install claude-taskq@<marketplace>`する（未検証）。
- `tests/plugin.rs`がmanifest（name、versionの一致）、frontmatter（先頭行`---`、`name`がdirectory名、`description`）、launcherの解決（`XDG_DATA_HOME`配下、worktreeからの共有、`CMUX_TASKQ_DB`の優先）・エラー・`init`・登録・`show`を実バイナリで確認する。テストは`XDG_DATA_HOME`を一時dirに向け、開発者の実queueに触れない。skillに書いたgoal系のコマンド列（`goal add` → `add --goal` → `ready` → `goal show` → `goal close`）は自動テストにせず、skillを変えたtaskのrun sessionが`cargo build --locked`したバイナリと使い捨てqueue（`--db`）で実行し、その実行ログをreceiptのevidenceに残す（task 13）。
