---
id: adr-0036
type: adr
title: 凍結済みのdocs/journal/を削除し、今も効く手順と観測事実だけをdesign文書へ移す
status: accepted
created: 2026-09-25
updated: 2026-09-25
accepted_on: 2026-09-25
owners:
  - hisamekms
tags:
  - documentation
  - conventions
related:
  - docs-frontmatter
  - design-manual-smoke
  - design-provider-lifecycle
  - design-supervisor-lifecycle
---

# ADR-0036: 凍結済みのdocs/journal/を削除し、今も効く手順と観測事実だけをdesign文書へ移す

## Context

`docs/journal/`（001〜021とREADME）は2026-09-22に凍結した（commit d9f9b80）。以後、タスクの経過と状態はdagqのqueue（`show ID`のrun履歴とreceipt）が持ち、人の判断はADR・Goalの記述・`Task.context`・receiptの`summary`に残している。

凍結後もジャーナルは保守の対象として残り続けた。

- AGENTS.md・README.md・docs/README.md・plans・frontmatter仕様・design文書がジャーナルを根拠や結果としてリンクし、文書を直すたびにそのリンクと記述を確かめる必要があった。
- ジャーナルは当時の役割名（SV、operator、maintainer）とCLI名（`cmux-taskq`）で書かれており、overviewの用語集で読み替えを保守していた。
- 実Claudeを含む経路の手動スモーク（AGENTS.mdの「手動スモーク（journal 010, 012）」）の手順が、日時付きのログの中にしか無かった。

2026-09-24のユーザー決定: ジャーナルはメンテ対象にしたくないので削除する。手動スモークの手順はskillではなくdesign文書へ移す。

## Decision

1. **`docs/journal/`を削除する。** 本文はGit履歴で読める。凍結のcommit d9f9b80以後ジャーナルは変わっていないので、`git show d9f9b80:docs/journal/<file>`で削除前と同じ本文が読める。
2. **今も効く内容だけをdesign文書へ移す。** 日時付きのログは移さない。
   - 手動スモークの手順（journal 010の故障経路のスモーク、012の独立task 1件の完走）は[manual-smoke](../design/manual-smoke.md)に、現在のCLIと役割名（supervisor / worker / planner / inbox / observer）で書き直す。
   - design文書がジャーナルを根拠に挙げていた実機の観測（folder trust dialog、`cmux workspace create --command`の終了後の挙動、launchd modeでのcmux socketの拒否）は、観測した版と結果を本文に書き、ジャーナルへのリンクを外す。
3. **ADR以外の文書からジャーナルへのリンクと言及を無くす。** plansの完了済みステップの結果は、該当するADR・design文書へのリンクに替えるか削る。frontmatter仕様から`journal`型を外す。
4. **既存のADRは書き換えない**（[ADR-0035](0035-adr-is-superseded-whole-with-dates-and-banner.md)のappend-only）。既存のADRに残る`../journal/<file>`へのリンクは切れたままにし、読み手はこのADRの決定1のとおり`git show d9f9b80:docs/journal/<file>`で読む。

## Alternatives

- **凍結のまま残す**: 書き足さなくても、他の文書からのリンクと旧称の読み替えは保守の対象として残り続ける。
- **手動スモークをpluginのskillにする**: skillはsessionが使うCLIの手順で、手動スモークは人がまれに流す検証の手順なので、design文書に置く（ユーザー決定）。
- **既存ADRのリンクを書き換える**: ADRはappend-onlyなので採らない。

## Consequences

- 文書を直すときにジャーナルとの整合を確かめる必要が無くなる。
- 既存ADRのジャーナルへのリンクは切れる。本文はGit履歴で読める。
- ジャーナルにしか無かった細部（当時のコマンド列、イベント列、DBの状態）は履歴でしか読めない。今も効く手順と観測は[manual-smoke](../design/manual-smoke.md)・[provider-lifecycle](../design/provider-lifecycle.md)・[supervisor-lifecycle](../design/supervisor-lifecycle.md)にある。
