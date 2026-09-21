---
id: adr-0002
type: adr
title: cmuxを最初のworkspace backendにする
status: accepted
created: 2026-09-21
updated: 2026-09-21
owners:
  - hisamekms
tags:
  - cmux
  - workspace
related:
  - design-supervisor-lifecycle
---

# ADR-0002: cmuxを最初のworkspace backendにする

## Context

タスク開始時に新しいworkspaceとGit worktreeを作り、開発エージェントを隔離された環境で実行する必要がある。現在の運用対象はcmuxである。

## Decision

最初のruntimeはcmuxを必須workspace backendとして実装し、プロダクト名を`cmux-taskq`とする。domain/applicationはcmuxのAPIを直接参照せず、workspace controller portを介して利用する。

## Alternatives

- backendを最初から複数対応する: 初期の設計・テスト範囲が広がる。
- tmuxを採用する: 現在のworkspace運用と一致しない。

## Consequences

cmux CLI/socketが実行環境の前提になる。将来別backendを追加する場合は、adapterとplugin設定を追加できる。
