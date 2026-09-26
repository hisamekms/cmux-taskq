---
id: adr-0017
type: adr
title: runのqueue配下のpathは読むたびにqueueディレクトリとrun IDから解決する
status: superseded
created: 2026-09-23
updated: 2026-09-23
accepted_on: 2026-09-23
superseded_by: adr-0053
superseded_on: 2026-09-26
owners:
  - hisamekms
tags:
  - runtime
  - persistence
  - operations
related:
  - adr-0006
  - adr-0008
  - adr-0015
  - design-persistence
  - design-domain-model
  - design-supervisor-lifecycle
---

# ADR-0017: runのqueue配下のpathは読むたびにqueueディレクトリとrun IDから解決する

> **置き換え済み（2026-09-26）**: このADRの決定は現在有効ではない。現行の決定は[ADR-0053](0053-queue-in-data-dir-run-paths-from-queue-and-rebind.md)を読む。

## Context

`task_runs`の`run_dir`・`worktree_path`・`receipt_path`・`log_path`は、provision時に`runs_dir(db)`から組み立てた絶対pathの文字列で保存され、`integrate`・supervisorの検証・`doctor`・`recover`・`show`・後続runのpromptはその文字列をそのまま使っていた。queueディレクトリ（[ADR-0006](0006-queue-per-repository.md)）を別の場所へ動かすと、これらは旧pathを指したままになり、未完了のrunと`awaiting_integration`のrunはworktreeもreceiptも見つけられなくなる。[ADR-0015](0015-rename-to-dagq.md)の切り替えでは、この制約のために移動前にtask 45と48を着地させて回避した。

run配下の配置は最初のsupervisor実装から変わっていない: `<queue dir>/runs/<run-id>/`に`worktree/`、`receipt.json`、`claude.debug.log`があり、idle markerとClaudeのsettingsは既に`run_dir`から導出している。

Git側にも絶対pathがある。linked worktreeの`.git`ファイルはrepositoryの`.git/worktrees/<name>`を指し（repositoryは動かないので有効なまま）、repository側の`.git/worktrees/<name>/gitdir`はworktreeの旧pathを指す。使い捨てrepositoryで確かめた挙動（git 2.39）:

- 動かしたworktreeの中での`git status`・`rebase`・`rev-parse`はそのまま動く
- `git worktree list`は旧pathを`prunable`と表示する
- 新しいpathでの`git worktree remove`は`is not a working tree`で失敗し、branchもworktreeに checkout されたまま扱われるので`git branch -D`も消せない
- `git worktree repair <新しいpath>`をrepositoryで実行すると`gitdir`が直り、以後の`remove`と`branch -D`が通る。直っているworktreeに対しては何もしないで0で終わる
- `git worktree prune`（や期限後の`git gc`）は`prunable`の管理情報を消し、そうなるとworktreeの`.git`ファイルの参照先が無くなって`repair`でも戻せない

## Decision

1. **queue配下のpathはDBの値を信用せず、run IDとqueueディレクトリから解決する。** `SqliteQueue`は開いたDBのpathから`runs_dir`を決め、`task_runs`の行を読むたびに`run_dir`・`worktree_path`・`receipt_path`・`log_path`を`<runs_dir>/<run-id>/…`に置き換える（`TaskRun::relocated`、配置は`RunPaths`の1か所）。値がnull（まだprovisionしていないrun）ならnullのまま。`repo_path`はqueueではなくrepositoryを指すので対象外（repositoryの移動と束縛の付け替えは別のtaskのrebindが扱う）。
2. **列とその書き込みは残し、schemaは変えない。** provisionは今までどおり絶対pathを書く。列の値は「claim時にどこへ作ったか」の記録になり、読み出しでは使わない。migrationで相対pathへ書き換える案を採らないのは、どのmigrationも`user_version`を上げて古い固定バイナリを`unsupported queue schema version`で止めるのに対し、読み出し時の解決なら既存の絶対pathの行をそのまま読めて、古いバイナリと新しいバイナリが同じDBを読んでも壊れないから。既存queueの移行作業は無い。
3. **worktreeのGit管理情報は`integrate`が直す。** `land`はworktreeの存在を確かめた直後に、repositoryの主working treeで`git worktree repair <worktree>`を実行する。移動していなければno-opなので毎回実行する。これで着地後の`worktree remove`と`branch -D`が新しいpathで通る。supervisorの検証（`git -C <worktree>`の読み取り）は`.git`ファイル経由で動くのでrepairしない。
4. **移動の手順**: supervisorを止め（`down --wait`で走行中のrunを終わらせる）、queueディレクトリの中身（`queue.db`とWALファイル、`runs/`、`logs/`、`repository`）をまとめて移し、移した先を指して`up`し直す。走行中のrunのcmux workspaceのcommandは`runner`と`--db`を旧pathで持つので、runを走らせたまま動かすことはサポートしない。`needs_session`のrunは移動前に解消して着地させる（再開するClaude sessionは旧worktreeのcwdと、promptに書かれた旧receipt pathを覚えているので、移動後の`--resume`が同じsessionを見つけられる保証も、新しい場所にreceiptを書く保証も無い）。移動からintegrateまでの間に`git worktree prune`を打たない（打つと管理情報が消えて`repair`で戻らない）。動かしたworktreeを手で片付けるときは先に`git worktree repair <path>`を打つ。

## Alternatives

- **列を相対pathに書き換えるmigration**: 読み出しで`runs_dir`と結合するのは同じだが、schema versionが上がり、本番queueを開いたままの古い固定バイナリを止める（AGENTS.mdが`target/`のバイナリを本番queueに使わない理由と同じ）。新しい行と古い行で意味が変わる列を持つことにもなる。
- **列を削除して導出のみにする**: 最終形としてはきれいだが、テーブルの作り直しが要り、上と同じくschema versionを上げる。記録として残す害は無いので見送った。
- **移動を検出するコマンド（`relocate`）でDBの値を書き換える**: 移動のたびに人が打つ手順が増え、打ち忘れると今と同じ壊れ方をする。読み出しで解決すれば手順が要らない。
- **worktree repairを手順だけに書く**: `integrate`の後始末（`cleanup_failed`）が壊れ、手作業が残る。repairは冪等で安いので`land`で吸収した。

## Consequences

- queueディレクトリを動かしても、`show`・`status`・`doctor`・`recover`・`integrate`・検証・promptの前任者summaryが新しい場所のpathを返し、使う。絶対pathが入った既存の行もそのまま解決し直される。
- `runs/<run-id>/`の配置を変えるときは`RunPaths`と、既存runの配置を同時に扱えるかを考える必要がある（今は配置が1通りしかないので、run IDだけから導ける）。
- DBの`run_dir`等の列は、移動後は実際の場所と一致しない。調べるときは`show`を使い、DBを直接読まない。
- `run_events`の`payload`に写したpath（`worktree_created`、`verification_command`の`log_path`など）は記録時点のままで、書き換えない。
- `git worktree prune`を移動後・着地前に打つと、そのrunのworktreeは`integrate`できなくなる（`repair`が失敗し、`integrate`はmainを動かす前に止まってrunを元のstatusに戻す）。
- DBのpathを正規化してから`runs/`を決めるので、相対pathやsymlinkの`--db`で開いても、`supervise`が作った場所と同じ絶対pathが返る。
