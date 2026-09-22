---
id: journal-018
type: journal
title: Merge queue with rebase, re-validation, and squash landing
status: done
created: 2026-09-22
updated: 2026-09-22
plan_step: 7
queue_task: null
depends_on_journal: [17]
verify:
  - cargo fmt --all --check
  - cargo test --locked
  - cargo clippy --locked --all-targets -- -D warnings
related:
  - plan-rust-runtime-mvp
  - design-supervisor-lifecycle
  - design-domain-model
---

# 018: Merge queue with rebase, re-validation, and squash landing

## Goal

[plans/current.md](../plans/current.md) ステップ7。`integrate`を「mainへの手動mergeを確認する」操作から「runtimeがmainへ着地させる」操作に置き換え、mainに1 task = 1 commitの直線の履歴を積む。

- 統合スロットは1つ。`awaiting_integration`のrunを検証完了の古い順（FIFO）に取り出し、`integrating`にする。
- runtimeがrun worktreeで最新mainへの`git rebase`を試す。衝突なしなら再検証する（headの親を辿るとmain headに到達する、worktreeがclean、検証コマンドが通る）。
- 再検証したheadのtreeを1 commitにsquashしてmainを進める。messageはtaskのtitleとreceiptのsummary、trailerに`Taskq-Task: <id>`と`Taskq-Run: <run-id>`。着地したcommitを`result_commit`にし、runを`integrated`、Taskを`completed`にする。fast-forwardは使わずsquash固定。
- 衝突したら`rebase --abort`して`needs_session`で止める。SVが`claude --resume <run-id>`でworktreeにworkspaceを開き直し、セッションが解消・検証コマンド再実行・receiptの書き直しを行う。runtimeは新しいheadで再検証から続ける。変更が不要になった場合はセッションが`failed`のreceiptで理由を書く。
- run branchの詳細履歴は`refs/taskq/runs/<run-id>`に残し、worktreeは着地後に削除する。
- 初期はSVが`integrate`を呼ぶ承認制。承認なしの自動着地は後回し。pushはSVが行う。
- `plugins/claude-taskq/`のskill（統合）を新しい`integrate`と`needs_session`の扱いに追従させる。
- ADRを追加し、`domain-model.md`・`persistence.md`・`supervisor-lifecycle.md`・READMEを更新する。

完了条件: 衝突なしのrunがClaudeなしで着地し、衝突したrunがセッションでの解消後に着地し、いずれもmainが直線で1 task = 1 commitになることがテスト（e2e含む）で確認できる。着地前の`result_commit`と着地後のmain headのtreeが一致する。

## Log

### 2026-09-22 claude (worker)

- worker として開始。branch `journal/018-merge-queue`、worktree `.worktrees/018-merge-queue`。016/017/008 のジャーナル、ADR-0006/0007、design 3件、runtime.rs / runtime_store.rs / adapters.rs / sqlite.rs / domain.rs / main.rs、tests/{runtime,e2e,queue,cli,plugin}.rs、plugin skill 3件、README を読了
- 決定（状態）: `RunStatus` に `integrating`（統合スロットを持つ）と `needs_session`（衝突・再検証失敗でセッション待ち）を追加。migration `0006_merge_queue.sql`（schema v6）で 0004 と同じ手順で `task_runs` を作り直し、CHECK に2状態を加える。`one_unfinished_run_per_task` は2状態を含めて作り直し、統合スロットは部分 UNIQUE index `one_integrating_run_per_queue ON task_runs((1)) WHERE status='integrating'` で DB が保証する
- 決定（スロットの所有）: `integrate` プロセスは run 単位 lease（`run_leases`、017 と同じ表）を `integrating` の間だけ持ち、既存の `Heartbeat` thread で更新する。`doctor`/`status` は `integrating` を未完了 run として並べ、`integrate` プロセスが死んで lease が stale になった run は `recover` で `awaiting_integration` に戻す（`interrupted` にはしない: 検証済みの成果は残っている）。次の `integrate` は途中の rebase があれば `rebase --abort` してからやり直す
- 決定（FIFO）: `integrate --next` は `awaiting_integration` の run を `validation_finished` イベントの id 順（検証完了の古い順）で1件取る。`needs_session` の run は `--next` では取らず、SV がセッション完了を確認して `integrate ID` で明示的に再開する
- 決定（書き直した receipt の検出）: mtime/hash も `resume` サブコマンドも使わない。`integrate ID` が再開の明示ステップで、runtime は rebase の前に「receipt が parse でき、`result` が `succeeded`（`failed` なら run を `failed` にする）、`commit` が worktree の現在の HEAD に一致する」ことを要求する。衝突なし run では HEAD = `result_commit` なので検証済みの receipt がそのまま通り、`needs_session` から戻る run ではセッションが新しい head で receipt を書き直したことの証明になる。書き直していなければ理由付きで `needs_session` のまま
- 決定（再検証の順序）: (1) worktree が存在し、途中の rebase を abort、(2) receipt の検査（上記）、(3) `git rebase <main head>`（衝突 → status と conflicted files を記録して abort → `needs_session`）、(4) HEAD が main head の子孫で main head と異なる（rebase で commit が空になった場合は `needs_session`: 変更不要ならセッションが `failed` receipt を書く）、(5) clean、(6) 検証コマンド（ログは `<run-dir>/integrate-verify-N.log`）。(4)〜(6) の失敗は rebase 済みの worktree をそのまま残して `needs_session`（セッションが rebase 後の状態を直せる）
- 決定（着地）: `git commit-tree <HEAD>^{tree} -p <main head> -m title -m summary -m trailers` で1 commit を作り、`refs/taskq/runs/<run-id>` を rebase 後の HEAD に向けてから main を進める。main を checkout している worktree（`git worktree list --porcelain` の `branch refs/heads/main`）があればそこで `git merge --ff-only <commit>`（index と working tree も一緒に進む。ローカル変更と衝突すれば失敗して run は `awaiting_integration` に戻る）、なければ `git update-ref refs/heads/main <commit> <main head>`（CAS）。`update-ref` だけだと main checkout の working tree が逆向きの差分に見えるため
- 決定（後始末）: 着地 → DB（`integrated`/`completed`、`result_commit` = 着地 commit、lease 解放）→ `git worktree remove --force`（clean は検証済み。ignored な build 成果物で止まらないよう force）→ `git branch -D taskq/<run-id>`（履歴は `refs/taskq/runs/<run-id>` が持つ）。後始末の失敗は `cleanup_failed` イベントと `last_error` に残し status は変えない
- 決定（error の扱い）: main を進める前の Git/DB error は `integration_error` イベントと `last_error` を書いて元の status（`awaiting_integration` / `needs_session`）に戻し lease を解放する。main を進めた後の DB error は status を戻さず error にする（既知の限界: `recover` → `integrate` で rebase 後に commit が空になり `needs_session` になる。実機で起きたら 010 で扱う）
- `main_head` の読み直しは 017 の `fill_slots` が claim ごとに `refs/heads/main` を読んでいるので変更不要。着地後の依存 task が着地 commit から始まることをテストで確認する
- AGENTS.md は触らない。SV の merge 手順（worktree で fmt/test/clippy/llvm-cov/e2e → main へ ff merge → push）は 019 で cmux-taskq の `integrate`（squash 着地、push は SV）に置き換わる。今回の変更で「fast-forward 優先」の記述が runtime の挙動と食い違うようになる点を SV に伝える

### 2026-09-22 claude (worker) 続き

- 実装: migration `0006_merge_queue.sql`（`task_runs` 作り直し、`integrating` / `needs_session`、`one_unfinished_run_per_task` の作り直し、`one_integrating_run_per_queue`）、`domain::{RunStatus::Integrating, NeedsSession}`、`IntegrationOutcome::{Integrated, NeedsSession, Failed, NoRunAwaiting}`（`NotIntegrated` は削除）、`runtime_store::{Landing, next_awaiting_integration, begin_integration, defer_integration, fail_integration, abort_integration, finish_integration, record_cleanup_failure}`、`recover_run` の `integrating → awaiting_integration`、`active_runs` に `integrating`、`adapters::GitRepository::{rebase_in_progress, rebase_abort, rebase, conflicted_files, tree_of, commit_tree, update_ref, ref_exists, main_checkout, advance_main, remove_worktree_and_branch}`、`runtime::{IntegrateTarget, integrate, land, commit_message, remove_landed_worktree}`、main.rs の `integrate [ID] [--next]`（clap の `required_unless_present` / `conflicts_with`）
- 気付き（worktree の削除）: `integrate` を run worktree の中から呼ぶと `GitRepository::inspect` の root がその worktree になり、削除後に `git -C root` が失敗する。`git worktree list --porcelain` の先頭（main working tree）を先に解決してそこから `worktree remove` と `branch -D` を実行する `remove_worktree_and_branch` にまとめた
- 気付き（`--next` の順序）: `ORDER BY (SELECT MIN(e.id) ...)` は `validation_finished` のない run（v5 以前の手作り行）で NULL が先頭に来る。`NULLS LAST` を付けた
- 気付き（e2e）: 2 件同時のテストで `--next` は依存 task ではなく先に検証が終わった 2 件目を取る（FIFO どおり）。2 件目は 1 件目と同じ `e2e.txt` を書くので `needs_session` になり、テストがセッション役で `git rebase` → 解消 → `rebase --continue` → receipt 書き直し → `integrate ID` で着地させる形にした
- 気付き（test provider）: `TestProvider::command` が prompt に fixture の検証コマンド `test -f seed.txt` を要求していたため、検証コマンドの違う task を混ぜられなかった。見出し `Verification commands (run in the worktree):` の確認に緩めた
- テスト: tests/runtime.rs 33 件（新規 6 件: 衝突なし着地（message、tree、`refs/taskq/runs`、worktree 削除、依存解放、二重着地拒否、別 repository 拒否）、FIFO と rebase（main checkout 経由と detached での `update-ref` 経由）、衝突 → `needs_session` → 未解消の再試行 → セッション解消 → 古い receipt の拒否 → 着地、`failed` receipt、rebase 後の検証失敗（rebase 済み tree の保持と解消後の着地）、スロットの排他・`doctor`・`recover` → `awaiting_integration`・main checkout の衝突するローカル変更で `integration_error` → 戻して着地）、既存の並列テストを着地経由に変更（依存 task の base が着地 commit）。tests/queue.rs に v5 → v6 migration（index、CHECK、FK、`needs_session` の占有）。tests/cli.rs の `integrate` 引数検査。tests/e2e.rs 2 件を `integrate` 着地と `needs_session` 解消に拡張
- ゲート: fmt / test 66 件（e2e 2 件は ignored）/ clippy / llvm-cov 行 87.10% / e2e（cmux 0.64.25、1 件 9 秒、2 件同時 + 依存 + 解消 14 秒）通過。`claude plugin validate plugins/claude-taskq` → Validation passed
- docs: ADR-0008、design/{supervisor-lifecycle, persistence, domain-model, overview, plugin-integration}、README（status、command table、"Land a run on main" 節）、plans/current.md ステップ7と Ordering、plugin skill 3 件（taskq-run に §5 "Resume a run that needs a session"）。`docs/journal/README.md` の Open と AGENTS.md は触っていない
- SV への注記: AGENTS.md の「main へ merge（fast-forward 優先）」は 019 で `integrate` の squash 着地に置き換える。それまでの暫定運用（worker branch を SV が ff merge）は runtime を使わないので矛盾はしないが、cmux-taskq で流す task は `integrate` が着地させるため merge commit / ff は作らない
- 未検証: 実 Claude を `claude --resume <run-id>` で開き直して衝突を解消させる経路（010 / 013 のドッグフーディングで確認）。main を進めた後に DB 更新が失敗する経路（既知の限界として ADR-0008 に記載）

## Result

`integrate` を「手動 merge の確認」から「runtime が main へ着地させる merge queue」に置き換えた（ADR-0008、schema v6）。`integrate ID` / `integrate --next` は統合スロット（`integrating`、`integrate` プロセスの lease）を取り、途中の rebase を abort → receipt が worktree の HEAD を指し `succeeded` であることを確認 → `git rebase <main head>` → 再検証（HEAD が main の子孫で main と異なる、clean、検証コマンドを `integrate-verify-N.log` に再実行）→ `commit-tree` で 1 commit（title、summary、`Taskq-Task` / `Taskq-Run` trailer）→ `refs/taskq/runs/<run-id>` を rebase 後の HEAD に向ける → main を checkout している worktree で `merge --ff-only`（なければ `update-ref` CAS）→ run を `integrated`、`result_commit` を着地 commit、Task を `completed` → worktree と branch を削除する。衝突は `rebase --abort` して `needs_session`、rebase 後の再検証失敗は rebase 済み tree を残して `needs_session`。SV が `claude --resume <run-id>` で開いたセッションが解消・検証再実行・新しい head での receipt 書き直しを行い、`integrate ID` で再開する。receipt の `commit` が HEAD に一致することが再開の検出で、`failed` receipt は run を `failed` にする。`--next` は `validation_finished` 順の FIFO で `needs_session` を取らない。main を進める前の error は元の status に戻し、`integrate` プロセスが死んだ run は `recover` が `awaiting_integration` に戻す。

完了条件: 衝突なしの run が Claude なしで着地し main が seed → 1 commit の直線で tree が `result_commit` の tree に一致すること、2 件が FIFO で着地し 2 件目が rebase されること、衝突した run が `needs_session` になりセッション役の解消後に着地すること、着地後に worktree が消え `refs/taskq/runs/<id>` が残ること、依存 task が着地 commit から始まることを tests/runtime.rs で、1 件の着地と 2 件同時からの `needs_session` 解消を tests/e2e.rs で確認。fmt / test 66 件 / clippy / llvm-cov 87.10% / e2e 通過。

## Promoted

- 着地の手順、スロットと lease、`needs_session` と `failed` receipt、main の進め方（checkout 経由の ff / update-ref）、commit message の契約、代替案と既知の限界 → [ADR-0008](../adr/0008-merge-queue-squash-landing.md)
- `integrate` の 8 手順、`needs_session` の SV 手順、error と `recover` → [design/supervisor-lifecycle.md](../design/supervisor-lifecycle.md)
- schema v6、`integrating` の遷移と lease、`one_integrating_run_per_queue`、`result_commit` の意味の変化 → [design/persistence.md](../design/persistence.md)
- 新 status と `IntegrationOutcome`、直線の main と `refs/taskq/runs` の不変条件 → [design/domain-model.md](../design/domain-model.md)
- 利用手順（`integrate ID` / `--next`、`needs_session` の resume）→ README、plugin skill 3 件
- ステップ7の状態 → [plans/current.md](../plans/current.md)
