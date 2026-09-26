---
id: adr-0003
type: adr
title: supervisorがagentとworkspaceのライフサイクルを所有する
status: superseded
created: 2026-09-21
updated: 2026-09-21
accepted_on: 2026-09-22
superseded_by: adr-0054
superseded_on: 2026-09-26
owners:
  - hisamekms
tags:
  - supervisor
  - lifecycle
related:
  - design-supervisor-lifecycle
  - design-persistence
---

# ADR-0003: supervisorがagentとworkspaceのライフサイクルを所有する

> **置き換え済み（2026-09-26）**: このADRの決定は現在有効ではない。現行の決定は[ADR-0054](0054-run-lease-ownership-parallel-supervisors-and-recover.md)を読む。

## Context

ClaudeまたはCodexは実装、テスト、subagentレビュー、完了レポートの作成を担当する。workspace削除までagentに任せると、失敗調査や終了競合が起きる。

## Decision

キューごとに一つのRust supervisorを起動し、supervisorがworkspace作成、agent sessionの監視、完了検証、workspace削除を行う。agentはworkspaceを削除せず、結果をイベントまたは完了レシートで通知する。

## Alternatives

- agent自身がworkspaceを削除する: agent異常終了時や途中終了時のcleanupが不確実。
- main sessionが直接全てを管理する: main session終了後に監視が失われる。

## Consequences

supervisorのlease、heartbeat、復旧状態が必要になる。workspaceとworktreeの寿命は分離し、統合待ちのworktreeはworkspace削除後も残す。
