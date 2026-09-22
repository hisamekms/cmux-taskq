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

- 決まったタスクは次の連番で`status: planned`のジャーナルをGoalだけ書いて作り、下のOpenに追加する。着手したら`open`にする。
- 作業中はLogに追記する。一時的なpath、workspace番号、制限の復活時刻など、他の文書に書くほどでもない備忘はここに書く。
- 閉じるときにResultとPromotedを書き、`status: done`（または`abandoned`）にしてOpenから外す。
- Openの並びが実行順。依存は末尾に一言で書く。ready/draftのような状態は持たない。状態機械はキューだけが持つ。
- ステップの順序と完了条件は[plans/current.md](../plans/current.md)が持つ。ジャーナルは1ステップを複数タスクに割る単位。

## Migration to cmux-taskq

ドッグフーディング（ステップ6）を始めるとき、openのジャーナルを順に`cmux-taskq add`へ登録し、返ったIDを`queue_task`に書き戻す。以後の一覧は`cmux-taskq list`が正となり、このOpen節は削除する。doneのジャーナルはDBへ入れない。

移行後は、キューが一覧・状態・依存・run履歴を持ち、`<db>.runs/<run-id>/`がエージェント実行のprompt・log・receiptを持つ。ジャーナルは人が関わったセッションの経過と判断を残す場所として続ける。エージェントが完走したタスクにはジャーナルを作らなくてよい。

## Open

2. [006 workspace close](006-workspace-close.md) — after 005
3. [007 session exit request](007-session-exit-request.md) — after 005
4. [008 integration confirm](008-integration-confirm.md) — after 005
5. [009 doctor / recover](009-doctor-recover.md) — after 005
6. [015 e2e happy path](015-e2e-happy-path.md) — after 006, 008。実cmux + stub agentの自動e2e
7. [010 failure path smoke](010-failure-path-smoke.md) — after 006, 009, 015。ステップ4の完了
8. [011 Claude Code plugin](011-claude-code-plugin.md) — after 008。ステップ5
9. [012 dogfood: independent task](012-dogfood-independent-task.md) — after 011。ここでopenなジャーナルをキューへ移行する
10. [013 dogfood: dependent tasks](013-dogfood-dependent-tasks.md) — after 012
11. [014 dogfood: failure and recovery](014-dogfood-failure-recovery.md) — after 013。ステップ6とM1の完了

M2以降（Codex provider、配布、Python版からの移行）は[plans/current.md](../plans/current.md)のAfter first dogfoodingに留め、まだタスクに割らない。
