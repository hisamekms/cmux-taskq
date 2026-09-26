---
id: design-supervisor-lifecycle
type: design
title: Supervisor and workspace lifecycle
status: current
created: 2026-09-21
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle-claim-hold
  - adr-0044
  - adr-0047
  - adr-0038
  - adr-0002
  - adr-0003
  - adr-0006
  - adr-0007
  - adr-0008
  - adr-0009
  - adr-0010
  - adr-0011
  - adr-0012
  - adr-0039
  - adr-0013
  - adr-0014
  - adr-0045
  - adr-0016
  - adr-0018
  - adr-0019
  - adr-0020
  - adr-0021
  - adr-0022
  - adr-0040
  - adr-0024
  - adr-0025
  - adr-0026
  - adr-0027
  - adr-0028
  - adr-0029
  - adr-0031
  - adr-0037
  - adr-0043
  - design-persistence
  - design-provider-lifecycle
  - design-plugin-integration
---

# Supervisor and workspace lifecycle

```text
ready task (dependencies completed)
  → claim TaskRun + run lease      ─┐
  → create Git worktree              │ up to --parallel N runs at once,
  → create cmux workspace            │ each with this state machine
  → start session wrapper            │
  → start Claude/Codex               │
  → running                          │
  → completion receipt, session idle │ (the session stays open)
  → validate receipt, commit, clean state (own thread)
  → awaiting_integration: headless review (claude -p) → verdict
      pass    → /exit → close workspace → land (single slot, push)
      revise  → fixed request to the live session → rewritten receipt
                → validate → review again (at most 2 revises)
      concern / 3rd non-pass → /exit → close → approve_landing ask
      unreadable verdict     → review once more (review_retried)
      review failed          → /exit → close → approve_landing ask + review_failed
  → release run lease               ─┘
  → integrate (by hand, one at a time, FIFO by validation):
      integrating → rebase onto main → re-validate → squash-land on main
      → run integrated (result_commit = landed commit), task completed
      → worktree and branch removed; history kept at refs/dagq/runs/<run-id>
    conflict / failed re-validation → needs_session
      → the supervisor resumes the session in the worktree (up to 3 attempts); it resolves,
        reruns verification, rewrites the receipt → the supervisor lands the run whose
        integrate was called, or validates and reviews it with the resumed session open
        (failed receipt → run failed)
  → dependents become candidates; the resident loop claims them from the landed main
failed / interrupted run (a dead run nobody leases is recovered to interrupted first;
a run whose resumes are used up is failed with its resume_exhausted alert)
  → headless recovery job (claude -p) → verdict; the runtime checks the action again,
    then closes its workspaces
      retry          → task ready → a new run (only without commits of its own)
      retry_inherit  → task ready → a new run carrying the branch over (once per task)
      resume         → needs_session → resumed like above (while resumes are left)
      wait           → the job runs again after recheck_after_secs
      escalate / low confidence / refused / 3 jobs used → decide ask
                       (retry / resume / cancel + the job's options), applied once answered;
                       a job's option goes back to the job
      job failed → triage_failed (triage by hand, read as recover by hand)
live session alert (long_background, stuck_exit, prompt_waiting)
  → the same recovery job in the session's slot → repair applied, or the alert's ask;
    a failed job → recovery_failed (recover by hand)
```

各節は`supervisor-lifecycle/`の下の別のファイルにある。下の見出しは各ファイルへの目次で、以前この文書の中にあった節へのリンク（見出しのanchor）もここに届く。

## Implementation status

- [Implementation status](supervisor-lifecycle/implementation-status.md)

## Roles

- [Roles](supervisor-lifecycle/roles.md)

## `up` / `down`

- [`up` / `down`](supervisor-lifecycle/up-down.md)

### `plan` / `planners`

- [`plan` / `planners`](supervisor-lifecycle/plan-planners.md)

### Build identifier

- [Build identifier](supervisor-lifecycle/build-identifier.md)

### Logs

- [Logs](supervisor-lifecycle/logs.md)

### Naming

- [Naming](supervisor-lifecycle/naming.md)

### Session prompts

- [Session prompts](supervisor-lifecycle/session-prompts.md)

### 人への通知経路（ADR-0016で決定、ADR-0022とADR-0024で改めた）

- [人への通知経路（ADR-0016で決定、ADR-0022とADR-0024で改めた）](supervisor-lifecycle/notification-route.md)

## `supervise`

- [`supervise`](supervisor-lifecycle/supervise.md)

### Handoff

- [Handoff](supervisor-lifecycle/handoff.md)

### claimを控える（load average）

- [claimを控える（load average）](supervisor-lifecycle/claim-hold.md)

### 人の答えを待つrun（slotの外の待ち）

- [人の答えを待つrun（slotの外の待ち）](supervisor-lifecycle/waiting.md)

### `install`

- [`install`](supervisor-lifecycle/install.md)

### Auto-update

- [Auto-update](supervisor-lifecycle/auto-update.md)

### 人への通知（`cmux notify`）

- [人への通知（`cmux notify`）](supervisor-lifecycle/cmux-notify.md)

### Run environment

- [Run environment](supervisor-lifecycle/run-environment.md)

### Stall thresholds

- [Stall thresholds](supervisor-lifecycle/stall-thresholds.md)

### Conflict thresholds

- [Conflict thresholds](supervisor-lifecycle/conflict-thresholds.md)

### Landing recheck

- [Landing recheck](supervisor-lifecycle/landing-recheck.md)

### Prompt

- [Prompt](supervisor-lifecycle/prompt.md)

### 1 runの異常（abandon）

- [1 runの異常（abandon）](supervisor-lifecycle/abandon.md)

## Observer

- [Observer](supervisor-lifecycle/observer.md)

## `session` wrapper

- [`session` wrapper](supervisor-lifecycle/session-wrapper.md)

## Receipt and session exit

- [Receipt and session exit](supervisor-lifecycle/receipt-and-session-exit.md)

### wrapperが黙ったsession

- [wrapperが黙ったsession](supervisor-lifecycle/silent-wrapper.md)

### ダイアログ待ちの検知

- [ダイアログ待ちの検知](supervisor-lifecycle/prompt-waiting.md)

### receiptの無いidleの検知

- [receiptの無いidleの検知](supervisor-lifecycle/idle-without-receipt.md)

### backgroundの処理が終わらないときの復旧job

- [生きているsessionの復旧job](supervisor-lifecycle/background-recovery-job.md)（`long_background`、`stuck_exit`、`prompt_waiting`）

### workerの質問への回答の送信

- [workerの質問への回答の送信](supervisor-lifecycle/worker-question-answer.md)

### sessionへの送信と確認

- [sessionへの送信と確認](supervisor-lifecycle/session-send.md)

### 最初のcommitの観測

- [最初のcommitの観測](supervisor-lifecycle/first-commit.md)

### worktreeへの読み取り専用のgit

- [worktreeへの読み取り専用のgit](supervisor-lifecycle/worktree-read-only-git.md)

## Validation

- [Validation](supervisor-lifecycle/validation.md)

## Review (supervisor)

- [Review (supervisor)](supervisor-lifecycle/review.md)

## Triage (supervisor)

- [Triage (supervisor)](supervisor-lifecycle/triage.md)（終わったrunの復旧job）

## Draft planners (supervisor)

- [Draft planners (supervisor)](supervisor-lifecycle/draft-planners.md)

## Plan review (supervisor)

- [Plan review (supervisor)](supervisor-lifecycle/plan-review.md)

## `review`

- [`review`](supervisor-lifecycle/review-command.md)

## `integrate`

- [`integrate`](supervisor-lifecycle/integrate.md)

### `needs_session`

- [`needs_session`](supervisor-lifecycle/needs-session.md)

### errorと復旧

- [errorと復旧](supervisor-lifecycle/integrate-errors.md)

## Cleanup and recovery

- [Cleanup and recovery](supervisor-lifecycle/cleanup-and-recovery.md)

### Run workspaces

- [Run workspaces](supervisor-lifecycle/run-workspaces.md)

### Run worktrees

- [Run worktrees](supervisor-lifecycle/run-worktrees.md)

### backendの呼び出しの失敗

- [backendの呼び出しの失敗](supervisor-lifecycle/backend-call-failures.md)

### `status`

- [`status`](supervisor-lifecycle/status.md)

### `events` / `watch`

- [`events` / `watch`](supervisor-lifecycle/events-watch.md)

### `timeline`

- [`timeline`](supervisor-lifecycle/timeline.md)

### `ask` / `answer` / `asks`

- [`ask` / `answer` / `asks`](supervisor-lifecycle/ask.md)

### `stats`

- [`stats`](supervisor-lifecycle/stats.md)（Claude sessionの区間のkindごとの開いている時間とtranscriptのturnから導く稼働時間の集計`sessions`を含む。[ADR-0048](../adr/0048-record-claude-sessions-by-kind-with-open-and-active-time.md)。runのsessionの作業の内訳`work_breakdown`（task 514）も）

### `mark` / `marks`

- [変更の印（`mark` / `marks`）](supervisor-lifecycle/marks.md)

### `kpi`

- [`kpi`](supervisor-lifecycle/kpi.md)（KPIを日・ISO週・taskの種類・claimの属性で集計し、前の期間と目標と比べ、変更の印の前後を比べる。[ADR-0051](../adr/0051-kpi-time-series-report-and-push.md)の決定1〜9・14〜19）

### `doctor`

- [`doctor`](supervisor-lifecycle/doctor.md)

### `recover RUN_ID`

- [`recover RUN_ID`](supervisor-lifecycle/recover.md)

### `rebind`

- [`rebind`](supervisor-lifecycle/rebind.md)
