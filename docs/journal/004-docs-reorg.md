---
id: journal-004
type: journal
title: Docs reorganization and task journals
status: done
created: 2026-09-22
updated: 2026-09-22
plan_step: null
queue_task: null
related:
  - docs-index
---

# 004: Docs reorganization and task journals

## Goal

セッションやエージェントが替わっても作業状態を引き継げ、作業中の備忘を残す場所がある。`docs/README.md` の分類とジャーナルの運用が一致し、ドッグフーディング開始時にopenなジャーナルを `cmux-taskq add` へ移行できる。

## Log

### 2026-09-22 08:30 claude

- 発端: `plans/claude-lifecycle-spike.md` の置き場所への違和感と、エージェント間の引き継ぎ・備忘の置き場がないこと
- 検討経過: notes/ + handoff.md → 日付単位のjournal → タスク単位のjournal に収束。spike文書は「タスク001のジャーナル」と再解釈
- 決定: `docs/journal/NNN-slug.md`、frontmatterは既存仕様に `type: journal` を追加。一覧はjournal READMEのOpen節、状態はopen/doneのみ。ドッグフーディング開始時にキューへ移行し、以後の一覧は `cmux-taskq list`
- notes/ は保留。複数タスクで同じ事実を参照するようになったら作る
- `AGENTS.md` にルールを置き、`CLAUDE.md` は `@AGENTS.md` で参照
- 作成: template、README、001（spike移動）、002、003、004、`AGENTS.md`、`CLAUDE.md`。`docs/README.md`、`frontmatter.md`、`plans/README.md`、root README のリンク更新

## Result

`docs/journal/` を導入し、spike文書を001として移動、002〜004を作成。`AGENTS.md` と `CLAUDE.md`（`@AGENTS.md`）を追加。`docs/README.md`、`frontmatter.md`、plans、root README のリンクと分類を更新。相対リンクの検証と `git diff --check` を通過。

## Promoted

- [docs/README.md](../README.md): 4分類と更新ルール
- [docs/frontmatter.md](../frontmatter.md): `journal` 型
- [AGENTS.md](../../AGENTS.md): セッション開始・作業中・閉じるときの手順
