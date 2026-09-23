---
id: adr-0025
type: adr
title: supervisorが手放した未完了runをattention（recover run）にする
status: accepted
created: 2026-09-23
updated: 2026-09-23
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - operations
related:
  - adr-0012
  - adr-0016
  - adr-0019
  - adr-0022
  - adr-0024
  - design-supervisor-lifecycle
---

# ADR-0025: supervisorが手放した未完了runをattention（recover run）にする

## Context

supervisorのabandon（wrapperのheartbeat切れ、wrapperが登録しない、監視中のstepのエラー、provisioningの失敗）は、runの`last_error`を書き、leaseを消し、`runtime_error`（payload: `message`, `lease_released`）を記録するが、statusは`claimed` / `starting` / `running` / `validating`のまま残す。adopt（[ADR-0012](0012-adopt-stale-lease-of-live-wrapper.md)）はstaleなleaseしか拾わないので、このrunを進めるものは`recover`しかない。ところが[ADR-0016](0016-maintainer-notification-and-compact-output.md)の決定2が決めたattentionの一覧にこの状態は無く、`status`にも`watch`にも出ない。2026-09-23にtask 49のrunがこの状態（leaseの無い`running`）で止まり、maintainerが画面と`show`から気づいて`recover`した。

## Decision

1. **leaseの無い未完了runをattentionにする。** `in_progress`のtaskの最新runが`claimed` / `starting` / `running` / `validating` / `integrating`で、lease行が無ければ、`next`を`recover run`とするattentionにする（`domain::AttentionNext::RecoverRun`、判定は`domain::run_attention`）。`kind`はそのrunの直近の`runtime_error`。`integrating`を含めるのは、`integrate`がleaseの取得と`integrating`への遷移、leaseの解放と`integrating`からの遷移をそれぞれ1トランザクションで行い、正常な経路でleaseの無い`integrating`が生じないため。leaseがあってstaleなrunはこのattentionにしない（supervisorが死んだならその`supervisor_stale` / `supervisor_stopped`、wrapperが生きていればadoptの側。死んだ`integrate`が残したstaleなleaseのように、どのattentionにも出ないものは本ADRの対象外として残る）。leaseが無いことは`exit_request_timed_out`より優先する（`/exit`を送っても進めるものがいない）。
2. **attentionイベントは`lease_released: true`の`runtime_error`だけ。** `runtime_error`を`ATTENTION_KINDS`に足し、`event_attention`はpayloadの`lease_released`が`true`のものだけを`recover run`にする。leaseを手放さない`runtime_error`（`record_runtime_error`、heartbeat失敗時の記録など）は記録にとどめ、attentionにしない。kind名とpayloadは変えない（ADR-0016の決定2）。
3. **通知は足さない。** [ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定5で`cmux notify`は`ask_opened`のときだけinbox宛てに送ることになり、runの遷移は通知しない。このattentionもmaintainerは`watch`で受ける。
4. **対処は`dagq-recover`の手順。** `recover run`を見たmaintainerは`doctor`でprocessを確かめて`recover`し、再試行するかは別に決める。

## Alternatives

- **abandonでrunを`failed`や`interrupted`にする**: attentionの一覧は変えずに済むが、abandonは「sessionが生きているかもしれない曖昧な失敗」で、processが止まったことを確かめずにstatusを進めると`recover`の安全確認（processとleaseの再検査）を飛ばすことになる。statusはそのままにして人に知らせる。
- **`runtime_error`をすべてattentionにする**: leaseを持ったままのsupervisorが監視を続けるrunまで人を起こす。`lease_released`で区別できるので分ける。

## Consequences

- ADR-0016の決定2のattention一覧に「supervisorが手放した未完了run（`recover run`）」が加わる。`status` / `events` / `watch`の`next`の値が1つ増える。
- `watch`は`runtime_error`（`lease_released: true`）で起きる。maintainerは画面を読まずにabandonに気づける。
- 監視中のstepのエラーは、validationが`awaiting_integration`を記録した後の後始末（workspaceのclose）の失敗でも起きる。このときの`runtime_error`（`lease_released: true`）は`watch` / `events`では`recover run`になるが、runは未完了ではないので`status`は`recover run`ではなくstatusどおりのattention（`review and integrate`）を出し、`recover`も拒否する。`status`の判定を正とする。
- [ADR-0024](0024-retire-maintainer-into-jobs-and-observer.md)がruntime自身の`recover`を実装するまでの、人かmaintainerが`recover`するための経路である。
- `recover`した後はrunが`interrupted`（`integrating`だったものは`awaiting_integration`）になり、このattentionは消える。
