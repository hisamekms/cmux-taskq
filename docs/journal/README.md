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

- タスクを始めるとき、次の連番で`status: open`のジャーナルを作り、下のOpenに追加する。
- 作業中はLogに追記する。一時的なpath、workspace番号、制限の復活時刻など、他の文書に書くほどでもない備忘はここに書く。
- 閉じるときにResultとPromotedを書き、`status: done`（または`abandoned`）にしてOpenから外す。
- Openの並びが実行順。依存は末尾に一言で書く。ready/draftのような状態は持たない。状態機械はキューだけが持つ。
- ステップの順序と完了条件は[plans/current.md](../plans/current.md)が持つ。ジャーナルは1ステップを複数タスクに割る単位。

## Migration to cmux-taskq

ドッグフーディング（ステップ6）を始めるとき、openのジャーナルを順に`cmux-taskq add`へ登録し、返ったIDを`queue_task`に書き戻す。以後の一覧は`cmux-taskq list`が正となり、このOpen節は削除する。doneのジャーナルはDBへ入れない。

移行後は、キューが一覧・状態・依存・run履歴を持ち、`<db>.runs/<run-id>/`がエージェント実行のprompt・log・receiptを持つ。ジャーナルは人が関わったセッションの経過と判断を残す場所として続ける。エージェントが完走したタスクにはジャーナルを作らなくてよい。

## Open

（なし。次はステップ4のタスクを起こす）
