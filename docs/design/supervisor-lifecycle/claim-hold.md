---
id: design-supervisor-lifecycle-claim-hold
type: design
title: "claimを控える（load average）"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-supervise
  - design-supervisor-lifecycle-status
  - design-supervisor-lifecycle-stats
  - design-supervisor-lifecycle-observer
  - design-supervisor-lifecycle-backend-call-failures
---

# claimを控える（load average）

負荷の高い時間帯にrunを増やすと、cmuxの時間切れ（`backend_call_failed`）とrunの起動の遅れが増える（goal 17、task 327）。supervisorは新しいrunをclaimする前に「claimを控えるか」を1つの判定で決め、控えている間は新しいclaimをしない。走っているrun（sessionの監視・validation・review・resume・triage・着地）には触れない。

## 判定

`domain::claim_hold::ClaimHold::judge`が、判定の入力（`HoldInputs`）から最初に当たった理由（`HoldReason`）を返す。理由は今のところ1つ。

- `load_average`: hostの1分のload average（`getloadavg`）が`supervise --max-load`（既定16.0）を超えている。等しいときは控えない。load averageが読めないときは控えない

後続のtask（463・377・437）は`HoldReason`と`HoldInputs`に理由を足し、同じ判定・同じイベント・同じ`status` / `stats`の出し方を使う。

`--max-load`の既定値16.0の根拠: この queue の host は8コアで、2026-09-26の`stats --full`の`backend_failures.by_load_band`（load帯ごとの`backend_call_failed`）は`0-4`が1件、`8-16`が1件、`16-32`が56件、`32-64`が257件、`64+`が116件だった。cmuxの時間切れはloadがコア数の2倍（16）を超えたところから出始める。`--max-load 0`（0以下）で控えを無効にする。libraryの`SuperviseOptions::new`の既定は無効（`max_load: None`）で、CLIの`supervise`だけが既定16.0を渡す。`up`はまだ`--max-load`を渡さないので、`up`が起動するsupervisorは既定値で動く。

## 判定する場所

`fill_slots`の中で、放置されたrunのadopt・answerの適用・parkしたrunのresume・triage・sweepの後、`[run.env]`のprogramが無いときの打ち切り（ADR-0049の決定9）の次、claimのloopの前に判定する（`Supervisor::hold_claims`）。控えるときはそのpassのclaimをしない。空きslotの有無に関わらず、claimするpassごとに1回判定する。drainやhandoffの途中（claimしないpass）では判定しない。

`--once`のsupervisorは、runが無く控えているpassで終わる（claimできるtaskが無いのと同じ扱い）。

## 記録

判定がqueueの前回の記録と変わったときだけ、queueイベント（task・goal・runを持たない）を1件書く（`domain::claim_hold::transition`）。前回はqueueの最新の`claim_held` / `claim_resumed`で、最新の`claim_held`は、書いたsupervisorがこのsupervisor自身か、今動いている（登録があり、heartbeatがstaleでない）間だけ控えが続いているとみなす。loadはhostのものなので、同じqueueに2つのsupervisorが居ても交互に書き直さない。控えていたsupervisorが止まった（`down --wait`の後の`up`など）か死んだ後に、loadが高いまま起動したsupervisorは自分のtokenで`claim_held`を書き直すので、`status`と`stats`は控えを出し続ける。loadが下がっていれば、残った`claim_held`を`claim_resumed`で終える。

- `claim_held`: 控え始めたとき、または別の理由で控え直したとき。payloadは`reason`、`value`（判定した値。loadなら1分のload average）、`threshold`（`--max-load`）、`message`、`supervisor`
- `claim_resumed`: 控えが終わったとき。payloadは終わった控えの`reason`と`supervisor`

supervisorのlogにも`claim_held`はwarn、`claim_resumed`はinfoで出る。

## `status`と`stats`

- `status`: 最新の`claim_held` / `claim_resumed`が`claim_held`なら、その`supervisor`の登録の項目に`claim_hold`（`claim_held`のpayloadと`since`（記録の時刻））を付ける（[`status`](status.md)）
- `stats`: `claim_holds`に、windowの中で始まった控えの`count`と`secs`（合計秒）、理由ごとの`by_reason: {<reason>: {count, secs}}`、今の控え`held`（`{reason, supervisor, since, value, threshold}`、無ければnull）を出す。控えは次の`claim_held` / `claim_resumed`か、同じsupervisorの`supervisor_stopped`で終わり、まだ終わっていない控えとwindowの後に終わった控えはwindowの終わりまでを数える。`--goal`では件数を数えない（taskを持たないため）が、`held`は出す。控えている間に空きslotがあれば、`idle_slots`の代わりにalert `claim_held`（`value`は空きslotの数）を出すので、控えによる空きと依存の詰まりによる空きを区別できる（[`stats`](stats.md)）
- observerは`stats --since <cursor>`を入力に読むので、`claim_holds`とalert `claim_held`もそのまま載る
