---
id: journal-019
type: journal
title: Replace the interim SV/worker workflow with cmux-taskq operation
status: draft
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

## Result

## Promoted
