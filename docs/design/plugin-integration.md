---
id: design-plugin-integration
type: design
title: Claude Code and Codex plugin integration
status: current
created: 2026-09-21
updated: 2026-09-23
last_verified: 2026-09-23
scope: distribution
related:
  - adr-0005
  - adr-0006
  - adr-0010
  - adr-0016
  - adr-0018
  - adr-0019
  - adr-0021
---

# Claude Code and Codex plugin integration

runtimeとpluginを分離する。pluginはskill、hook、provider設定を配布し、SQLite・supervisor・cmux操作は`dagq`バイナリが担当する。

```text
dagq repository
  ├── runtime binary
  ├── .claude-plugin/marketplace.json  (Claude Code の marketplace としての自己申告)
  ├── plugins/claude-dagq
  └── plugins/codex-dagq (未着手)
```

Claude Code pluginは`.claude-plugin/plugin.json`と`skills/`を持ち、Codex pluginは`.codex-plugin/plugin.json`とskills/hooks/scriptsを持つ。共通の手順はshared skillから生成または同期する。pluginのskillからPATH上の`dagq`を呼び出し、見つからなければインストール方法を案内する。

platformごとのbinary release、checksum、version compatibilityはruntime側で管理する（[配布](#配布)）。pluginはruntimeのDB schemaを直接操作しない。

## 配布

binary releaseとchecksumはruntime側の責務なので（[ADR-0005](../adr/0005-binary-and-plugin-distribution.md)）、releaseはGitHub Actionsの`.github/workflows/release.yml`が作る。trigger は `v*` の tag push、runnerは`macos-14`、targetは`aarch64-apple-darwin`だけ（他のplatformは未対応）。

### tagとversionの一致規則

tagは`v<version>`で、`<version>`は`Cargo.toml`の`[package].version`と完全に一致する（例: version `0.2.0` には tag `v0.2.0`）。workflowはcheckoutの直後、buildより前のstepで`GITHUB_REF_NAME`から先頭の`v`を外したものと`Cargo.toml`の値を比べ、違えばそこでfailする。pluginの`.claude-plugin/plugin.json`のversionもクレートと同じ値にそろえる。

### artifact

Releaseには2つのファイルを添付する。

| ファイル | 中身 |
| --- | --- |
| `dagq-v<version>-aarch64-apple-darwin.tar.gz` | `dagq`バイナリ（`cargo build --release --locked --target aarch64-apple-darwin`）、`LICENSE`、`README.md`をアーカイブ直下に平置き |
| `SHA256SUMS` | 上のtar.gzの`shasum -a 256`出力1行 |

release notes は tag からの自動生成（`gh release create --generate-notes`）でよい。

### checksumの検証とインストール

同じdirectoryに両方を置いて検証する。

```sh
VERSION=0.2.0
gh release download "v$VERSION" --repo hisamekms/dagq
shasum -a 256 -c SHA256SUMS
tar -xzf "dagq-v$VERSION-aarch64-apple-darwin.tar.gz"
mkdir -p ~/.local/bin
install -m 755 dagq ~/.local/bin/dagq
```

`shasum -a 256 -c SHA256SUMS`が`OK`を出さないarchiveは展開しない。`~/.local/bin`をPATHに入れておくと、pluginのlauncherもsupervisorの起動も同じバイナリを解決する。

## Claude Code plugin (`plugins/claude-dagq`)

[011](../journal/011-claude-code-plugin.md)で実装。Claude Code 2.1.278のplugin形式（`.claude-plugin/plugin.json`、`skills/<name>/SKILL.md`、`hooks/hooks.json`、`bin/`）に従う。hookは`SessionStart`の1本だけで（[ADR-0016](../adr/0016-maintainer-notification-and-compact-output.md)がそれまでの「hookを持たない」を改めた）、agent・MCPは持たない。

```text
plugins/claude-dagq/
  .claude-plugin/plugin.json      name "claude-dagq"、version はクレートと同じ
  bin/dagq                        launcher（POSIX sh）
  hooks/hooks.json                SessionStart（matcher compact|clear）で session-start.sh を呼ぶ
  hooks/session-start.sh          DAGQ_ROLE=maintainer の時だけ dagq status を stdout に出す
  skills/dagq/                    バイナリと DB の解決、goal の登録と task への分解、ready、参照コマンドの要点、結果の読み方
    reference/locate.md             install、version 警告、db_exists false と rebind
    reference/inspect.md            参照コマンドの表と各フィールド、list のページング、task / run の状態、graph、goal edit / set-goal
    reference/goal-close.md         goal の close の手順
  skills/dagq-maintain/           maintainer のループ: 役割、up、status の読み方と attention の振り分け、watch の background 実行、権限の線引き、down
    reference/up-down.md            up / down の出力、別 version の入替、cmux の接続拒否と --in-cmux、in_cmux の down、log
    reference/status.md             status / watch / events / show のフィールド、run の状態一覧
  skills/dagq-land/               review ID → subagent に review.md の path を渡して結論だけ受け取る → ユーザーの承認 → integrate（着地後に origin へ push し、follow_ups を draft task として登録する）、draft を ready にするかをユーザーに聞く
    reference/integrate.md          review.md の中身、integrate の再検証と skip、outcome、--next
  skills/dagq-session/            run の session への操作: trust / permission prompt と質問への応答、exit_request_timed_out の /exit、failed / interrupted の workspace close、needs_session の resume
    reference/cmux.md               read-screen / send-key / send / workspace close の使い方
  skills/dagq-recover/SKILL.md    doctor、recover、再試行
```

各`SKILL.md`は手順だけを書き8 KB以下に収め（`tests/plugin.rs`が確かめる）、出力フィールドや状態の一覧は各skillの`reference/`に置いて本文から「必要な時に読む」と指す。skillは呼ぶたびに読み込まれcompaction後にも読み直されるので、読み込み単位を小さくする。descriptionはtriggerが重ならないように書き分ける: `dagq`は登録と参照、`dagq-maintain`は起動・停止・`status`・`watch`と「今何が要るか」、`dagq-land`はレビューと着地（`review and integrate`）、`dagq-session`はrunのsessionへの操作（`resume session`・`send /exit`・`inspect and close workspace`）、`dagq-recover`は止まったleaseの復旧。

[ADR-0010](../adr/0010-maintainer-and-resident-supervisor.md)は、maintainerが使うCLIの手順（起動・監視・レビュー・着地・停止）をskillに集め、AGENTS.mdにはrepository固有の注意だけを残すことを決めた。task 16で`taskq-run`を廃し、その内容を`dagq-maintain`にあたるskillへ移し、task 65（[ADR-0016](../adr/0016-maintainer-notification-and-compact-output.md)の(6)(7)(8)）でそれを`dagq-maintain` / `dagq-land` / `dagq-session`に分けた。maintainerの初期promptはruntimeが生成し（[supervisor-lifecycle](supervisor-lifecycle.md#maintainer-prompt)）、`status`から始めて`dagq-maintain` skillに従い`watch`をbackgroundで回すよう指示する。

### 起き直しhook（ADR-0016）

maintainerは状態を持たない使い捨てのsessionで、compactionと`/clear`からの起き直しをhookが自動化する。

- `hooks/hooks.json`は`SessionStart`に matcher `compact|clear` の1グループを置き、`${CLAUDE_PLUGIN_ROOT}/hooks/session-start.sh`を呼ぶ。`startup`は`maintainer_prompt`が担い、`resume`は元のcontextを持つので含めない。
- `session-start.sh`（成功時のstdoutは`status`のJSONだけで、stderrは混ぜない）は`DAGQ_ROLE`が`maintainer`でなければ何も出力せずexit 0する。workerはruntimeの`--settings`で起動されpluginを読まないが、読んだとしてもroleが違うので影響しない。
- maintainerなら`bin/dagq status`（supervisor、未完了run、attention、cursor）をそのままstdoutに出し、Claude Codeがcontextに入れる。`up`が渡す`DAGQ_QUEUE`があり`DAGQ_DB`が無ければ`DAGQ_DB`にしてlauncherに渡すので、cwdがrepositoryの外でもmaintainerのqueueを読む。
- バイナリが見つからない（`DAGQ_BIN`が実行可能でない、PATHに`dagq`が無い）時や`status`が失敗した時も1行だけ理由（`dagq status unavailable: …` / `dagq status failed: …`）を出してexit 0し、session開始を止めない。
- maintainerはhookの出力を起点に`dagq-maintain`の手順（attentionの報告、`watch --after <cursor>`をbackgroundで再開）へ戻る。`watch`の結果で`integrate`は呼ばず、着地はユーザーの承認後に`dagq-land`で行う。

### launcher

skillはすべて`${CLAUDE_PLUGIN_ROOT}/bin/dagq`を呼ぶ。launcherはバイナリを解決してcwdのまま`dagq <args>`を`exec`するだけで、DBのpathを計算せず、DBも開かない（[016](../journal/016-queue-per-repository.md)、[ADR-0006](../adr/0006-queue-per-repository.md)）。

- バイナリ: `DAGQ_BIN`、なければPATHの`dagq`。どちらもなければ`{"error": ...}`をstderrに出し、GitHub Release（<https://github.com/hisamekms/dagq/releases>）の`dagq-v<plugin_version>-aarch64-apple-darwin.tar.gz`を`SHA256SUMS`で検証して`~/.local/bin`に置く手順と、開発時の`cargo build --locked`＋`DAGQ_BIN`を案内する（CLI本体のエラー形式と同じ）。
- queue: バイナリがcwdのrepositoryから`$XDG_DATA_HOME/dagq/<hash>/queue.db`に解決する。`DAGQ_DB`が設定されているときだけ`--db "$DAGQ_DB"`を前置する。dirの作成と束縛は`init`が行う。
- `--resolve`: `dagq locate`のJSON（`db`、`db_exists`、`queue_dir`、`runs_dir`、`source`、`git_common_dir`）に`binary`、`binary_version`（`dagq --version`の数字部分）、`plugin_version`（launcherの隣の`.claude-plugin/plugin.json`をsedで読む）、`repo`（`git rev-parse --show-toplevel`、repository外は空文字）を加えた1つのobjectを返す。skillはこれをユーザーへの報告と、cmux workspaceへ渡す絶対pathの取得に使う。`--version` / `--help`はそのままバイナリに渡す。
- version不一致: `plugin_version`と`binary_version`のmajor.minorが違うとき、stdoutの解決結果はそのまま出したうえでstderrに`{"warning": ...}`を1行出し、exitは0のまま（解決自体は正しく、CLIの差だけが不明）。skillは止まらずユーザーに報告し、古い方の更新（pluginは`claude plugin update claude-dagq@dagq`、バイナリはRelease）を案内する。

### skillの契約

- 完了はStop hookやreceiptファイルの存在ではなく、`show`のrun `status`（`awaiting_integration` / `needs_session` / `integrated`）、`result_commit`、`last_error`、`validation_finished`イベントで判定する。
- 登録の標準手順は「課題を聞く → `goal add`で登録 → taskに分解して`add --goal`で登録 → `ready`」（[ADR-0009](../adr/0009-goal-groups-tasks.md)）。goalなしを許すのは一発task（typo修正、clippy警告の解消など、1 taskで終わり判断を揃える相手がいないもの）だけで、判断基準は「2つ目のtaskが存在する、または後のtaskがこのtaskの決定（名前・境界・形式）を知る必要があるならgoalを作る」。`goal add`はtitle、description、acceptance（全task着地後にmaintainerがgoalの達成を判定する基準）、constraints（命名・境界・やらないこと）、doc（repository内の参照文書のパス。workerはworktreeで読むのでcommit済みであること）を集め、`add`は`--goal`と`--context`（goalの記述で足りないときの背景と最初に読むもの）を足す。`goal list` / `goal show`はInspectの要点と`reference/inspect.md`の表にあり、`set-goal`はdraft / readyのtaskだけ、`goal edit`は`goal_updated`イベントに新旧を残しclaim済みのrunには届かない、と書く。
- goalのcloseはmaintainerがskillの手順で行い、runtimeは閉じない。`goal show`で全taskが`completed`（または`canceled`）になったら、各taskの`show --full`にある最後の`integration_receipt`イベント（着地前は`validation_finished`）のreceiptから`summary`と`follow_ups`を読み、goalのacceptanceに照らして未達があれば同じgoalに`add --goal`で後続taskを登録してから（閉じたgoalはtaskを拒否する）、なければ`goal close ID --verdict achieved`を呼ぶ。`abandoned`はdraft / readyのtaskをcancelしない（`in_progress`があるときだけ拒否する）ので、先にcancelする。receiptの`follow_ups`は`integrate`が着地後に同じgoalのdraft taskとして登録する（[ADR-0019](../adr/0019-move-routine-maintainer-work-into-the-runtime.md)の決定4）。`dagq-land` skillは登録されたtaskを報告し、`ready`にするかをユーザーに聞く。goalのcloseの手順（`dagq` skillの`reference/goal-close.md`）はまずgoalのdraft taskの有無を見て、draftが残っていれば`ready`か`cancel`かをユーザーに聞く。
- supervisorの起動は`dagq-maintain` skillが`"$DAGQ" up --plugin-dir "$CLAUDE_PLUGIN_ROOT"`（必要なら`--parallel N`）をlauncher経由で呼ぶ（[021](../journal/021-maintainer-up-down.md)、[supervisor-lifecycle](supervisor-lifecycle.md#up--down)）。`up`がlaunchdのLaunchAgentとしてsupervisorを常駐させ、maintainer workspaceの有無を判定し、生きているsupervisorがあれば`reused`を返すので、skillは重複起動の判定もworkspaceの作成も自分では行わず、`cmux workspace create`で`supervise`を起動する手順も持たない。maintainer session（`DAGQ_ROLE=maintainer`）の中から呼ぶと`maintainer`は`skipped`になり、これはerrorではないとskillに明記する。`status`のsupervisorが`stale`なら`up`を叩き直す（死んだ登録をpruneして起動し直す）。停止は`dagq down [--wait] [--force]`で、`--force`は実行中のrunを捨てるのでユーザーの同意が要る。supervisorのlogは`locate`の`log_dir`（`supervisor-<started_at>-<pid>.log`と`launchd.log`）。
- mainへの着地はruntimeの`integrate ID` / `integrate --next`が行う（rebase → 再検証 → squash、[ADR-0008](../adr/0008-merge-queue-squash-landing.md)）。`dagq-land` skillは`review ID`の`review.md`をsubagentにレビューさせて結論だけ受け取り、ユーザーの承認後にこれを呼び（`watch`の結果やイベントの副作用としては呼ばない）、`outcome`（`integrated` / `needs_session` / `failed` / `no_run_awaiting`）を読んで結果を伝える。`needs_session`のrunは`dagq-session` skillに従いmaintainerが`claude --resume <run-id>`でworktreeに開き直すセッション（workspace名は`[<repo>]dagq resume <run ID>`、[ADR-0021](../adr/0021-maintainer-and-supervisor-workspace-names-follow-the-run-style.md)）が解消し、解消後は着地がworktreeを消すので`/exit`で終えてから`integrate`し直す。runのworkspace名は`[<repo>]dagq#<task-id> <task title>`、descriptionは`run <run-id>`で（[ADR-0018](../adr/0018-run-workspace-named-after-the-task.md)）、`failed` / `interrupted`のworkspaceはruntimeが閉じないのでmaintainerが`dagq-session`に従って`cmux workspace close`する。
- `recover`はバイナリが拒否条件を判定する。skillはプロセスをkillせず、`doctor --full`の`blockers`をユーザーに示す。
- `show`・`goal show`・`doctor`の既定は圧縮形で（長い文字列は300文字で`…`と`truncated: true`、`show`は最新runと直近10件のイベントの要点、`doctor`はrun 1件1行相当）、skillはそれぞれの説明に`--full`と切り詰めを書き、receiptやpayload、`run_dir`、`blockers`が要る手順では`--full`を付ける。

### marketplaceとinstall

repository rootの`.claude-plugin/marketplace.json`がこのrepository自身をmarketplaceにする（marketplace名`dagq`、`owner.name` `hisamekms`、`plugins`は`claude-dagq`の1件で`source`はrepository相対の`./plugins/claude-dagq`）。ユーザーの導線は2行。

```sh
claude plugin marketplace add hisamekms/dagq
claude plugin install claude-dagq@dagq
```

`add`はGitHubのrepositoryをcloneし、`install`はそのcloneの`./plugins/claude-dagq`からuser scopeに入れる。更新は`claude plugin marketplace update dagq`と`claude plugin update claude-dagq@dagq`。pluginはskillと`SessionStart` hookだけでinstallに`-y`を要する宣言commandはなく、runtimeバイナリは同梱しない（Releaseから別に入れる。launcherのエラー文がその手順を持つ）。

### 読み込みと検証

- 検証: `claude plugin validate plugins/claude-dagq`（hookとskillを含む。Claude Code 2.1.280で確認）と`claude plugin validate .claude-plugin/marketplace.json`（`--strict`も通る）、inventory: `claude --plugin-dir plugins/claude-dagq plugin details claude-dagq`。
- 開発中の読み込み: `claude --plugin-dir /path/to/dagq/plugins/claude-dagq`（そのsessionのみ）。supervisorがworkerに渡すのもこの形（`up --plugin-dir`）。
- marketplace経由のinstallは、使い捨ての`HOME` / `CLAUDE_CONFIG_DIR`でローカルpathを`marketplace add`して`install`し、`plugin list`と`plugin details`でskillが載ることを確認する（task 29、Claude Code 2.1.278で確認。task 65以降は`plugin details`でskill 5件とhook 1件）。
- `tests/plugin.rs`がmanifest（name、versionの一致）、marketplace manifest（marketplace名、pluginのnameとrepository相対の`source`がpluginのdirectoryを指すこと）、launcherのversion比較（`--version`と`locate`だけ答えるfake binaryを`DAGQ_BIN`にして、major.minorが同じならstderrが空、1 minor違えばstderrに`{"warning": ...}`が出てexit 0）、skill一覧（`dagq` / `dagq-land` / `dagq-maintain` / `dagq-recover` / `dagq-session`）、各`SKILL.md`が8 KB以下であること、`reference/`のファイルと本文からの参照が過不足なく対応すること、frontmatter（先頭行`---`、`name`がdirectory名、`description`）、hook（`hooks.json`の形式、matcherが`compact` / `clear`だけ、scriptが実行可能、role無し・別roleで出力が空、maintainerで`status`のJSON、`DAGQ_QUEUE`での解決、バイナリ無し・`status`失敗で1行とexit 0）、launcherの解決（`XDG_DATA_HOME`配下、worktreeからの共有、`DAGQ_DB`の優先、`binary_version`と`plugin_version`）・エラー（Release URL、tarball名、`SHA256SUMS`、`~/.local/bin`、`cargo build --locked`を含むこと）・`init`・登録・`show`を実バイナリで確認する。テストは`XDG_DATA_HOME`を一時dirに向け、開発者の実queueに触れない。skillに書いたコマンド列のうち自動テストにしないもの（goal系の`goal add` → `add --goal` → `ready` → `goal show` → `goal close`、runtime系の`up` → `status` → `down`）は、skillを変えたtaskのrun sessionが`cargo build --locked`したバイナリと使い捨てrepository・使い捨てqueue（`--db`）で実行し、その実行ログをreceiptのevidenceに残す（task 13、task 16）。
