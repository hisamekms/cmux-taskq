---
id: journal-018
type: journal
title: Merge queue with rebase, re-validation, and squash landing
status: planned
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
- ADRを追加し、`domain-model.md`・`persistence.md`・`supervisor-lifecycle.md`・READMEを更新する。

完了条件: 衝突なしのrunがClaudeなしで着地し、衝突したrunがセッションでの解消後に着地し、いずれもmainが直線で1 task = 1 commitになることがテスト（e2e含む）で確認できる。着地前の`result_commit`と着地後のmain headのtreeが一致する。

## Log

## Result

## Promoted
