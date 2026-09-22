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
  skills/taskq/SKILL.md        バイナリと DB の解決、登録、ready、一覧・詳細・候補・status・doctor、結果の読み方
  skills/taskq-run/SKILL.md    supervise の起動（専用 cmux workspace）、show による監視、完了判定、integrate
  skills/taskq-recover/SKILL.md doctor、recover、再試行
```

### launcher

skillはすべて`${CLAUDE_PLUGIN_ROOT}/bin/taskq`を呼ぶ。launcherはバイナリとDBのpathを解決して`cmux-taskq --db <db> <args>`を`exec`するだけで、DBを開かない。

- バイナリ: `CMUX_TASKQ_BIN`、なければPATHの`cmux-taskq`。どちらもなければ`{"error": ...}`をstderrに出し、`cargo build --locked`と`CMUX_TASKQ_BIN`の設定を案内する（CLI本体のエラー形式と同じ）。
- DB: `CMUX_TASKQ_DB`、なければ`$(git rev-parse --path-format=absolute --git-common-dir)/taskq/queue.db`。repositoryの全worktreeで共有され、worktreeの外にあるので`supervise`の配置制約を満たす。`init`のときだけ親ディレクトリを作る。
- `--resolve`: `{"binary","version","db","db_exists","repo"}`を返す。skillはこれをユーザーへの報告と、cmux workspaceへ渡す絶対pathの取得に使う。`--version` / `--help`はDBなしでバイナリに渡す。

### skillの契約

- 完了はStop hookやreceiptファイルの存在ではなく、`show`のrun `status`（`awaiting_integration` / `integrated`）、`result_commit`、`last_error`、`validation_finished`イベントで判定する。
- `supervise`はブロックするのでClaude Codeのshellでは実行せず、`cmux workspace create --cwd <repo> --command "<bin> --db <db> supervise --repo <repo>"`で専用workspaceに起動する。workspaceのshellはsessionの環境変数を継承しないため絶対pathを渡す。
- mainへのmergeは手動。skillは`integrate ID`の`outcome`を読んで結果を伝える。
- `recover`はバイナリが拒否条件を判定する。skillはプロセスをkillせず、`doctor`の`blockers`をユーザーに示す。

### 読み込みと検証

- 検証: `claude plugin validate plugins/claude-taskq`、inventory: `claude --plugin-dir plugins/claude-taskq plugin details claude-taskq`。
- 利用: `claude --plugin-dir /path/to/cmux-taskq/plugins/claude-taskq`（そのsessionのみ）。恒久化するにはsettingsのmarketplaceにローカルpathを登録して`claude plugin install claude-taskq@<marketplace>`する（未検証）。
- `tests/plugin.rs`がmanifest（name、versionの一致）、frontmatter（先頭行`---`、`name`がdirectory名、`description`）、launcherの解決・エラー・`init`・登録・`show`を実バイナリで確認する。
