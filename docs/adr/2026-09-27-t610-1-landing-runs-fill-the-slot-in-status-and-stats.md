---
id: adr-t610-1
type: adr
title: statusのslots.usedとstatsのidle_slotsを、supervisorがclaimと戻りの判定に使うslotと同じ集合で数え、着地中のrunも埋まったslotに数える（ADR-0071決定12・13をamends）
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
amends:
  - adr-0071 decision 12
  - adr-0071 decision 13
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - operations
related:
  - adr-0071
  - adr-t598-1
  - design-supervisor-lifecycle-status
  - design-supervisor-lifecycle-stats
  - design-supervisor-lifecycle-waiting
---

# ADR-t610-1: statusのslots.usedとstatsのidle_slotsを、supervisorがclaimと戻りの判定に使うslotと同じ集合で数え、着地中のrunも埋まったslotに数える（ADR-0071決定12・13をamends）

## Context

supervisorは着地中のrun（`integrating`）も、reviewや着地の順番を待つ`awaiting_integration`のrunも、leaseを持ったままslotに持ち続け、新しいclaimと、戻り待ちのrunをslotへ戻す判定にはその数を使う（[ADR-0071](0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)の決定5）。ところがADR-0071の決定12は`status`の`slots.used`を「そのsupervisorのtokenのleaseを持つ`integrating`でないrun」から待ちと戻り待ちを引いた数と決め、決定13は`stats`の`idle_slots`の空きをその`used`で数えると決めていた。

そのため着地中のrunがあると、`status`では空きがあるように見えるのに、戻り待ちのrunは戻れず新しいclaimも起きない。2026-09-26にtask 537でこの食い違いが観測され、戻り待ちが戻れない理由を`status`から読めなかった。goal 41のacceptance (4)（待ちのrunと時間がstatusとstatsで見える）の不足として、人が2026-09-26に、ADR-0071の決定12・13をこのADRで改めることを選んだ。

## Decision

1. **`status`の`slots.used`と`stats`の`idle_slots`の空きは、supervisorがclaimと戻りの判定に使うslotと同じ集合で数える。** 登録されたsupervisorのleaseを持つrunは、着地中（`integrating`）でも、review中や着地の順番を待つもの・resume中のもののように未完了のstatusでなくても、埋まったslotに数える。人の答えを待つrunと戻り待ちのrunは、今までどおり数えない。
2. **登録の無いtokenのleaseで着地中のrun（人が手で打った`integrate`など）は、どのsupervisorのslotにも数えない。** supervisorのslotを占めていないからである。

ADR-0071の決定12の`used`の定義のうち「`integrating`でない」の限定と、決定13の`idle_slots`がそれに従う部分を、これで改める。決定12・13のその他（`waiting`の数え方、`waiting`の配列、`stats`の`waiting`の項目）は変えない。

## Alternatives

- **表示を変えずに、戻れない理由を別の欄で出す**: supervisorの判定と違う数を`used`として見せ続けることになり、読む側（人・inbox・observer）が食い違いを毎回補正しなければならない。
- **supervisorの側で着地中のrunをslotから外す**: 着地は検証のbuildとtestでhostの負荷を持つので、slotの外に出すと`--parallel`で負荷を抑える意味が薄れる。この変更は表示を判定に揃えるだけにする。

## Consequences

- `status`の`slots.used`はsupervisorの`used_slots`と同じ値になり、`used`が`parallel`に達していれば戻り待ちもclaimも止まっていると読める。
- 着地中のrunがあるあいだ`idle_slots`のalertの空きは減り、着地待ちで塞がったslotをobserverが依存の詰まりと取り違えない。
- 数え方の詳細（どのleaseを数えるか、関数の名前）は[`status`](../design/supervisor-lifecycle/status.md)・[`stats`](../design/supervisor-lifecycle/stats.md)・[人の答えを待つrun](../design/supervisor-lifecycle/waiting.md)が持つ。
