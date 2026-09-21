---
id: design-provider-lifecycle
type: design
title: Agent provider lifecycle
status: current
created: 2026-09-21
updated: 2026-09-22
last_verified: 2026-09-22
scope: provider
related:
  - adr-0004
  - design-supervisor-lifecycle
---

# Agent provider lifecycle

application層はagent providerの共通契約を使い、CLI引数や出力形式を直接扱わない。

```text
AgentProvider
  preflight()              -- 実装済み: 実行可能性の確認
  command(run, prompt)     -- 実装済み: wrapperが起動するコマンド
  inspect / interrupt / collect_result  -- 後続
```

Claude Code adapter（`src/infrastructure/adapters.rs`）はworktreeをcwdにし、`--session-id`にrun IDを渡し、`--debug-file`をrun管理領域に置き、`--add-dir`でrun管理領域への書き込みを許可し、promptを位置引数で渡す。stdin/stdout/stderrはwrapperのTTYを継承する。permission modeは上書きしない。

Claude providerはcmux内の通常セッションを起動し、実装、unit test、E2E、subagent review、完了レポートを実行させる。Codex providerはCodexの対応するセッション方式を使う。provider capabilityとしてinteractive、subagents、stream events、structured resultを表現する。

requested providerとactual providerをTaskRunに保存する。Claudeが起動不能の場合はCodexへfallbackできるが、実装途中の一般的な失敗は自動fallbackしない。
