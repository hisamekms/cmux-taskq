---
id: design-supervisor-lifecycle-rebind
type: design
title: "`rebind`"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0020
---

# `rebind`

`dagq rebind [--repo REPO]`はrepositoryを移動した後に、開いたqueueの`queue_repository`を`--repo`（既定はcwd）の`GitRepository::inspect`が返すcanonicalなcommon directoryに付け替える（[ADR-0020](../../adr/0020-rebind-queue-to-a-moved-repository.md)）。repositoryから解決したqueueでopen直後の`assert_repository`を通らない唯一のコマンドで、`bind_repository`を使う`init`・`supervise`は今までどおり別のrepositoryを拒否する。

1. **拒否**: `supervisors`の登録のうちPIDが生きているものがあれば失敗する（heartbeatの古いhungも含む。PIDの死んだ登録は無視する）。`integrating`のrunのleaseのPIDが生きていれば（着地中の`integrate`）失敗する。どちらも束縛は変えない。
2. **付け替え**: `rebind_repository`が1トランザクションで旧値を読み、新しいcommon directoryをupsertする。旧値と同じなら`outcome: unchanged`。
3. **記録**: 変わったときだけ`<queue dir>/logs/rebind.jsonl`に1行追記し、`<queue dir>/repository`があれば新しいpathに書き換える。`run_events`には書かない。
4. **worktreeのrepair**: runのうちworktree（queueの今の`runs/`から解決したpath）が残っているものに、新しいrepositoryの主working treeで`git worktree repair <worktree>`を実行する。main working treeの移動で壊れた`.git`ファイルが直る。失敗は`worktrees[].error`に出すだけで`rebind`は成功する。
ユースケースはapplication層の`src/application/rebind.rs`の`rebind(Rebind, RebindTarget)`で、queueは`RunStore`（`rebind_repository`・`all_runs`を含む）、worktreeのrepairは`Repository`、`rebind.jsonl`の追記・`repository`ファイルの書き換え・worktreeの有無・`move_to`の比較のためのcanonicalな解決は`RunFiles`、PIDの生死は`ProcessControl`、`at`は`Clock`越しに行う。queueのpath（`db`、`queue_dir`、`logs/`、`repository`ファイル）と新しいcommon directory、そこから解決されるqueueディレクトリは`RebindTarget`として値で受ける。`src/compose.rs`の`OneShot::rebind(db, repo)`がqueueとrepositoryを開き、`QueueLocation`とdata homeからそれらを求めて呼ぶ入口で、`runtime::rebind`として再公開する。

5. **出力**: `previous_git_common_dir`、`git_common_dir`、`db`、`queue_dir`、`repository_queue_dir`（新しいcommon directoryから解決されるqueueディレクトリ。data homeが決まらなければnull）、`move_to`（それが`queue_dir`と違うときだけ。data homeが決まらないときもnull。行き先が既にあるかは見ないので、手順で「存在しないこと」を求める）、`worktrees`。
