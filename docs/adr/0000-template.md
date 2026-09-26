---
id: adr-tTASK-N                # 書く task の ID と枝番（1 本でも -1）。ファイル名は YYYY-MM-DD-tTASK-N-<slug>.md（日付は accepted_on）
type: adr
title: Decision title
status: proposed
created: YYYY-MM-DD
updated: YYYY-MM-DD
# accepted_on: YYYY-MM-DD      # accepted にする変更で足す。updated は変えない
# supersedes: [adr-tTASK-N]    # 置き換える ADR があれば。置き換えは丸ごと（ADR-t598-1）。本文と一緒に書き、後から変えない
# amends: [adr-0047 決定 24]    # 凍結した大きな ADR の一部を変えるとき。元の ADR に amended_by を足し、design を今の姿に直す
# amended_by: [adr-tTASK-N]     # この ADR の一部を変えた ADR（凍結した大きな ADR だけ）
# superseded_by: adr-tTASK-N     # superseded にするとき、後継の ID を 1 つ
# superseded_on: YYYY-MM-DD    # superseded にした日（後継の accepted_on と同じ）
# deprecated_on: YYYY-MM-DD    # deprecated にした日
owners:
  - owner
tags:
  - architecture
related: []
---

# ADR-tTASK-N: Decision title

<!--
superseded / deprecated にするときだけ、H1 の直後にどちらか 1 行の注記を置く（置き換え済みの日付は superseded_on、廃止の日付は deprecated_on）。
> **置き換え済み（YYYY-MM-DD）**: このADRの決定は現在有効ではない。現行の決定は[ADR-XXXX](XXXX-....md)を読む。
> **廃止（YYYY-MM-DD）**: このADRの決定は現在有効ではない。理由: ...
本文は append-only。後から変えてよいのは status・accepted_on・superseded_by・superseded_on・deprecated_on・amended_by とこの注記だけ。
1 ADR に決定 1 つ（密に結びついた数個まで）、本文はおおむね 100 行以内。書くのは変えるのに人の判断が要るもの（問題と文脈、方針・原則・境界・不変条件、退けた案、結果）。
event の kind や欄名、flag の綴り、JSON の形、既定値・閾値の数値、関数・ファイル名、migration の番号、test の名前は docs/design に書く。目安は「これを変えるとき人に聞くか」。
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
