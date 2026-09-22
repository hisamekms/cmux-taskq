---
id: journal-index
type: design
title: Task journals
status: current
created: 2026-09-22
updated: 2026-09-22
last_verified: 2026-09-22
tags:
  - documentation
---

# Task journals

タスク単位の作業記録。セッションやエージェントが替わっても、そのタスクの経過を1ファイルで追えるようにする。テンプレートは [000-template.md](000-template.md)。

## Rules

- 決まったタスクは`cmux-taskq add`で登録する。人が関わるタスクは次の連番でジャーナルをScopeだけ書いて作り、返ったIDを`queue_task`に書く。
- `status: draft`のジャーナルは登録待ち。登録したら`planned`、セッションが触り始めたら`open`にする。SVがジャーナルを直接起動することはなく、実行は常に`cmux-taskq supervise`が行う。
- 作業中はLogに追記する。一時的なpath、workspace番号、制限の復活時刻など、他の文書に書くほどでもない備忘はここに書く。
- 閉じるときにResultとPromotedを書き、`status: done`（または`abandoned`）にする。
- 一覧・実行順・依存・状態は`cmux-taskq list`と`show ID`が正。ジャーナルはready/draftのような状態を持たない。状態機械はキューだけが持つ。
- ステップの順序と完了条件は[plans/current.md](../plans/current.md)が持つ。ジャーナルは1ステップを複数タスクに割る単位。

## Migration to cmux-taskq

ドッグフーディング（ステップ9、journal 012）を始めるとき、draftのジャーナルを順に`cmux-taskq add`へ登録し、返ったIDを`queue_task`に書き戻す。登録は016・017・018・010がdoneになってから行い、それまでSVはdraftを起動しなかった。以後の一覧は`cmux-taskq list`が正となり、Open節は削除した（2026-09-22、[019](019-replace-interim-workflow.md)）。doneのジャーナルはDBへ入れない。

移行後は、キューが一覧・状態・依存・run履歴を持ち、`<db>.runs/<run-id>/`がエージェント実行のprompt・log・receiptを持つ。ジャーナルは人が関わったセッションの経過と判断を残す場所として続ける。エージェントが完走したタスクにはジャーナルを作らなくてよい。

M2以降（Codex provider、配布、Python版からの移行）は[plans/current.md](../plans/current.md)のAfter first dogfoodingに留め、まだタスクに割らない。
