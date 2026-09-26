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
failed / interrupted run (a dead run nobody leases is recovered to interrupted first)
  → headless triage (claude -p) → verdict, then close its workspaces
      retry  → task ready → a new run
      resume → needs_session → resumed like above
      ask    → decide ask (retry / resume / cancel), applied once answered
      triage failed → triage_failed (triage by hand)
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

### `install`

- [`install`](supervisor-lifecycle/install.md)

### 人への通知（`cmux notify`）

- [人への通知（`cmux notify`）](supervisor-lifecycle/cmux-notify.md)

### Run environment

- [Run environment](supervisor-lifecycle/run-environment.md)

### Stall thresholds

- [Stall thresholds](supervisor-lifecycle/stall-thresholds.md)

### Conflict thresholds

- [Conflict thresholds](supervisor-lifecycle/conflict-thresholds.md)

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

- [backgroundの処理が終わらないときの復旧job](supervisor-lifecycle/background-recovery-job.md)

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

- [Triage (supervisor)](supervisor-lifecycle/triage.md)

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

- [`stats`](supervisor-lifecycle/stats.md)

### `doctor`

- [`doctor`](supervisor-lifecycle/doctor.md)

### `recover RUN_ID`

- [`recover RUN_ID`](supervisor-lifecycle/recover.md)

### `rebind`

- [`rebind`](supervisor-lifecycle/rebind.md)
