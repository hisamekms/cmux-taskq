---
id: adr-0020
type: adr
title: repositoryの移動はrebindサブコマンドでqueueの束縛を付け替える
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
  - adr-0015
  - adr-0017
  - design-persistence
  - design-supervisor-lifecycle
---

# ADR-0020: repositoryの移動はrebindサブコマンドでqueueの束縛を付け替える

> **置き換え済み（2026-09-26）**: このADRの決定は現在有効ではない。現行の決定は[ADR-0053](0053-queue-in-data-dir-run-paths-from-queue-and-rebind.md)を読む。

## Context

queueは`queue_repository.git_common_dir`でGit common directoryに束縛される（[ADR-0006](0006-queue-per-repository.md)）。この行は`init`（repositoryから解決したqueue）か最初の`supervise`（`--db`のqueue）が`bind_repository`で書くだけで、`bind_repository`は別のrepositoryへの束縛を拒否し、付け替えを暗黙に行わない。`assert_repository`はrepositoryから解決した全コマンドの冒頭で束縛を検査し、`supervise`と`integrate`も一致を要求する。

そのためrepositoryを動かす（ディレクトリの移動、GitHubのrenameに伴うghqの移動）と、queueは全コマンドで`queue is bound to another Git repository`になる。[ADR-0015](0015-rename-to-dagq.md)の切り替え手順4は、これを「DBは手で直さない」の唯一の例外としてsqlite3の`UPDATE`で乗り切った。実際にはauto modeの分類器がsqlite3を拒否し、ユーザーが手でSQLを打った。ADR-0015自身が「rebind用のサブコマンドを足すなら別taskにする」と書いている。

repositoryを動かすとqueueのpathも変わる。repositoryから解決したqueueのディレクトリ名はcanonicalなcommon directoryのSHA-256から決まるので、移動後のcheckoutは存在しない新しいqueueに解決される。queueディレクトリの移動そのものは[ADR-0017](0017-resolve-run-paths-from-the-queue-directory.md)でDBを書き換えずに扱えるようになった。

Git側では、main working treeを動かすとrunのlinked worktreeの`.git`ファイルが旧repositoryの`.git/worktrees/<name>`を指したままになり、worktreeの中で`git status`も通らなくなる。使い捨てrepositoryで確かめた挙動（git 2.39）: 新しいrepositoryで`git worktree repair <worktree>`を実行すると`.git`ファイルが直り、queueディレクトリも動かしていれば続けて同じコマンドでrepository側の`gitdir`も直る。どちらの順で動かしても、repairの後は`worktree remove`と`branch -D`が通る。

## Decision

1. **束縛を変える明示的なサブコマンド`rebind [--repo PATH]`を足す。** 開いたqueueの`queue_repository`を、cwd（または`--repo`）のrepositoryのcanonicalなGit common directoryに付け替える。`rebind`だけはopen直後の`assert_repository`を通らない。`bind_repository`は変えず、`init`・`supervise`・その他のコマンドは今までどおり別のrepositoryへの束縛を拒否する。暗黙の付け替えはどこでも起きない。束縛が無い`--db`のqueueは`rebind`で束縛される。
2. **出力**: `outcome`（`rebound`か、同じrepositoryなら`unchanged`）、`previous_git_common_dir`と`git_common_dir`（旧と新）、`db`、`queue_dir`、`repository_queue_dir`（新しいrepositoryから解決されるqueueのディレクトリ）、`move_to`（`queue_dir`と違うときだけ`repository_queue_dir`、同じならnull）、`worktrees`（下の4）。
3. **記録**: 変わったときだけ、queueの`logs/rebind.jsonl`に`{"at", "previous_git_common_dir", "git_common_dir", "binary_version"}`を1行追記し、queueディレクトリに`repository`ファイルがあれば新しいpathに書き換える。`run_events`には書かない。`run_events`は`task_id`か`goal_id`を必須とするCHECKを持ち、queue単位のeventを入れるにはschemaを変えるmigrationが要る。migrationは`user_version`を上げて古い固定バイナリを`unsupported queue schema version`で止める（ADR-0017と同じ理由）ので、今回はschemaを変えない。
4. **worktreeのGit管理情報を直す**: 付け替えの後、DBのrunのうちworktree（queueの今の`runs/`から解決したpath）が残っているものすべてに、新しいrepositoryで`git worktree repair <worktree>`を実行する。失敗しても`rebind`は失敗せず、`worktrees`の各項目に`repaired: false`と`error`を出す。これで`needs_session`のrunを再開するsessionもworktreeでGitを使える。queueディレクトリをこの後で動かす場合は`integrate`の`land`がもう一度repairする（ADR-0017）。
5. **走行中のsupervisorがいれば拒否する。** 登録された`supervisors`のうちPIDが生きているもの（heartbeatが古いhung状態を含む）が1つでもあれば、`down --wait`で止めるよう求めて失敗する。PIDの死んだ登録（killされた残り）は無視する。着地中の`integrate`（`integrating`のrunで、leaseのPIDが生きている）も同じく拒否する。どちらも旧repositoryのpathを持って動いているため。
6. **手順（repositoryとqueueを動かす順番）**: README の「Move the repository or the queue」に書く。推奨は次の順。
   1. 旧checkoutで`down --wait`（走行中のrunを終わらせ、旧hashのLaunchAgentを外す）。`needs_session`のrunは先に解消して着地させる（ADR-0017）。
   2. repositoryを動かす。新しいcheckoutで`init`を打たない（新しいhashの場所に空のqueueができ、移すqueueとぶつかる）。
   3. 新しいcheckoutで旧queueを`--db`（pluginのlauncherなら`DAGQ_DB`）で指して`rebind`する。拒否されるならここで止まり、何も動いていない。
   4. `rebind`の`move_to`へqueueディレクトリを丸ごと動かす（ADR-0017の移動と同じ）。
   5. 新しいcheckoutで`list`・`status`を確かめ、`up`で戻す。

   逆の順（queueディレクトリを新しいhashの場所へ先に動かし、フラグ無しで`rebind`）でも動く。その間は`init`を含む全コマンドが束縛の不一致で拒否する。推奨を`rebind`先にするのは、拒否されうる操作を何も動かす前に済ませられ、動かす先を`move_to`がそのまま教えるから。
7. **DBの直接操作の例外は無くなる。** ADR-0015の手順4の直接SQLはこれ以降使わず、repositoryの移動は`rebind`で扱う。AGENTS.mdの「DBは手で直さない」に例外は無い。ADR-0015自体は書き換えない。

## Alternatives

- **`init`に`--rebind`を足す**: `init`は「作成またはmigrate」でrepository queueの新しい場所に空のDBを作りうる。付け替えを別コマンドにしておくほうが、誤って空のqueueを作る経路と混ざらない。
- **束縛の不一致を検出したら自動で付け替える**: `bind_repository`の「暗黙に付け替えない」を崩し、hash衝突やコピーしたデータディレクトリを黙って受け入れてしまう（ADR-0006が束縛を置いた理由そのもの）。
- **旧queueを探して自動で移す（`rebind`がqueueディレクトリも動かす）**: 旧pathの`repository`ファイルから探せはするが、WALファイルを含むディレクトリの移動をruntimeが開いたDBの下で行うことになり、失敗時に半端な状態が残る。移動は人が`mv`で丸ごと行い、`rebind`は行き先を示すだけにした。
- **queue単位のevent表を足すmigration**: `rebind`を`events`に出せるが、schema versionが上がり本番queueを開いたままの古い固定バイナリを止める。束縛の変更は稀で、ログファイルで足りる。
- **supervisorが走っていても付け替える**: 走っているsupervisorは旧repositoryのpathで`main`を読み、worktreeを作り続けるので、束縛だけ変えても動作は変わらず、次の検査で壊れる。拒否して止めてもらうほうが分かりやすい。

## Consequences

- repositoryの移動にDBの直接操作は要らない。`rebind`の出力に旧と新のcommon directoryが出て、`logs/rebind.jsonl`に履歴が残る。
- `rebind`を打つまでは、移動後のcheckoutから旧queueを使うコマンドは今までどおり束縛の不一致で落ちる（`--db`で開いたqueueは`supervise`と`integrate`だけが落ちる）。
- `events`・`watch`には`rebind`は出ない。見るときは`logs/rebind.jsonl`を読む。
- `task_runs.repo_path`は書き換えない。使い道はrunのcmux workspace名の`<repo>`だけで、既に作ったworkspaceの名前は変わらない。
- 旧hashのLaunchAgentは`down`を旧checkoutで打たないと残る。`rebind`はそれを外さないので、手順1を飛ばしたときは`launchctl bootout gui/<uid>/com.dagq.<旧hash>`とplistの削除が要る。
