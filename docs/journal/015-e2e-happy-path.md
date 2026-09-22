---
id: journal-015
type: journal
title: End-to-end happy path with cmux and a stub agent
status: done
created: 2026-09-22
updated: 2026-09-22
plan_step: 4
queue_task: null
depends_on_journal: []
verify:
  - cargo fmt --all --check
  - cargo test --locked
  - cargo clippy --locked --all-targets -- -D warnings
  - cargo llvm-cov --locked --fail-under-lines 80
  - cargo test --locked --test e2e -- --ignored
related:
  - design-supervisor-lifecycle
  - journal-003
---

# 015: End-to-end happy path with cmux and a stub agent

## Goal

`tests/e2e.rs` に、実バイナリ・実Git・実cmuxを使うハッピーパスのe2eテストを1本置く。Claudeの代わりに、promptを受け取って変更・commit・receipt書き込みを行うstubスクリプトを `--claude` に渡す。

- 使い捨てrepositoryとDBを作り、`init` → `add` → `ready` → `supervise --repo ... --claude <stub>` をバイナリで実行する。
- cmux workspaceが作られ、`session` wrapperがstubを起動し、receipt検証が通って `awaiting_integration` になることを `show` のJSONとcmuxの一覧で確認する。
- 006（workspace close）と008（`integrate` で `completed`）がmergeされたら、それらの確認を同じテストに追記する。初版はそこまで含めない。
- cmuxが必要なので `#[ignore]`。cmuxの `ping` が失敗したら明確なメッセージでskipではなくfailにする。
- 残ったworkspaceはテストが必ず閉じる。

完了条件: `cargo test --locked --test e2e -- --ignored` がcmux起動環境で通り、AGENTS.mdのe2e制約を満たす。cmux adapterの行カバレッジが上がる。

## Log

### 2026-09-22 claude (worker)

- worker として開始。branch `journal/015-e2e-happy-path`、worktree `.worktrees/015-e2e-happy-path`。cmux 0.64.25 (106)、`cmux ping` → `PONG`
- 事前調査（使い捨て workspace `taskq-e2e-probe`）: cmux は `--command` のプロセスが終了すると 1〜2 秒後に workspace を自動で閉じる。つまり stub が終わると workspace は supervisor が閉じなくても消える。テストの close guard は「既に閉じている」を失敗扱いにしない。006 の close 実装でも close 失敗（既に閉じている）を想定する必要がある。また supervisor の `read-screen`（terminal-final.txt）は wrapper 終了後の 1〜2 秒と競合するので `screen_capture_failed` イベントが出ることがある
- `cmux --json --id-format uuids identify --workspace workspace:N` の `caller.workspace_id` は大文字 UUID。`cmux --json --id-format uuids workspace list` の各要素 `id` も大文字。`cmux workspace close <UUID>` は UUID を受け付ける（`OK workspace:N`）

### 2026-09-22 claude (worker) 続き

- `tests/e2e.rs` を作成。stub は `--version` に応答し、`--session-id / --debug-file / --add-dir / -- PROMPT` を解析、prompt の `You are executing cmux-taskq task N, run X.` と `Write a completion receipt to PATH using a temporary file...` の行から run id と receipt path を `sed` で取り出す。`e2e.txt` を commit し、receipt を `.tmp` → `mv` で公開してから `sleep 3` で対話セッションらしく居残り、exit 0。argv は `--debug-file` に書き、テストが `--session-id <run id>` と `--add-dir <run dir>` を検証する
- テストは `show` を 200ms ごとに poll して `workspace_id` を拾い、UUID 検証と `cmux --json --id-format uuids workspace list` での存在確認を行う。supervise は 120 秒で打ち切り。guard は Drop で「まだ list にあれば close」（既に閉じていれば成功扱い）、supervise の子プロセスも Drop で kill
- 初回: 1 回 + 3 回連続で全て通過。supervise は 6.8〜8.1 秒、workspace 登録は起動から 0.66〜0.9 秒。テスト全体 8〜10.5 秒。guard が毎回 close していた（006 merge 前）
- SV から 006 merge の連絡。`git rebase main`（schema v3、supervisor が受理後に close）。`schema_version == 3`、`workspace_closed` イベント、`workspace_closed_at` が数値、`cleanup_failed` なし、`cmux workspace list` に workspace がないことを追加で assert。5 回連続で通過（supervise 6.9〜7.1 秒）、guard は毎回「already closed」。supervisor の close は cmux の自動 close より先に成功している
- `cargo llvm-cov -- --include-ignored` は失敗する: cmux が起動する `runner` は `LLVM_PROFILE_FILE` を継承せず、cwd（worktree）に `default_*.profraw` を書くので `worktree is not clean` で `failed` になる。計測手段の副作用であり製品の問題ではない。e2e はカバレッジ対象にしない（AGENTS.md どおり必須ゲートは通常実行）。この理由を overview.md に記載
- src/ は変更なし。`cmux workspace list` に他タスクのスモーク workspace（007 の `taskq 1 6b6c33f6...`）が残っていたが、自分のものではないので触っていない
- ゲート: fmt / test（e2e は ignored、他 32 件）/ clippy / llvm-cov 行 85.99% 通過
- SV から 007・009 merge の連絡。再度 `git rebase main`（衝突なし）。007 で Claude adapter が `--settings <run-dir>/claude-settings.json` を渡すようになり、stub は未知の引数で exit 64 するので `--settings` を受け付け、Stop hook が含まれることを確認するように変更
- 決定: stub は receipt 後に自分で終了せず、Stop hook が書く `idle.json`（`hook_event_name` `session_id` `stop_hook_active`）を tmp → mv で書き、TTY から `/exit` 行を `read` するまで待つ。これで 007 の `session_idle_observed` → `send_exit`（`cmux send` + `send-key enter`）→ `exit_requested` → `session_exited` が実 cmux で通る。SV は「stub が自分で exit しても良い」と言ったが、cmux 固有の経路こそ e2e で検証したいので採用。`/exit` が届かない場合は supervisor の 120 秒 timeout がエラーになるので、テストの打ち切りは 180 秒にした
- 3 回連続通過。supervise 6.1〜8.7 秒、テスト全体 7.4〜10.1 秒。`exit_request_timed_out` なし。追加 assert: `session_idle_observed.session_id == run id`、`exit_requested.workspace_id == workspace`
- ゲート（rebase 後）: fmt / test 38 件（e2e は ignored）/ clippy / llvm-cov 行 87.66% 通過
- SV から 008 merge の連絡。`git rebase main`（衝突なし、schema v4）。テスト末尾に統合確認を追記: supervise 直後は temp repo の `main` が base のまま（supervisor は merge しない）、`integrate 1` は `not_integrated`（`main == base`）で task は `in_progress` のまま。`git merge --ff-only taskq/<run id>` で main を進めてから `integrate 1` → `outcome: integrated`、task `completed`、run `integrated`、`run_integrated` イベント、worktree は残る
- 3 回連続通過（supervise 6.0〜6.3 秒、テスト全体 7.5〜7.7 秒）。ゲート: fmt / test 42 件（e2e は ignored）/ clippy / llvm-cov 行 88.40% 通過

## Result

`tests/e2e.rs` に `#[ignore]` のハッピーパス 1 本を追加。実バイナリで `init → add（--verify 2 件）→ ready → supervise --repo --cmux --claude <stub> → integrate` を実行し、cmux workspace の作成（UUID、`workspace list` に存在）、stub による commit と receipt 提出、idle marker を見た supervisor の `/exit` 送信（`cmux send` / `send-key`）でのセッション終了、`awaiting_integration`、`result_commit == worktree HEAD`、イベント列（`worktree_created` `workspace_created` `wrapper_started` `agent_started` `receipt_observed` `session_idle_observed` `exit_requested` `session_exited` `supervision_finished` `verification_command` ×2 `validation_finished` `workspace_closed`）、process 2 件の exit 0、supervisor による close（`workspace_closed_at`、`cmux workspace list` から消えている、`cleanup_failed` なし）、lease 解放、候補なし、main 無変更、そして `integrate` が merge 前は `not_integrated`、`--ff-only` merge 後は task `completed` / run `integrated` になることを確認する。stub は adapter の argv（`--session-id` `--debug-file` `--add-dir` `--settings` `-- PROMPT`）を検証し、prompt 本文から receipt path を読む。cmux 不在は preflight で明確なメッセージの fail。workspace は guard が必ず片付ける。

cmux 0.64.25 で計 15 回通過（006 前 4 回、006 後 5 回、007 後 3 回、008 後 3 回）、supervise 6.0〜8.7 秒、テスト全体 7.4〜10.5 秒。flaky な挙動なし。006・007・008 の経路（close、`/exit` 送信、integrate）を全て含む。

## Promoted

- [design/overview.md](../design/overview.md): テストの 3 層（unit / runtime double / e2e）と e2e の実行方法、`--include-ignored` でカバレッジを取れない理由
- [design/supervisor-lifecycle.md](../design/supervisor-lifecycle.md): cmux がコマンド終了後に workspace を自動で閉じることと、supervisor の close / read-screen との競合
- [design/provider-lifecycle.md](../design/provider-lifecycle.md): 関連なし（adapter の引数は 007 が更新済み）
- ADR は追加しない。テストの置き場所と制約は AGENTS.md に既にある
