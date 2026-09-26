---
id: design-supervisor-lifecycle-doctor
type: design
title: "`doctor`"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
---

# `doctor`

ユースケースは`application::health::doctor`で、worktreeとrun directoryとreceiptの有無は`RunFiles`、PIDの生死は`ProcessControl`で見る。

`dagq doctor`は状態を変えずにJSONで報告する。以下は`doctor --full`の内容で、既定の出力はsupervisor 1件・run 1件につき1行相当に圧縮する（ADR-0016の決定4。キー名は変えず省くだけ）: supervisorは`pid`、`alive`、`registered`、`mode`、`workspace_id`、`binary_version`、`heartbeat_age_secs`、`stale`、`run_ids`、runは`run_id`、`task_id`、`status`、`lease_stale`（leaseがなければnull）、`recoverable`、`blocker_count`（`blockers`の件数）、`workspace_id`、`worktree_path`（`RunHealth::summary` / `SupervisorHealth::summary`）。

- `supervisors`: `status`と同じ。staleな登録は報告するだけで、`doctor`も`recover`も`integrate`も消さない。
- `runs`: `claimed`/`starting`/`running`/`validating`/`integrating`のrunごとに、`workspace_id`、worktreeとrun directoryとreceiptの存在、`last_error`、そのrunの`lease`（PID、`kill -0`による生存、heartbeatの経過秒数、30秒を超えた`stale`。なければnull）、登録済みwrapper/agentプロセスのPID・生存・heartbeat経過秒数・終了コード。`exited_at`が記録済みのプロセスはPIDが再利用されうるため生存確認せず`alive: null`にする。
- `blockers`: そのrunの`recover`を拒む理由の一覧。そのrunのprocessとleaseだけを見る。空なら`recoverable: true`。

cmux workspaceの存在は確認しない（cmuxなしで動く）。IDを見てユーザーが`cmux workspace list`で確認する。
