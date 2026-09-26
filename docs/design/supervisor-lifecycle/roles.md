---
id: design-supervisor-lifecycle-roles
type: design
title: "Roles"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0044
---

# Roles

- **supervisor**: runtimeの`supervise`プロセス。taskをclaimし、runごとにworktreeとworkspaceを作って監視し、receiptを検証する。`up`がlaunchdのLaunchAgentとして常駐させるか（既定）、`--in-cmux`なら`[<repo>]supervisor` workspaceの中で動かす。
- **worker**: run session。runごとのcmux workspace `[<repo>]worker#<task-id> - <task title>`（descriptionは`dagq role=worker queue=<queue hash> run=<run-id> task=<id>`）で動くClaude session。`needs_session`のrunをresumeするworkspaceも同じtitleで、descriptionは`run <run-id> resume`。
- **planner**: goal / taskを書いてproposalとしてsubmitするsession。proposalごとのオンデマンドのworkspaceで、常駐しない（[ADR-0044](../../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定1・6）。人が`dagq plan`で開くもの（何度打っても新しいworkspaceが開き、複数同時に開ける）と、runtimeが立てるもの（差し戻し先のplannerが閉じていたproposalなど）がある。workspaceは`[<repo>]planner#<planner-id>`（`DAGQ_ROLE=planner`）で、session wrapper `planner-session`がClaude sessionを`planner_prompt`（runtimeが立てたものは`runtime_planner_prompt`）付きで起動する（[`plan` / `planners`](plan-planners.md#plan--planners)）。人に頼まれれば`up` / `down`も打つ。
- **inbox**: 人に届くものすべての窓口になるsession。openなask（worker・supervisor・job・observerの質問）を人に見せてanswerを書き戻し、それ以外のattentionを人に知らせ、人の指示があるときだけ`dagq-recover` skillの手順（`up` / `down`、手でのreviewと`integrate`、`recover`、runのworkspaceへのキーと`/exit`）を実行する。唯一の常駐sessionで、`up`が`[<repo>]inbox`のworkspace（`DAGQ_ROLE=inbox`）に、`inbox_prompt`付きで起動する（[Session prompts](session-prompts.md#session-prompts)）。
- **observer**: supervisorのtimerが起動するheadlessのjob（`DAGQ_ROLE=observer`、workspaceは持たない）。stats・note・openなask・graphを読み、note・`blocked`のask・draftのgoalだけを書く（[Observer](observer.md#observer)）。

役割はこの5つ（ADR-0044の決定1。ADR-0024の決定1を引き継ぐ）で、review / triageのjobはsupervisorの一部（`DAGQ_ROLE=reviewer`、workspaceは持たない）。runtimeの中で人が打つ`/exit`や復旧は「人（person）」と書き、inboxかplannerのsessionから打つ（`src/`に`operator`も退役した役割名も残らない）。
