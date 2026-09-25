---
id: adr-0018
type: adr
title: runのcmux workspace名はtaskのtitleにし、run IDはdescriptionに置く
status: accepted
created: 2026-09-23
updated: 2026-09-23
accepted_on: 2026-09-23
owners:
  - hisamekms
tags:
  - runtime
  - cmux
  - operations
related:
  - adr-0010
  - adr-0013
  - adr-0015
  - design-supervisor-lifecycle
  - design-plugin-integration
---

# ADR-0018: runのcmux workspace名はtaskのtitleにし、run IDはdescriptionに置く

## Context

[ADR-0010](0010-maintainer-and-resident-supervisor.md)の決定5と[ADR-0015](0015-rename-to-dagq.md)の対応表で、run（worker）のcmux workspace名は`dagq <repo> <task-id> <run-id>`になった。cmuxのsidebarではUUIDのrun IDが幅を取り、どのtaskが動いているかは`show`を引かないと分からない。

run IDを名前に含める必要はない。runtimeはrunのworkspaceを名前で逆引きせず（titleで探す`find_named`はmaintainerとsupervisorのworkspaceだけに使う）、runはcmuxが返したUUIDの`workspace_id`をDBに持つ。cmux 0.64.25は`workspace create --description <text>`を持ち、`--json workspace list`の`description`で読める。

## Decision

1. runのworkspace名を`[<repo>]dagq#<task-id> <task title>`にする。`<repo>`はrunの`repo_path`のbasename（従来どおり）、`<task-id>`は`tasks.id`、`<task title>`はtaskのtitleをそのまま使い、切り詰めも整形もしない。例: `[dagq]dagq#15 Set last_error when a run fails`。
2. run IDは名前から外し、workspace作成時に`--description "run <run-id>"`で置く。`<run-id>`は`show`・`claude --resume`・run_dirと同じUUID。
3. maintainer（`dagq <repo> maintainer`）とsupervisor（`dagq <repo> supervisor`）の名前、`dagq-maintain` skillが案内する手動の`dagq resume <run-id>` workspaceは変えない。`up`のtitle完全一致の検索とAGENTS.md・testsが依存しているので、切り替えるなら別に決める。
4. titleはrunが持たないので、`WorkspaceBackend::create`はtaskも受け取り、名前とdescriptionはadapterの純粋関数（`run_workspace_name` / `run_workspace_description`）が組み立てる。domainとapplicationはcmuxの書式を知らない（[ADR-0013](0013-layered-architecture-and-type-function-style.md)）。

この決定はADR-0010の決定5のworker名と、ADR-0015の対応表の「cmux workspace名」行のrun部分を上書きする。

## Alternatives

- **名前に短縮したrun IDを残す**: sidebarでrunを区別できるが、同じtaskの再試行は同時に走らないのでtask IDで足り、run IDの一部は`show`との照合にも使いにくい。
- **titleを一定長で切り詰める**: 幅は揃うが、切り方の規則を持つことになり、cmux側の表示で省略されるので不要。
- **maintainer / supervisorも同じ書式にそろえる**: `up`の検索、AGENTS.md、testsを同時に変える必要があり、この変更の目的（runの識別）とは独立なので見送る。

## Consequences

- sidebarでどのtaskのrunかが一目で分かる。run IDは`cmux --json workspace list`の`description`か`show`の`workspace_id`で辿る。
- 既に開いている旧名のrun workspaceはruntimeが`workspace_id`で扱うので、名前の移行は要らない。
- titleにcmuxのtitleとして不都合な文字（改行など）が入っていてもそのまま渡す。問題が出たら整形の規則を別に決める。
