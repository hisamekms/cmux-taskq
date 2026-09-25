---
id: docs-index
type: design
title: Documentation guide
status: current
created: 2026-09-21
updated: 2026-09-25
last_verified: 2026-09-25
tags:
  - documentation
---

# Documentation guide

dagqの文書は、決定、現在の設計、実装計画を分けて管理する。タスクの経過と状態はdagqのキュー（`dagq show ID`のrun履歴とreceipt）が持つ。エージェント向けの運用ルールはrepo rootの[AGENTS.md](../AGENTS.md)にある。

## 文書の種類

- `adr/`: なぜその決定をしたか。重要な決定は必ず追加し、既存のADRを書き換えない。`accepted`のADRだけが現在の決定で、`superseded`なら`superseded_by`を辿り、`deprecated`は後継なしの廃止（日付は`deprecated_on`）。決定を変えるときは古いADRを丸ごと置き換える統合ADRを書く（[ADR-0042](adr/0042-adr-is-superseded-whole-and-deprecation-date-is-deprecated-on.md)、索引は[adr/README.md](adr/README.md)）。
- `design/`: 現在の実装がどうなっているか。コードを読む前のショートカットとして保守する。
- `plans/`: これから何を作るか。ステップの順序と完了条件を持つ。完了した計画は`status: completed`にして残す。

人の判断はADR、Goalの記述、`Task.context`、receiptの`summary`に残す。

## Frontmatter

全てのdocs文書は [frontmatter仕様](frontmatter.md) に従う。

## 更新ルール

アーキテクチャ全体に影響し、手戻りが大きい決定は先にADRを作る。実装を変更したら関連するdesign文書の`updated`と`last_verified`を確認する。計画は実際の依存関係と完了条件を更新する。
