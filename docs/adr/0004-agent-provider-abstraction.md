---
id: adr-0004
type: adr
title: ClaudeとCodexをagent providerとして抽象化する
status: accepted
created: 2026-09-21
updated: 2026-09-21
owners:
  - hisamekms
tags:
  - provider
  - claude
  - codex
related:
  - design-provider-lifecycle
---

# ADR-0004: ClaudeとCodexをagent providerとして抽象化する

## Context

既定providerはClaudeだが、Claudeを起動できない場合や明示指定時にはCodexを使う。通常セッション、非対話実行、subagentレビュー、structured resultなどproviderごとに異なる機能がある。

## Decision

application層はagent sessionのライフサイクル契約だけを使い、CLI引数・出力形式・セッション方式はprovider adapterに閉じ込める。TaskRunにはrequested providerとactual providerを記録する。

## Alternatives

- Claude/CodexのCLIをapplication層から直接呼ぶ: provider追加時に状態管理とテストが分散する。
- すべてのproviderを同じCLI方式に揃える: interactive sessionなどの固有能力を失う。

## Consequences

provider capabilityとfallback条件を明示する必要がある。実装途中の自動provider切り替えは避け、起動不能など安全に判断できる段階だけfallbackする。
