---
id: journal-013
type: journal
title: Dogfooding: dependent tasks A then B
status: done
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

### 2026-09-22 15:10 claude (SV)

- T2（019）の着地直後のpollで、AとCが同時にclaimされた: A = run `a5076f4a`（workspace:122）、C = run `e04c8ada`（workspace:123）。B（T4）は `ready` のまま候補に出ない（Aが `in_progress`）
- C が先に `awaiting_integration`（検証: `cargo test --test plugin`、grep）。差分はskillのみ、subagent review済み。`integrate 5` → `7e184d8`、push。receiptの注記: READMEとsupervisor-lifecycle.mdの「`doctor` が失敗runのworkspace IDを出す」は誤り（doctorは未完了runのみ）→ 後続task候補
- A は実行中。Aの着地後にBがAの変更を含むmainから始まることを確認する

### 2026-09-22 15:40 claude (SV)

- A（run `a5076f4a`）が `awaiting_integration`。runtime変更なので着地前にrun worktreeで llvm-cov（87.11%）と e2e 2件を実行。`integrate 3` → `e862843`、push
- 着地の次のpollでB（T4）がclaimされ、run `033dea4b` の base commit = `e862843`（Aの着地commit）。Aが `awaiting_integration` の間はBが候補に出なかった
- B が `awaiting_integration` → `integrate 4` → `dfaac7b`、push。mainは seed から 1 task = 1 commit の直線（88012c4, fedff3d, 7e184d8, e862843, dfaac7b とSVのdocsコミット）

## Result

AとCが同時に走り（並列claim）、Aの実行成功だけではBが始まらず、Aの着地後にBがAの変更を含むmain（`e862843`）から作られたworktreeで始まった。3件とも人の介入なしにreceipt→検証→closeまで進み、`integrate` で着地した。DBの手修正なし。

## Promoted
