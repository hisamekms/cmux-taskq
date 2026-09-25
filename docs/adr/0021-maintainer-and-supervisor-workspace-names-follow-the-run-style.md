---
id: adr-0021
type: adr
title: maintainer / supervisor / resumeのcmux workspace名もrunと同じ`[<repo>]dagq <role>`にそろえる
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
  - adr-0014
  - adr-0015
  - adr-0018
  - design-supervisor-lifecycle
  - design-plugin-integration
---

# ADR-0021: maintainer / supervisor / resumeのcmux workspace名もrunと同じ`[<repo>]dagq <role>`にそろえる

## Context

[ADR-0018](0018-run-workspace-named-after-the-task.md)でrunのworkspace名を`[<repo>]dagq#<task-id> <task title>`にし、決定3でmaintainer（`dagq <repo> maintainer`）とsupervisor（`dagq <repo> supervisor`）、`dagq-maintain` skillが案内する手動のresume workspace（`dagq resume <run-id>`）は据え置いた。cmuxのsidebarでは同じrepositoryのworkspaceが2つの書式で並び、repository名の位置もそろわない。ユーザーは3種の名前もrunと同じスタイルにそろえると決めた。

名前に依存しているのは次だけである。

- `up`はmaintainer workspaceをtitleの完全一致（`find_named`）で探し、あれば`reused`にする。
- `up --in-cmux`はsupervisorの名前のworkspaceが開いていればerrorで止まる（`ensure_supervisor_workspace_free`）。
- `down`とsupervisorの入れ替えは登録の`workspace_id`で閉じ、maintainer session内の`skipped`は`DAGQ_ROLE` / `DAGQ_QUEUE`で判定するので、名前に依存しない。

## Decision

1. maintainerのworkspace名を`[<repo>]dagq maintainer`（`maintainer_workspace_name`）、in-cmux modeのsupervisorを`[<repo>]dagq supervisor`（`supervisor_workspace_name`）にする。`<repo>`は従来どおりrepository rootのbasenameで、basenameが空なら（`/`など）path自体を使う。例: `[dagq]dagq maintainer`、`[dagq]dagq supervisor`。`up`のJSONの`maintainer.name` / `supervisor.name`はこの名前を返す。
2. `dagq-maintain` skillが案内する手動のresume workspaceを`cmux workspace create --name "[<repo>]dagq resume <run ID>"`にする。`<run ID>`はrunのUUID（`claude --resume`に渡すsession ID）。
3. 旧名を探す互換の検索は持たない。切替は下記の手順で一度だけ行う。

この決定はADR-0018の決定3（maintainer / supervisor / resumeの名前は据え置き）と、[ADR-0015](0015-rename-to-dagq.md)の対応表の「cmux workspace名」行のmaintainer / supervisor部分を上書きする。

### 切替手順

固定バイナリ（`~/.local/bin/dagq`）を新しい名前のバイナリに入れ替えた後の`up`は新しい名前でmaintainer workspaceを探す。

- **maintainer**: 旧名`dagq <repo> maintainer`のworkspaceが残っていると、maintainer sessionの外から打った`up`は見つけられず2つ目のmaintainerを作る。バイナリを入れ替える前に`cmux workspace-action --workspace <id> --action rename --title "[<repo>]dagq maintainer"`で改名するか、閉じる。maintainer sessionの中から打つ`up`は環境変数で`skipped`になるので、改名しなくても二重にはならない。
- **supervisor**: `down --wait`が登録の`workspace_id`で旧名のworkspaceを閉じるので、`down --wait` → バイナリ入れ替え → `up --in-cmux`で済む。`down --wait`を経ずに入れ替えた場合も、`up`のversion違いの入れ替え（[ADR-0014](0014-up-replaces-a-supervisor-of-another-binary-version.md)）は`workspace_id`で閉じる。crashしたsupervisorが残した旧名のworkspaceは新しい名前と衝突しないので`up --in-cmux`を止めないが、人が画面を読んで閉じる。
- **run / resume**: runのworkspaceは`workspace_id`で扱うので移行は要らない。開いている旧名のresume workspaceもそのまま使える。

実施はmaintainerが固定バイナリの更新のときに行う。

## Alternatives

- **旧名も探す互換を`up`に持たせる**: 切替の手作業は要らないが、1回だけの移行のためにruntimeに旧書式を残し続けることになる。
- **据え置く（ADR-0018の決定3のまま）**: sidebarで書式が混在し続ける。

## Consequences

- sidebarで同じrepositoryのworkspaceが`[<repo>]dagq`で始まり、repositoryごとにまとまって見える。
- 切替を忘れて旧名のmaintainer workspaceを残したまま外から`up`を打つと、maintainerが2つになる。そのときは片方を閉じる。
