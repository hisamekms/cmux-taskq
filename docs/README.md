---
id: docs-index
type: design
title: Documentation guide
status: current
created: 2026-09-21
updated: 2026-09-22
last_verified: 2026-09-22
tags:
  - documentation
---

# Documentation guide

cmux-taskqの文書は、決定、現在の設計、実装計画、作業記録を分けて管理する。エージェント向けの運用ルールはrepo rootの[AGENTS.md](../AGENTS.md)にある。

## 文書の種類

- `adr/`: なぜその決定をしたか。重要な決定は必ず追加し、既存のADRを書き換えない。
- `design/`: 現在の実装がどうなっているか。コードを読む前のショートカットとして保守する。
- `plans/`: これから何を作るか。ステップの順序と完了条件を持つ。完了した計画は`status: completed`にして残す。
- `journal/`: タスク単位の作業記録。セッションやエージェントをまたぐ引き継ぎと、他の文書に書くほどでもない備忘の置き場。閉じるときに得られた事実をdesign/ADRへ昇格させる。

## Frontmatter

全てのdocs文書は [frontmatter仕様](frontmatter.md) に従う。

## 更新ルール

アーキテクチャ全体に影響し、手戻りが大きい決定は先にADRを作る。実装を変更したら関連するdesign文書の`updated`と`last_verified`を確認する。計画は実際の依存関係と完了条件を更新する。タスクを始めるときにジャーナルを作り、作業中に追記し、閉じるときにResultとPromotedを書く。
