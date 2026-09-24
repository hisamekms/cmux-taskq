---
id: adr-NNNN
type: adr
title: Decision title
status: proposed
created: YYYY-MM-DD
updated: YYYY-MM-DD
# accepted_on: YYYY-MM-DD      # accepted にする変更で足す。updated は変えない
# supersedes: [adr-NNNN]       # 置き換える ADR があれば。置き換えは丸ごと（ADR-0035）
# superseded_by: adr-NNNN      # superseded にするとき、後継の ID を 1 つ
# superseded_on: YYYY-MM-DD    # superseded / deprecated にした日（superseded なら後継の accepted_on と同じ）
owners:
  - owner
tags:
  - architecture
related: []
---

# ADR-NNNN: Decision title

<!--
superseded / deprecated にするときだけ、H1 の直後にどちらか 1 行の注記を置く（日付は superseded_on）。
> **置き換え済み（YYYY-MM-DD）**: このADRの決定は現在有効ではない。現行の決定は[ADR-XXXX](XXXX-....md)を読む。
> **廃止（YYYY-MM-DD）**: このADRの決定は現在有効ではない。理由: ...
本文は append-only。後から変えてよいのは status・accepted_on・superseded_by・superseded_on とこの注記だけ。
このコメントは ADR を作るときに消す。
-->

## Context

何が問題で、どの制約があるか。

## Decision

採用する決定。

## Alternatives

検討した選択肢と採用しなかった理由。

## Consequences

この決定による利点、コスト、将来の制約。
