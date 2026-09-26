---
id: design-supervisor-lifecycle-recover
type: design
title: "`recover RUN_ID`"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
---

# `recover RUN_ID`

ユースケースは`application::health::recover`（`doctor`と同じport）。

1. runが`claimed`/`starting`/`running`/`validating`/`integrating`でも、leaseの残った`awaiting_integration`（reviewの途中でsupervisorが死んだrun。task 236）でもなければ拒否する。
2. `doctor`と同じ確認を行い、未終了として登録されたプロセスのPIDが生きている、そのrunのleaseのheartbeatが30秒以内、leaseのPIDが生きている、のいずれかなら拒否する。heartbeatが止まったまま生きているsupervisorのleaseを`recover`は奪わず、ユーザーが止める（wrapperが生きていれば[adopt](supervise.md#supervise)が引き継ぐ）。supervisorがabandonしたrunはleaseがないので、processが止まれば復旧できる。`recover`が扱うのは、引き継ぎの条件を満たさないrun: wrapperが死んだか30秒以上黙っている、`claimed` / `starting`、`integrating`、leaseのないrun、それとsupervisorが居ないときの`awaiting_integration`のstaleなlease。
3. `BEGIN IMMEDIATE`の中でそのrunのleaseが新鮮でないことと`run_processes`の行数が確認時と同じことを再検査し、runを`interrupted`（`integrating`なら`awaiting_integration`: 検証済みの成果は残っており、次の`integrate`が途中のrebaseをabortしてやり直す。`awaiting_integration`はそのまま: leaseだけを外し、`integrate`で着地できるようにする）にし、確認した内容を`run_recovered`イベント（`previous_status`、`status`、`lease_deleted`、`run`）に記録し、そのrunのleaseだけを削除する。

他のrun、そのlease・process、`run_processes`、worktree、branch、workspace、run directoryは触らない。Taskは`in_progress`のまま残る。`recover`はTaskを`ready`に戻さない: 復旧と再実行は別の判断であり、`failed`で止まったrunと同じく、supervisorのtriageが`interrupted`のrunの次の一手（retry / resume / ask）を決める（[Triage (supervisor)](triage.md#triage-supervisor)）。supervisorは、leaseが無くprocessの止まったrunをこの手順で自分でrecoverする（`run_recovered`に`by: "supervisor"`）。人が`recover`を打つのは、supervisorが居ないとき（または死んだsupervisorのstaleなleaseが残っているとき）だけ。再試行を人が決めるなら`ready ID`（編集するなら`draft ID`）で行い、動いているsupervisor（または次のsupervisor）が新しいTaskRunと新しいworktreeを作る。
