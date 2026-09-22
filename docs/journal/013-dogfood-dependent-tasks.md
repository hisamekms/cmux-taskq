---
id: journal-013
type: journal
title: Dogfooding: dependent tasks A then B
status: draft
created: 2026-09-22
updated: 2026-09-22
plan_step: 9
queue_task: [3, 4, 5]
depends_on_journal: [19]
related:
  - plan-rust-runtime-mvp
---

# 013: Dogfooding: dependent tasks A then B

## Goal

[plans/current.md](../plans/current.md) ステップ9。A → Bの依存taskと、依存のないCを登録し、依存解放が着地に紐づくこと、依存のないtask同士は並列に走ることを確認する。

A・B・Cの実taskはSVが[012のProcedure](012-dogfood-independent-task.md)に従って選び、登録内容をこのジャーナルのLogに書く。

完了条件: AとCが同時に走り、Aの実行成功（`awaiting_integration`）だけではBが始まらず、Aの着地後にBがAの変更を含むmainから作られたworktreeで始まる。

## Log

### 2026-09-22 13:49 claude (SV)

012の手順4に従い、010のFoundから選んだ3件をT3〜T5として登録する。いずれもT2（019）の後。

- A（T3、依存なし）: 非0終了で `failed` になったrunに `last_error` が無い（010 Found 1）。`finish_supervision` で終了コードが非0のとき `last_error` に `session exited with code N` を書き、`show` に出ることをtests/runtime.rsで確認する。verify: fmt / test / clippy
- B（T4、`--depends-on A`）: `last_error` の意味（検証拒否・runtime error・cleanup失敗・非0終了のどれが何を書くか）を `docs/design/domain-model.md` とREADMEに書く。Aの挙動を文書化するのでAに依存。verify: `grep -q 'session exited' docs/design/domain-model.md`
- C（T5、依存なし）: `plugins/claude-taskq/skills/taskq-run/SKILL.md` に、実Claudeは新worktreeごとに信頼確認で止まるのでoperatorが応答すること（Found 3）と、失敗・中断したrunのworkspaceは `cmux workspace close` で閉じること（Found 5）を書く。verify: `cargo test --locked --test plugin`

## Result

## Promoted
