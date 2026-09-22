---
id: journal-index
type: design
title: Task journals
status: superseded
created: 2026-09-22
updated: 2026-09-22
last_verified: 2026-09-22
tags:
  - documentation
---

# Task journals

このディレクトリは2026-09-22で凍結した。新しいジャーナルは作らない。既存のファイル（001〜021）は過去の作業記録として参照用に残し、内容は当時のまま更新しない。各ファイルのfrontmatterの`status`は執筆時点の記録であり、キューのタスク状態を表さない。一覧・状態・依存・run履歴は`cmux-taskq list`と`show ID`が正である。人の判断はADR（[adr/](../adr/)）、Goalの記述（`cmux-taskq goal`）、`Task.context`、receiptの`summary`に残し、普遍的な事実は[design/](../design/)とADRに書く。
