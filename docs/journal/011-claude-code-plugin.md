---
id: journal-011
type: journal
title: Claude Code local plugin
status: done
created: 2026-09-22
updated: 2026-09-22
plan_step: 8
queue_task: null
depends_on_journal: [8]
verify:
  - cargo fmt --all --check
  - cargo test --locked
  - cargo clippy --locked --all-targets -- -D warnings
related:
  - adr-0005
  - design-plugin-integration
---

# 011: Claude Code local plugin

## Goal

[plans/current.md](../plans/current.md) ステップ8。Claude Code内の依頼から、ローカルビルドしたバイナリ経由でタスクの登録・状態確認・実行開始・統合確認ができる薄いpluginを作る。

- バイナリの場所とバージョンを確認するskillを用意する。
- task登録では説明、受け入れ条件、依存、検証コマンドをCLIへ渡す。
- 結果はCLIのJSONをエージェントが読める形で返す。pluginはDBを直接変更しない。
- 完了通知は停止hookだけで判定せず、receiptと`show`の状態を使う。

完了条件: Claude Codeのセッションから登録・実行開始・結果確認・統合確認まで操作できる。

## Log

### 2026-09-22 15:00 claude

- worker として開始。branch `journal/011-claude-code-plugin`、worktree `.worktrees/011-claude-code-plugin`。006-009 が並行しているので変更は `plugins/claude-taskq/`、`tests/plugin.rs`、README の plugin 節、design-plugin-integration に留める
- 形式の確認（Claude Code 2.1.278、`claude plugin --help`、https://code.claude.com/docs/en/plugins-reference と /skills）:
  - manifest は `.claude-plugin/plugin.json`。必須は `name`（kebab-case）だけ。`version` / `description` / `author` / `keywords` は任意。`skills` は既定 `skills/` に加算、`bin/` は「Bash tool から呼べる実行ファイル」、`hooks/hooks.json`・`commands/`・`agents/` は今回使わない
  - skill は `skills/<dir>/SKILL.md`。frontmatter は先頭行の `---` 必須、全 field 任意で `description` 推奨。`name` は command の最終 segment（plugin では `/claude-taskq:<name>`）。skill 本文では `${CLAUDE_PLUGIN_ROOT}` が plugin の絶対 path に置換される。`allowed-tools: Bash(... *)` はその turn の事前許可
  - ローカルで読み込む方法: `claude --plugin-dir <path>`（そのセッションのみ、設定変更なし）、`claude plugin validate <path>` で manifest と skills の検証、`claude plugin details` で inventory。settings の marketplace 登録や `~/.claude/skills/` へのコピーは不要なので SV への確認は不要

### 2026-09-22 16:00 claude

- 決定: skill は3つに分ける。`taskq`（バイナリ・DB の解決、`init`、`add`/`ready`、`list`/`show`/`candidates`/`status`/`doctor`、run 状態の読み方）、`taskq-run`（`supervise` の cmux workspace 起動、`show` による監視、完了判定、`integrate`）、`taskq-recover`（`doctor`/`recover`/再試行）。1つにまとめると description が全用途を抱えて長くなり、on-invoke の token も無駄になる。`plugin details` の見積りは always-on ~386 tok、on-invoke 1.0k〜1.9k
- 決定: skill は直接バイナリを呼ばず `${CLAUDE_PLUGIN_ROOT}/bin/taskq`（POSIX sh の launcher）を呼ぶ。バイナリ（`CMUX_TASKQ_BIN` → PATH）と DB（`CMUX_TASKQ_DB` → `$(git rev-parse --path-format=absolute --git-common-dir)/taskq/queue.db`）の解決を1か所に置き、`exec cmux-taskq --db <db> "$@"` する。DB は開かない。見つからないときは CLI と同じ `{"error": ...}` を stderr に出す。`--resolve` で `{"binary","version","db","db_exists","repo"}` を返し、`init` のときだけ `mkdir -p` する。`--version`/`--help` は DB なしでそのまま渡す（`cmux-taskq --db X --version` も clap は受けるが、DB 解決が Git repository 外で失敗するのを避けた）
- 決定: DB の既定は Git common dir 配下。worktree 間で共有され、worktree の外にあるので `supervise` の「worktree 外か common dir 配下」の制約を満たす。README の記述と一致
- 見送り: `allowed-tools` による Bash 事前許可。docs では `${CLAUDE_SKILL_DIR}`/`${CLAUDE_PROJECT_DIR}` の置換だけが明記され、`${CLAUDE_PLUGIN_ROOT}` を含む prefix が効くか不明なので入れない。permission はユーザー（SV）が答える。`bin/` が Bash tool の PATH に載る仕様も、`--plugin-dir` で確実か検証していないので skill は常に絶対 path を使う
- `taskq-run` の cmux 起動は `--resolve` の JSON を sed で `BIN`/`DB`/`REPO` に取り出し、`cmux workspace create --name "taskq supervise" --cwd "$REPO" --command "'$BIN' --db '$DB' supervise --repo '$REPO'"`。workspace の shell は session の環境変数を継承しないので絶対 path が必要
- 検証: `claude plugin validate plugins/claude-taskq` → `Validation passed`。`claude --plugin-dir plugins/claude-taskq plugin details claude-taskq` → Skills (3) taskq, taskq-recover, taskq-run
- スモーク（使い捨て repository、scratchpad 配下の `smoke/repo` に `git init -b main` + 空 commit）:
  ```sh
  export CMUX_TASKQ_BIN=<worktree>/target/debug/cmux-taskq
  claude --plugin-dir <worktree>/plugins/claude-taskq -p --model sonnet --permission-mode bypassPermissions \
    --max-turns 20 --output-format json "Use the claude-taskq plugin's taskq skill. Report the cmux-taskq binary path and version and the queue database path, initialize the queue if needed, register a task titled 'Plugin smoke' with description 'exercise the plugin', acceptance 'show returns the task', verification command 'true', make it ready, then show it and report its status and whether it is a candidate. Reply in at most 8 lines."
  ```
  結果（7 turns、$0.12）: 「Binary: …/target/debug/cmux-taskq (version cmux-taskq 0.1.0). Queue DB: …/smoke/repo/.git/taskq/queue.db — didn't exist, so I ran `init` (schema v4). Registered task 1 "Plugin smoke", moved it to `ready`, then `show 1` confirms it … Status: ready. It appears in `candidates`」。launcher で `show 1` を直接叩いて `ready / Plugin smoke / ['true'] / show returns the task` を確認。この repository の `.git/taskq` は作られていない（`--resolve` で `db_exists: false`）
- 未検証: `taskq-run` と `taskq-recover` を実 Claude から実行する経路（cmux workspace 起動、`integrate`、`recover`）。コマンド列は README/design と同じで、012/014 のドッグフーディングで確認する。settings の marketplace 経由の恒久インストールも未検証（`--plugin-dir` で足りる）
- テスト: `tests/plugin.rs` 4件。manifest（name、version = `CARGO_PKG_VERSION`）、frontmatter（先頭 `---`、name = directory 名、kebab-case、description 40〜1024 文字、launcher を呼ぶ、`sqlite3 ` を含まない）、launcher の解決（common dir 配下、worktree からの共有、`CMUX_TASKQ_DB` 優先、`init` の mkdir、`add`/`ready`/`show`、`--version`）、エラー（バイナリなし、非実行ファイル、repository 外）。テスト合計 42 → 46 件、lines 88.40%（src のみ計測なので変化なし）
- 変更なし: `src/`（migration 不要）、`docs/plans/current.md`（ステップ5の状態更新は SV が merge 時に行う）

## Result

`plugins/claude-taskq/` に Claude Code plugin を追加した。`.claude-plugin/plugin.json`（name `claude-taskq`、version はクレートと同じ 0.1.0）、launcher `bin/taskq`、skill 3件（`taskq`: バイナリ・DB の解決、`init`、登録と `ready`、一覧・詳細・候補・`status`・`doctor`、run 状態の読み方 / `taskq-run`: 専用 cmux workspace での `supervise` 起動、`show` での監視、完了判定、`integrate` / `taskq-recover`: `doctor`、`recover`、再試行）。launcher は `CMUX_TASKQ_BIN` → PATH でバイナリを、`CMUX_TASKQ_DB` → `<git common dir>/taskq/queue.db` で DB を解決して `cmux-taskq --db` に渡すだけで、DB は開かない。完了は Stop hook ではなく `show` の run `status`/`result_commit`/`last_error`/`validation_finished` で判定するよう skill に書いた。

完了条件: `claude plugin validate` 通過、`--plugin-dir` で読み込んだ `claude -p` session が使い捨て repository で `init` → `add` → `ready` → `show`/`candidates` を実行し結果を報告することを確認。`tests/plugin.rs` が manifest・frontmatter・launcher を `cargo test` で検査する。fmt / test（46件）/ clippy / llvm-cov（lines 88.40%）通過。README に「Use from Claude Code」、design-plugin-integration に構成と契約を追記。

未検証: `taskq-run` / `taskq-recover` を実 Claude から通す経路は 012/014 のドッグフーディングで確認する。

## Promoted

- plugin の構成、launcher の解決規則、skill の契約、読み込み・検証方法 → [design/plugin-integration.md](../design/plugin-integration.md)
- 利用手順と skill 一覧 → README「Use from Claude Code」
