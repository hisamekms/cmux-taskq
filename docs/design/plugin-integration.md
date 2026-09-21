---
id: design-plugin-integration
type: design
title: Claude Code and Codex plugin integration
status: current
created: 2026-09-21
updated: 2026-09-21
last_verified: 2026-09-21
scope: distribution
related:
  - adr-0005
---

# Claude Code and Codex plugin integration

runtimeとpluginを分離する。pluginはskill、hook、provider設定を配布し、SQLite・supervisor・cmux操作は`cmux-taskq`バイナリが担当する。

```text
cmux-taskq repository
  ├── runtime binary
  ├── plugins/claude-taskq
  └── plugins/codex-taskq
```

Claude Code pluginは`.claude-plugin/plugin.json`と`skills/`を持ち、Codex pluginは`.codex-plugin/plugin.json`とskills/hooks/scriptsを持つ。共通の手順はshared skillから生成または同期する。pluginのskillからPATH上の`cmux-taskq`を呼び出し、見つからなければインストール方法を案内する。

platformごとのbinary release、checksum、version compatibilityをruntime側で管理する。pluginはruntimeのDB schemaを直接操作しない。
