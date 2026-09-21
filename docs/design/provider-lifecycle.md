---
id: design-provider-lifecycle
type: design
title: Agent provider lifecycle
status: current
created: 2026-09-21
updated: 2026-09-21
last_verified: 2026-09-21
scope: provider
related:
  - adr-0004
  - design-supervisor-lifecycle
---

# Agent provider lifecycle

application層はagent providerの共通契約を使い、CLI引数や出力形式を直接扱わない。

```text
AgentProvider
  launch(run, workspace)
  inspect(session)
  interrupt(session)
  collect_result(session)
```

Claude providerはcmux内の通常セッションを起動し、実装、unit test、E2E、subagent review、完了レポートを実行させる。Codex providerはCodexの対応するセッション方式を使う。provider capabilityとしてinteractive、subagents、stream events、structured resultを表現する。

requested providerとactual providerをTaskRunに保存する。Claudeが起動不能の場合はCodexへfallbackできるが、実装途中の一般的な失敗は自動fallbackしない。
