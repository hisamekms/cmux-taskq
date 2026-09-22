---
id: journal-008
type: journal
title: Integration confirmation and task completion
status: done
created: 2026-09-22
updated: 2026-09-22
plan_step: 4
queue_task: null
depends_on_journal: [5]
verify:
  - cargo fmt --all --check
  - cargo test --locked
  - cargo clippy --locked --all-targets -- -D warnings
related:
  - design-domain-model
  - design-persistence
---

# 008: Integration confirmation and task completion

## Goal

[plans/current.md](../plans/current.md) ステップ4。手動mergeの後、成果commitがmainに含まれることを確認してTaskを`completed`にし、依存taskを解放する。

- `integrate ID`（名称は実装時に決める）コマンドを追加し、`awaiting_integration`のrunのresult commitが`refs/heads/main`の祖先であることをGitで確認する。
- 確認できたらrunを`integrated`、Taskを`completed`にし、イベントを記録する。merge/fast-forwardのみ対象とし、squash/cherry-pickは後回し。
- 確認できない場合は状態を変えない。

完了条件: mergeしたmainで`completed`になり依存taskが`candidates`に現れること、merge前やsquash後は`completed`にならないことをテストで確認できる。

## Log

### 2026-09-22 12:00 claude

- worker として開始。branch `journal/008-integration-confirm`、worktree `.worktrees/008-integration-confirm`。006/007/009 が並行しているため、変更は `integrate` に必要な範囲に留める
- 決定: コマンド名は `integrate ID`（task ID を取る）。同じ task に `awaiting_integration` の run は DB 制約で高々1件なので run ID を求める必要がない。`integrated` 済みの task に再実行すると「awaiting_integration の run がない」として JSON error（exit 1）になる
- 決定: repository は既定で `task_runs.repo_path`（supervise 時の checkout root）を `GitRepository::inspect` で開き、その common dir が `queue_repository.git_common_dir` と一致することを要求する。`--repo PATH` は移動した repository 向けの上書きで、同じ一致検査を通す。common dir を直接 `git -C` で開く案は `inspect` が `--show-toplevel` を前提にしているため見送った。worktree の path を `--repo` に渡しても同じ common dir に解決されるのでテストで確認した
- 決定: 判定は `git merge-base --is-ancestor <result_commit> refs/heads/main` のみ。fast-forward でも merge commit でも result commit 自体が main の祖先になる。squash / cherry-pick は別 SHA になるので祖先にならず、`{"outcome":"not_integrated","run":...,"main":<sha>,"reason":...}` を exit 0 で返し、DB は event も含めて一切変えない（`ClaimOutcome` と同じく判定結果は error ではなく outcome）。task 不在・awaiting run 不在・repository 不一致・Git 障害は error
- 決定: 遷移は lease を要求しない。`awaiting_integration` の run は検証終了時に lease を手放しており、supervise 側もこの状態の run に触らない。`UPDATE ... WHERE status='awaiting_integration'` と `WHERE status='in_progress'` を1トランザクションにまとめ、同時実行や二重実行は 0 行更新の error にする。event は `run_integrated`（result_commit, main, git_common_dir）と `task_status_changed`（in_progress → completed、run_id 付き）
- migration が必要だった: `task_runs.status` の CHECK に `integrated` がない。SQLite は CHECK を変更できないので `0003_integration.sql` で table を作り直す（rowid を明示して `show` の run 順序を保つ）。`one_integrated_run_per_task` の部分 UNIQUE index も追加。`DROP TABLE task_runs` は `foreign_keys=ON` だと run_events / run_processes の参照で失敗する（sqlite3 3.43 で確認）ため、`SqliteQueue::migrate` が transaction の外で `foreign_keys=OFF` にし、commit 前に `pragma_foreign_key_check` が 0 件であることを確認してから ON に戻す。`ALTER TABLE ... RENAME` は FK OFF のとき他 table の REFERENCES を書き換えないので、run_events の `task_runs` 参照はそのまま新 table を指す。schema version は 3（cli / queue / runtime のテストを更新）
- 既存の `succeeded` status は残した（統合を待たない運用向けに設計で予約されているもの）。今回使わない
- テスト: `tests/runtime.rs` に fast-forward（不統合 → ff → completed → 依存 task が candidates に現れる → 二重 integrate と store 直接呼び出しの拒否 → `integrated` 2件目の INSERT 拒否）、merge commit（別 repository の `--repo` 拒否、worktree path の `--repo` 受理）、squash（不統合のまま event 数不変）の3件。`tests/queue.rs` に v2 → v3 migration（run 2件・event・process を保持、FK 再有効化）と `one_integrated_run_per_task` の制約確認。`tests/cli.rs` に `integrate` の JSON error。合計 30 → 34 件
- `cargo llvm-cov --fail-under-lines 80`: lines 87.06%（前 86.03%）
- 未変更: `docs/plans/current.md`（ステップ4はまだ完了しない。冒頭の「`completed`への遷移…未実装」の文は 006/007/009 と同時に SV が直す方が衝突が少ない）

### 2026-09-22 14:00 claude

- SV の指示で main（006 workspace close、009 doctor/recover を含む）に rebase。migration を `0004_integration.sql` に改番し（schema version 4）、作り直す `task_runs` に 006 の `workspace_closed_at` 列を含めて INSERT ... SELECT でも複写する。migration テストは v3 DB（0001〜0003 適用済み、`workspace_closed_at` 入り）から v4 へ上げる形に変更
- 衝突は `sqlite.rs`（MIGRATIONS と `read_task` の可視性）、`main.rs`（Integrate と Doctor/Recover の並び）、`runtime.rs`（先頭の import）、`tests/runtime.rs`（末尾に両方の追加テスト）、README / design 3件（両方の記述を統合）。`integrate` 本体のロジックは変更なし
- 007（session exit request）を含む main にもう一度 rebase。衝突は README のコマンド表と supervisor-lifecycle.md の実装状況の文のみ。rebase 後: test 42件、lines 88.40%

## Result

`integrate ID [--repo PATH]` を追加した。task の `awaiting_integration` の run について、`result_commit` が `refs/heads/main` の祖先であることを Git（`merge-base --is-ancestor`）で確認し、確認できれば1トランザクションで run を `integrated`、task を `completed` にして `run_integrated` / `task_status_changed` を記録する。repository は run の `repo_path`（または `--repo`）を開き、common dir が `queue_repository.git_common_dir` と一致することを要求する。祖先でなければ `{"outcome":"not_integrated",...}` を返して何も変えない（squash / cherry-pick はここに落ちる）。awaiting run のない task は JSON error。migration `0004_integration.sql` で `task_runs` を作り直して `integrated` を CHECK に加え、`one_integrated_run_per_task` index を追加した（schema version 4、migration runner は FK を一時的に無効化して `foreign_key_check` で検証）。

完了条件: fast-forward と merge commit の後に `completed` になり依存 task が `candidates` に現れること、merge 前と squash 後は状態が変わらないこと、二重 integrate と2件目の `integrated` run が拒否されること、v2 DB の migration が run 履歴と FK を保つことをテストで確認。fmt / test（34件）/ clippy / llvm-cov（lines 87.06%）通過。

未検証: 実機 repository での `integrate`（010 / 012 のスモークで行う）。cherry-pick / squash の同等性判定は未実装のまま（計画どおり後回し）。

## Promoted

- [design/supervisor-lifecycle.md](../design/supervisor-lifecycle.md): 状態遷移図、`integrate` 節（判定手順、repository の決め方、event）
- [design/persistence.md](../design/persistence.md): schema version 4、`integrated` の遷移と `one_integrated_run_per_task`、CHECK 変更を table 作り直しで行う手順と FK の扱い
- [design/domain-model.md](../design/domain-model.md): `integrate` の遷移、`IntegrationOutcome`、`completed` の不変条件
- [design/overview.md](../design/overview.md): 実装状況
- [README.md](../../README.md): コマンド表と「Complete a task after merging」節
- ADR は追加しない。統合確認を手動 merge 後の明示操作にすることと merge / fast-forward のみ対象とすることは plans/current.md ステップ4の決定に含まれる
- `docs/plans/current.md` は未変更（ステップ4は 006 / 007 / 009 の完了後に SV が更新する）
