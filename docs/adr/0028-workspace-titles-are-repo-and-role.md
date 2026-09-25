---
id: adr-0028
type: adr
title: cmux workspaceのtitleを`[<repo>]<role>`にし、planner / inboxの名前とrole値を定義する
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
  - adr-0011
  - adr-0018
  - adr-0021
  - adr-0022
  - adr-0026
  - design-supervisor-lifecycle
  - design-plugin-integration
  - design-overview
---

# ADR-0028: cmux workspaceのtitleを`[<repo>]<role>`にし、planner / inboxの名前とrole値を定義する

## Context

[ADR-0018](0018-run-workspace-named-after-the-task.md)でrunのworkspace名を`[<repo>]dagq#<task-id> <task title>`にし、[ADR-0021](0021-maintainer-and-supervisor-workspace-names-follow-the-run-style.md)でmaintainer / supervisor / 手動resumeを`[<repo>]dagq maintainer` / `[<repo>]dagq supervisor` / `[<repo>]dagq resume <run-id>`にそろえた。どちらも名前で役割が分かることと、`up`がmaintainerをtitleの完全一致で探すことを前提にしていた。

[ADR-0026](0026-identify-workspaces-by-uuid-env-and-queue-group.md)で識別はqueue DBのUUIDに移り、roleはworkspaceの`--env DAGQ_ROLE`、queueはworkspace group（`[<repo>]`）とdescriptionが持つようになった。titleは表示専用になり、`dagq`という語は何も区別しない（1つのcmuxにあるdagqのworkspaceはすべてdagqのもの）。ユーザーは2026-09-23に、名前から`dagq`を取り`[<repo>]<role>`にすると決めた。また[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)が予定するplanner（人と対話してgoal / taskを登録するsession）とinbox（人がmaintainerからのaskに答えるsession）は、後続goalで`up`が開く。

## Decision

1. titleは`[<repo>]<role>`とする。`<repo>`はrepository rootのbasename（`/`のようにbasenameが空ならpath自体）で、`]`の直後に空白を入れない。
   - supervisor（in-cmux mode）: `[<repo>]supervisor`（`supervisor_workspace_name`）
   - maintainer: `[<repo>]maintainer`（`maintainer_workspace_name`）
   - worker: `[<repo>]worker#<task-id> - <task title>`（`run_workspace_name`。titleは切り詰めない）
   - planner: `[<repo>]planner`（`planner_workspace_name`）
   - inbox: `[<repo>]inbox`（`inbox_workspace_name`）

   `up`のJSONの`maintainer.name` / `supervisor.name`はこの名前を返す。
2. `needs_session`のrunをresumeするworkspace（当面はmaintainerが`dagq-session` skillに従って開き、goal 8の自動resumeではruntimeが開く）は、workerと同じ`[<repo>]worker#<task-id> - <task title>`をtitleにし、descriptionを`run <run-id> resume`にする。runtimeが作るときも`run_workspace_name`を使う。
3. `DAGQ_ROLE`の値を`SessionRole`（`maintainer` / `supervisor` / `worker` / `planner` / `inbox`）に合わせ、`lifecycle`に`MAINTAINER_ROLE`に加えて`WORKER_ROLE` / `PLANNER_ROLE` / `INBOX_ROLE`を定数として置く。planner / inboxのworkspaceはこの決定では作らない（名前とrole値の定義だけ）。
4. 旧名との互換は持たない。識別はUUIDなので、旧名のままのworkspaceもreuse・`down`・入れ替えの対象のままで、切替の手作業は要らない。旧名のworkspaceは次に作り直されたときに新しい名前になる。

この決定はADR-0018の決定1（runの名前の書式）とADR-0021の決定1・2（maintainer / supervisor / resumeの名前）を上書きする。ADR-0018とADR-0021の本文は書き換えない。

## Alternatives

- **`dagq`を残す（ADR-0021のまま）**: 名前で役割を示す必要はADR-0026で無くなり、`dagq`は区別に使われない語として幅を取るだけになる。
- **resumeに専用の名前（`[<repo>]resume <run-id>`など）を付ける**: resumeは同じrunの同じsessionを開き直すもので、sidebarでは元のrunと同じtaskとして見えるほうが分かりやすい。区別はdescriptionで足りる。

## Consequences

- sidebarでは`[<repo>]`のworkspace groupの中で、roleとtask titleが先頭に近い位置に見える。
- AGENTS.md・design・pluginのskillの旧表記は新しい名前に置き換えた。ADRとjournalに残る旧表記はそのまま読み替える。
- planner / inboxを開く後続goalは、名前とrole値をここで定義したものから使う。
