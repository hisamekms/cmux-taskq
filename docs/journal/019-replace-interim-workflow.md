---
id: journal-019
type: journal
title: Replace the interim SV/worker workflow with cmux-taskq operation
status: done
created: 2026-09-22
updated: 2026-09-22
plan_step: 9
queue_task: 2
depends_on_journal: [12]
related:
  - plan-rust-runtime-mvp
  - journal-index
---

# 019: Replace the interim SV/worker workflow with cmux-taskq operation

## Goal

[plans/current.md](../plans/current.md) ステップ9。ドッグフーディングが通ったら、このrepositoryで暫定的に行っているSV/worker運用をcmux-taskq前提に置き換える。

- AGENTS.mdのSV/worker節を、常駐SV sessionの手順に書き換える。SVはqueueに登録し、`supervise --parallel`を起動し、`read-screen`で完了を確認して差分をレビューし、`integrate`を呼び、`needs_session`のrunにはresumeで解消を指示し、pushする。`.worktrees/`とcmux workspaceの手作業は消す。
- docs/journal/README.mdのOpen節を削除し、Migration to cmux-taskqの記述どおり一覧の正を`cmux-taskq list`にする。
- SVの各操作はCLIコマンド単位で書き、後でruntimeへ移せる形にする。`read-screen`は当面の一次情報として認める。

完了条件: 新しいAGENTS.mdの手順だけで、次のtask（011以降）をSV sessionから登録・実行・着地できる。

## Log

### 2026-09-22 claude (run session, task 2, run 89ee1a3c-f9c9-4c45-87d5-2d054b06bc41)

- cmux-taskq の run session として開始。base は main `88012c4`。AGENTS.md、docs/journal/README.md、012 の Procedure と Log、README の supervise / recover / integrate 節、plans/current.md ステップ9、frontmatter.md を読了
- AGENTS.md を全面的に書き換えた。「セッション開始時に読む」は `cmux-taskq list` / `show` とジャーナル（`queue_task` で紐づく）に、「作業中」「タスクを閉じるとき」「コミット」は run branch と `integrate` 前提に直した。「変更後に必ず通す」のコマンド4行はそのまま
- 役割節は「SV と run session」にし、SV の操作を準備・登録・起動・監視・レビューと着地・needs_session・失敗と中断の7表に分け、1 行 1 CLI コマンドで書いた。`.worktrees/<NNN>-<slug>` と `TASKQ-<NNN>` workspace の手順、完了マーカー、`planned` ジャーナルの選定は削除した
- CLI の表記は実機で確認した。`cmux send-key` のキー名は `enter`（小文字）、`cmux workspace create` は `--name` / `--cwd` / `--command`、run の workspace 名は `taskq <task-id> <run-id>`（`src/infrastructure/adapters.rs`）。`needs_session` の解消指示は README の Land a run on main の文言を `cmux send` 1 行に写した
- docs/journal/README.md の Open 節を削除した。Rules は Open への追加・除外・並び順を前提にしていたので、その4項目だけ `cmux-taskq add` / `queue_task` / `cmux-taskq list` を正とする文に直し、他の Rules と Migration の本文は残した。Migration の「このOpen節は削除する」は削除済みの記録にした。M2以降の段落は Migration の末尾に残した
- docs/plans/current.md は触っていない。ステップ9はまだ 013・014 が残るので状態は変わらない。Ordering の「012・013・014・019 は draft で置く」は移行前の記述として残る
- subagent review（Opus）: CLI・README との矛盾なし。指摘4件（`show` は receipt の中身ではなくパスを返す、supervise の workspace で bare `cmux-taskq` が動くのは PATH のため、README Migration の「起動しない」が現在形、Rules 1 と 2 が同じ遷移を二重に書く）をすべて反映した
- 検証: task の verify 4 コマンド、`cargo fmt --all --check`、`cargo test --locked`、`cargo clippy --locked --all-targets -- -D warnings`、`cargo llvm-cov --locked --fail-under-lines 80` を worktree で実行。結果は receipt に書いた
- `cargo llvm-cov` の1回目は `--test cli` の途中で exit 101 になった（`cargo test --locked` 同時実行直後、同じ target の再ビルド中）。個別の失敗 test は出力に残っていない。再実行では全 test が通り TOTAL 行 87.10%。Markdown だけの変更なので flake と判断し、原因調査は task にしない

## Result

- AGENTS.md は常駐 SV session が cmux-taskq を操作する手順になった。登録は `add` / `ready`、実行は専用 workspace の `supervise --parallel 4`、完了確認は `read-screen` と `show`、着地は run branch の差分レビューのあと `integrate`、`needs_session` は run の worktree で `claude --resume <run-id>` を開いて rebase・解消・再検証・receipt 書き直しを指示し、`/exit` させてから `integrate` し直す。着地後に `git push origin main`。失敗・中断は `doctor` / `recover` / `ready` と `cmux workspace close`
- docs/journal/README.md に Open 節はなく、一覧・実行順・依存・状態は `cmux-taskq list` が正
- 完了条件（新しい AGENTS.md の手順だけで次の task を登録・実行・着地できる）は、この run 自体が 012 の Procedure と同じ CLI 列で流れていることで満たす。013 以降が新しい AGENTS.md だけで流れることは 013 で確認する

## Promoted

- AGENTS.md（SV の各操作の CLI コマンド表と run session の規則）。design / ADR への昇格はなし。SV の操作を runtime へ移す件は plans/current.md の After first dogfooding に既にある
