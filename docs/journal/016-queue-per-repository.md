---
id: journal-016
type: journal
title: One queue per repository under the user directory
status: done
created: 2026-09-22
updated: 2026-09-22
plan_step: 5
queue_task: null
depends_on_journal: []
verify:
  - cargo fmt --all --check
  - cargo test --locked
  - cargo clippy --locked --all-targets -- -D warnings
related:
  - plan-rust-runtime-mvp
  - design-persistence
---

# 016: One queue per repository under the user directory

## Goal

[plans/current.md](../plans/current.md) ステップ5。1 repositoryに1 queueとし、DBをユーザーDIRに置いてcwdから解決できるようにする。

- DBは`~/.local/share/cmux-taskq/<Git common directoryの正規化パスのhash>/queue.db`（`XDG_DATA_HOME`があれば従う）。run dir・worktree・ログも同じ配下に置く。
- CLIはcwdから`git rev-parse --git-common-dir`でqueueを解決する。`--db`は使い捨てrepositoryとテスト用の明示overrideとして残す。`init`は「このrepositoryのqueueを作る」操作になり、`supervise --repo`と`integrate --repo`は不要になる。
- `queue_repository`の束縛検査は残す。
- `plugins/claude-taskq/`のskillとlauncherを新しい解決方法（`--db`なし、`--repo`なし）に追従させる。
- ADRを追加し、`persistence.md`・`supervisor-lifecycle.md`・READMEを更新する。

完了条件: repository内の任意のworktree（run worktreeを含む）から`--db`なしで同じqueueが使え、別のrepositoryからは別のqueueになることがテストで確認できる。既存のunit testは`--db` overrideでそのまま通る。

## Log

### 2026-09-22 claude (worker)

- worker として開始。branch `journal/016-queue-per-repository`、worktree `.worktrees/016-queue-per-repository`。src/main.rs、runtime.rs、adapters.rs、runtime_store.rs、tests/{cli,plugin,runtime,e2e}.rs、plugin の launcher と skill、README、design 3件を読了
- 決定（queue の場所）: `$XDG_DATA_HOME/cmux-taskq/<hash>/queue.db`、`XDG_DATA_HOME` が未設定・空・相対 path なら `$HOME/.local/share`。hash は canonicalize した Git common directory の UTF-8 bytes の SHA-256 hex 先頭 16 文字（`sha2` crate）。人が hash dir を repository に戻せるよう `init` が同じ dir に `repository` ファイル（common dir の path 1 行）を書く
- 決定（run dir）: `<db>.runs/<run-id>/` をやめ、DB と同じ dir の `runs/<run-id>/` にする。`--db PATH` でも同じ規則（`dirname PATH`/runs）。run ID は UUID なので同じ dir に複数 DB があっても衝突しない
- 決定（CLI）: `--db` は任意の override。なしなら cwd の `git rev-parse --path-format=absolute --git-common-dir` から解決する。`supervise --repo` / `integrate --repo` は任意の override として残し、既定は cwd。`integrate` の「run の記録した repo_path を既定にする」挙動はやめる（cwd 解決と二重になり、`--db` を使う e2e は `--repo` を渡せば足りる）。新しい `locate` サブコマンドが解決結果を JSON で返し（DB を開かない）、plugin の `--resolve` はこれを使う
- 決定（束縛）: cwd から解決した queue は `init` 時点で `queue_repository` に common dir を記録し、以後の全コマンドで open 直後に一致を検査する。`--db` の queue は従来どおり最初の `supervise` で束縛し、`supervise`/`integrate` だけが検査する（tests/cli.rs、e2e は crate の cwd から `--db` で使い捨て repository を扱うため）
- 決定（`init` の dir 作成）: `init` は解決方法に関わらず DB の親 dir を `create_dir_all` する。launcher の `mkdir -p` は不要になる

### 2026-09-22 claude (worker) 続き

- 実装: `src/infrastructure/location.rs`（`QueueLocation::resolve/explicit/for_repository/prepare`、`runs_dir`、`repository_hash`、`data_home`）、`adapters::git_common_dir`（`GitRepository::inspect` も共用）、`runtime_store::{bind_repository, assert_repository}`、`runtime::integrate(db, id, repo: &Path)`（Option をやめ、main.rs が `--repo` か cwd を渡す）、main.rs の `locate` と解決・束縛の流れ。`sha2 = "0.10"` を追加（Cargo.lock 更新、`--locked` で通る）。migration は不要（schema v4 のまま）
- 気付き: `supervise` は DB path を canonicalize するので、`run_dir` は `/private/var/...` になる。テストの比較は `db.canonicalize()` 側で行う（runtime テストと e2e で一度ずつ踏んだ）
- wrapper（`session`）には `--db` を明示的に渡し続ける。cwd（run worktree）からでも同じ queue に解決されるが、supervisor が開いたファイルと同一であることは引数で保証する方が確実で、wrapper が git に依存しない
- launcher `--resolve` は `cmux-taskq locate` の pretty JSON の先頭 `{` を剥がして `binary`/`version`/`repo` を前置する（sh で JSON を parse しない）。`repo` は repository 外では空文字
- テスト: `tests/location.rs` 5件（root・subdirectory・第2 worktree・`runs/<id>/worktree` 配下の run worktree から同じ queue、別 repository は別 queue、repository 外は失敗、`--db` の優先と非束縛、`HOME` fallback と相対/空 `XDG_DATA_HOME` の無視、別 repository に束縛された queue の拒否）、`location.rs` の unit test 5件（SHA-256 の既知値 `fbf971f8b891f789` for `/tmp/repo/.git` など）、`tests/plugin.rs` を `XDG_DATA_HOME` 一時 dir で書き直し、`tests/runtime.rs` に `runs/<id>/worktree` の配置 assert、`tests/e2e.rs` は `--db`/`--repo` をやめて使い捨て repository を cwd に `XDG_DATA_HOME` で解決させ、run worktree からの `locate` が同じ DB を返すことも確認
- ゲート: fmt / test 56件（e2e は ignored）/ clippy / llvm-cov 行 89.19% / e2e（cmux 0.64.25、supervise 6.1〜6.2 秒）通過。`claude plugin validate plugins/claude-taskq` → Validation passed
- docs: ADR-0006、persistence（Queue location 節）、supervisor-lifecycle、plugin-integration、overview、README、plans/current.md ステップ5 を更新。`docs/journal/README.md` の Open は触っていない（SV が merge 時に更新）
- 未検証: 実 Claude から plugin skill で `supervise` を cmux workspace に起動する経路（012 のドッグフーディングで確認）

## Result

1 repository に 1 queue を `$XDG_DATA_HOME/cmux-taskq/<hash>/queue.db`（既定 `~/.local/share`、hash は canonical Git common dir の SHA-256 hex 先頭16文字）に置き、`--db` なしで cwd から解決するようにした。run dir・worktree・ログは同じ dir の `runs/<run-id>/`。`init` は dir 作成・`repository` ファイル・`queue_repository` の束縛を行い、cwd 解決の全コマンドが束縛を検査する。`--db PATH` は明示 override として残り（run dir は `dirname PATH`/runs）、`supervise --repo` / `integrate --repo` は既定 cwd の任意 override になった。新しい `locate` が解決結果を JSON で返す。plugin の launcher と skill は `--db`/`--repo` を渡さず、`CMUX_TASKQ_DB` だけを `--db` に変換する。

完了条件: `tests/location.rs` が root・第2 worktree・run worktree から同じ queue、別 repository は別 queue、`--db` の優先、`XDG_DATA_HOME`/`HOME` の扱い、束縛違反の拒否を確認。既存 unit test は `--db` で無変更に通り、e2e は cwd 解決で通る。fmt / test / clippy / llvm-cov 89.19% / e2e 通過。

## Promoted

- queue の場所、hash、`runs/`、`locate`、束縛の規則と理由 → [ADR-0006](../adr/0006-queue-per-repository.md)、[design/persistence.md](../design/persistence.md) Queue location
- `supervise` / `integrate` の cwd 既定と `--repo` override、wrapper への `--db` 明示 → [design/supervisor-lifecycle.md](../design/supervisor-lifecycle.md)
- launcher の `--resolve` と skill の起動手順 → [design/plugin-integration.md](../design/plugin-integration.md)
- 利用手順 → README
