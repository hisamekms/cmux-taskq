---
id: design-supervisor-lifecycle-claim-defer
type: design
title: "claimを控える（衝突の多いファイル）"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - adr-0069
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-supervise
  - design-supervisor-lifecycle-claim-hold
  - design-supervisor-lifecycle-conflict-thresholds
  - design-supervisor-lifecycle-status
  - design-supervisor-lifecycle-stats
---

# claimを控える（衝突の多いファイル）

候補のtaskが触ると予想するファイルと、進行中のrunのファイルが、衝突の多いファイル（hotspot）で重なるとき、supervisorはそのpassでそのtaskをclaimせずに次の候補をclaimする（[ADR-0069](../../adr/0069-do-not-claim-tasks-overlapping-hot-files.md)、goal 45、task 463）。queue全体のclaimを止める[load averageの控え](claim-hold.md)とは別で、1つのtaskだけを飛ばす。判定は`domain::claim_defer`、supervisorの側は`application/supervise/claim_defer.rs`（`Supervisor::claimable`）。

## 判定の入力

- **hotspot**: `stats`の`conflict_hotspots`（既定のwindow）で`alert`のファイル（mainから消えたものを除き、名前が変わったものは今の名前）。閾値は`dagq.toml`の`[conflicts]`（[Conflict thresholds](conflict-thresholds.md)）。plan reviewのpromptと同じ計算（`Supervisor::conflict_hotspot_files`）で、10分ごとに読み直す
- **予想するファイル**（`domain::claim_defer::expected_files`）: taskの`--paths`（globのまま）。無ければ`dagq related`で最も似た`completed`のtask 3件の`landed_commits`の各commitが変えたファイル（`git diff --name-only <commit>^ <commit>`）。taskごとにhotspotと同じ間隔でcacheする
- **進行中のrun**（`InFlight`）: `latest_runs_in_progress`（`in_progress`のtaskの最新のrun。着地待ち・`needs_session`を含む）ごとに、base commitからhead（`result_commit`、無ければbranch）までの差分のファイルと、そのtaskの予想するファイル。60秒ごとか、claimの直後に読み直す

hotspotが無いか、進行中のrunがどのhotspotも触らなければ、候補のファイルは読まずに全部claimできる。

## 判定

`fill_slots`のclaimのloopで、`dependency_graph`のcandidatesを順に`domain::claim_defer::decide`にかける。

- 候補の予想するファイルと、ある進行中のrunのファイルが同じhotspotを触れば（`glob_matches`）、控える。最初に控えたときだけtaskのevent `claim_deferred`を書く
- 効く優先度が`interrupt`のtaskは控えない。控えていたtaskは`claim_deferral_ended`（`why: cleared`）で終える
- 最初の`claim_deferred`から`[conflicts]`の`defer_max_secs`（既定3600秒）を過ぎたtaskは、重なっていてもclaimし、`claim_deferral_ended`（`why: expired`）を書く。そのtaskが次にclaimされるまで（重なりが一度消えても）控え直さない。`defer_max_secs`は正の整数で、控えを無効にする値は無い
- 重ならなくなったtaskは`claim_deferral_ended`（`why: cleared`）を書いてclaimする
- 控えていたtaskが候補から外れたら（他のsupervisorがclaimした、cancel、依存が戻った）`claim_deferral_ended`（`why: not_candidate`）を書く

控えたtaskを除いた順で`claim_for_supervisor_in_order`がclaimする。1件claimするたびに進行中のrunを読み直すので、同じpassでclaimしたrunとの重なりも見る。他にclaimできるtaskが無くslotが空いていても控えたまま待つ（直列にする）。上限は上の`defer_max_secs`。

supervisorは控えの状態（taskごとの始まりと上限を過ぎたか）を持ち、起動して最初の判定で、taskごとの最新の`claim_deferred` / `claim_deferral_ended` / `run_claimed`（`latest_task_events`）から組み立て直す。最新が`claim_deferred`なら控えの途中、`claim_deferral_ended`の`why: expired`なら上限を過ぎた控え、それ以外は控えていない。supervisorが入れ替わっても上限は最初の`claim_deferred`から数える。

## 記録

- `claim_deferred`（taskのevent）: `reason: hot_files`、`files`（重なったhotspot）、`runs`（`[{run_id, task_id}]`、重なった進行中のrun）、`max_secs`、`message`、`supervisor`。supervisorのlogにwarnで出る
- `claim_deferral_ended`（taskのevent）: `reason`、`why`（`cleared` / `expired` / `not_candidate`）、`deferred_secs`、`supervisor`。logにinfoで出る

## `status`と`stats`

- `status`: `claim_deferrals`に、今控えているtask（taskの最新の`claim_deferred` / `claim_deferral_ended` / `run_claimed`が`claim_deferred`のもの）を`{task_id, reason, since, files, runs, supervisor}`で並べる（[`status`](status.md)）
- `stats`: `claim_deferrals`に、windowの中で始まった控えの`count`と`secs`、終わり方ごとの`by_end: {<why>: {count, secs}}`（`cleared` / `expired` / `not_candidate`、`claim_deferral_ended`の前にclaimされた`claimed`、まだ終わっていない`open`、同じtaskの`claim_deferred`で置き換わった`superseded`）、hotspotごとの控えた回数`by_file`、今の控え`deferred`を出す。控えは次の`claim_deferral_ended`かそのtaskの`run_claimed`で終わり、まだ終わっていない控えはwindowの終わりまでを数える。空きslotがあり控えているtaskがあれば、alert `claim_deferred`（`value`は控えているtaskの数）を`idle_slots`の代わりに出す（[`stats`](stats.md)）
