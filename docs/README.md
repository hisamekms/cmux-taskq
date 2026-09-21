---
id: docs-index
type: design
title: Documentation guide
status: current
created: 2026-09-21
updated: 2026-09-21
last_verified: 2026-09-21
tags:
  - documentation
---

# Documentation guide

cmux-taskqの文書は、決定、現在の設計、実装計画を分けて管理する。

## 文書の種類

- `adr/`: なぜその決定をしたか。重要な決定は必ず追加し、既存のADRを書き換えない。
- `design/`: 現在の実装がどうなっているか。コードを読む前のショートカットとして保守する。
- `plans/`: これから何を作るか。完了した計画は`archive/`へ移す。

## Frontmatter

全てのdocs文書は [frontmatter仕様](frontmatter.md) に従う。

## 更新ルール

アーキテクチャ全体に影響し、手戻りが大きい決定は先にADRを作る。実装を変更したら関連するdesign文書の`updated`と`last_verified`を確認する。計画は実際の依存関係と完了条件を更新する。
