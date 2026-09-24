---
id: adr-0038
type: adr
title: taskがgoalに依存でき、依存先のgoalがachievedで閉じるまでclaimされない
status: accepted
created: 2026-09-25
updated: 2026-09-25
accepted_on: 2026-09-25
owners:
  - hisamekms
tags:
  - runtime
  - domain
  - planner
related:
  - adr-0009
  - adr-0013
  - adr-0019
  - adr-0023
  - adr-0024
  - design-domain-model
  - design-persistence
  - design-supervisor-lifecycle
---

# ADR-0038: taskがgoalに依存でき、依存先のgoalがachievedで閉じるまでclaimされない

## Context

依存はtask → task（`task_dependencies`）だけで、claimは依存元のtaskが`completed`かだけを見ていた（`READY_QUERY`）。別のgoalの成果を待つtaskは、そのgoalの終端taskに依存させるしかない。ところが`integrate`はreceiptの`follow_ups`からそのgoalにdraft taskを足し（[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)の決定4）、plannerがそれを`ready`にしても、終端taskが`completed`になった時点で依存は解けて後続がclaimされる。goalの作業がまだ残っているのに、意図しない時点で走る（2026-09-24の人の課題）。

goalが終わったことを宣言するのは、plannerが`follow_ups`の採否を決め、acceptanceを照合して打つ`goal close --verdict achieved`だけである（[ADR-0009](0009-goal-groups-tasks.md)）。所属taskが全部終端かどうかでは、`follow_ups`が足される余地を閉じられない。

なお、goal 20の記述は新しいADRを「0031」としていたが、0031は既に別の決定（inbox / plannerのworkspaceの色とピン）に使われているため、次の空き番号0038にした。

## Decision

1. **taskからgoalへの依存の辺を足す。** `task_goal_dependencies(task_id, goal_id)`（migration 0019）に保存する。既存の`task_dependencies`は変えず、task → taskの依存の意味もそのまま残す。辺を張れる・外せるのは、task依存と同じくdraft / readyのtaskだけ。CLIは`add --depends-on-goal ID`（繰り返し可）、`dependency add TASK --goal ID`、`dependency remove TASK --goal ID`。追加・削除はtaskのイベント`goal_dependency_added` / `goal_dependency_removed`（payload `goal_id`）で、task依存の`dependency_added` / `dependency_removed`とは別のkindにする。
2. **goal依存の充足条件は「goalが閉じていて、verdictが`achieved`」だけ。** `READY_QUERY`（`candidates`とsupervisorのclaimが共有する）は、依存先goalに`closed_at IS NOT NULL AND verdict = 'achieved'`でないものが1つでもあるtaskを除く。goalの所属taskが全部終端でも、goalが開いている間は待つ。`abandoned`で閉じたgoalは依存を解かない（canceledのpredecessorと同じく塞いだままにし、`graph`で見えるようにする）。
3. **循環はtask依存・goal依存・goalの所属を合わせたグラフで拒否する。** goalは所属taskを待つとみなし（goal → 所属taskの暗黙の辺）、task → predecessor、task → 依存先goalの辺と合わせる。自分の属するgoalへの依存は長さ2の循環で、自己依存に相当するので専用のエラー（`OwnGoalDependency`）で拒否する。それ以外の循環は、`dependency add --goal`と`add --depends-on-goal`では`GoalDependencyCycle`、`set-goal`でtaskを自分が（間接に）待つgoalへ移すときは`GoalMembershipCycle`、task依存の追加でgoalを経由する循環ができるときは従来の`DependencyCycle`で拒否する。`add --goal G --depends-on-goal H`はtaskを所属ごと保存してから辺を足すので同じ検査を通る。循環の検出は[ADR-0013](0013-layered-architecture-and-type-function-style.md)の例外の範囲で、SQLの1本の再帰CTEに置き、拒否の判断とエラー文はdomain（`task::check_not_own_goal` / `check_goal_acyclic` / `check_membership_acyclic` / `check_acyclic`）が持つ。
4. **出力に載せる。** `show`と`list`の要素に`goal_dependencies`（goal IDの昇順）、`goal show`にそのgoalを待つ未完了task（`dependents`）。`graph`はtaskごとに`goal_dependencies`を出し、`ready_after`に未充足のgoal依存を`{"goal": ID}`で（未完了の依存元taskのIDの後に）含め、閉じていないgoalの所属taskの`blocks` / `unblocks`にそのgoalを待つtaskを数える。`candidates`の順と`critical`の鎖（[ADR-0023](0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)の決定4）もそれに従う。
5. **workerのpromptは依存先goalの成果を示す。** Predecessorの節に、task依存の行に続けて依存先goalのtitleと、そのgoalの`completed`のtaskのtitle・着地commit・receiptのsummaryを並べる。goalはtaskが多くなりうるので、summaryは200文字で切る。

## Alternatives

- **goalの所属taskが全部終端になったら依存を解く。** `follow_ups`のdraftが後から足される余地を閉じられず、課題そのものが残る。
- **終端taskに依存させ、`follow_ups`の登録時に後続の依存を張り替える。** integrateがgoalの外のtaskの依存を書き換えることになり、plannerの判断（どのfollow_upを採るか）より先に辺が変わる。goal closeという既にある「終わった」の宣言を使う方が単純。
- **`abandoned`で閉じたgoalも依存を解く。** 待っていた成果が無いまま後続が走る。canceledのpredecessorと同じく塞いだままにし、plannerが依存を外すか付け替える。
- **task依存とgoal依存を1つの表にする。** 既存の`task_dependencies`の意味と主キーを変えることになり、既存queueの移行が要る。別表なら0019は表を足すだけで済む。

## Consequences

- goalをまたぐ待ちはgoal依存で書くのが標準になる。goalの成果を待つtaskは、そのgoalが`achieved`で閉じるまで走らない。
- goalを閉じ忘れると、依存するtaskが走らない。`graph`の`ready_after`と`goal show`の`dependents`で見えるので、plannerがgoalを閉じる判断の手がかりになる。
- `abandoned`で閉じたgoalに依存するtaskは、人が依存を外すまで`ready_after`に残る。
- `graph`の`ready_after`の要素がtask ID（数）とgoal（`{"goal": ID}`）の混在になる。task依存だけのtaskの出力は変わらない。
- plugin の skill（使い分けの説明）は、この決定の実装とは別の変更で書く。
