---
id: adr-0069
type: adr
title: 衝突の多いファイルで進行中のrunと重なるtaskはそのpassでclaimせずに次の候補へ進み、控えた理由と時間をstatusとstatsに出す
status: accepted
created: 2026-09-26
updated: 2026-09-26
accepted_on: 2026-09-26
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - performance
related:
  - adr-0029
  - adr-0046
  - adr-0049
  - adr-0068
  - design-supervisor-lifecycle-claim-defer
  - design-supervisor-lifecycle-claim-hold
---

# ADR-0069: 衝突の多いファイルで進行中のrunと重なるtaskはそのpassでclaimせずに次の候補へ進み、控えた理由と時間をstatusとstatsに出す

## Context

claimは依存と優先度だけで順を決め（[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)が引き継いだclaimの順）、走っているrunや着地待ちのrunと触るファイルが重なるかを見ない。2026-09-26の直近3時間の着地8件のうち5件がrebaseの衝突で延期され（延期7回、resume 10回）、実作業10〜44分に対して着地待ちが4〜8時間になった（goal 45）。`stats`の`conflict_hotspots`（goal 31）は、衝突が特定のファイル（`src/infrastructure/schema.rs`・`docs/plans/current.md`・`tests/cli.rs`など）に集中していることを示す。

task 327はload averageが高い間にqueue全体のclaimを控える仕組み（`claim_held` / `claim_resumed`、`status`の`claim_hold`、`stats`の`claim_holds`）を入れた。今回の控えは1つのtaskだけを飛ばして次の候補をclaimするもので、queue全体を止めるものではない。goal 15（abandoned）が同じ問題を挙げていた。

## Decision

1. **予想するファイル**: 候補のtaskが触ると予想するファイルは、宣言した`--paths`（[ADR-0029](0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)。globのまま扱う）。宣言が無ければ、`dagq related`（[ADR-0046](0046-full-text-search-related-and-duplicate-of.md)）で最も似た`completed`のtask 3件の着地commit（`landed_commits`）が変えたファイル。どちらも無ければ何も予想せず、控えない
2. **進行中のrunのファイル**: `in_progress`のtaskの最新のrun（走っている、validating・review中、着地待ち、`needs_session`で人やresumeを待つものを含む）ごとに、base commitからrunのhead（`result_commit`、無ければbranch `dagq/<run-id>`）までの差分のファイルと、そのtaskの予想するファイル（決定1と同じ規則）を合わせたもの。宣言の無いtaskのclaim直後のrunは差分が空なので、予想するファイルを入れないと同じpassの次の候補とぶつかる
3. **hotspot**: `stats`の`conflict_hotspots`のうち`alert`になるファイル（mainから消えたものを除き、名前が変わったものは今の名前）。閾値は`dagq.toml`の`[conflicts]`の`hotspot_conflicts`（既定3回）と`hotspot_ratio_percent`（既定20%）で、alertと同じく両方を満たすもの。回数か割合の片方だけにすると、着地が少ない間の1回の衝突（割合100%）や、よく変わる大きなファイルの少ない割合の衝突でも控え、控えが増えて並列度を落とす。alertとplan reviewのpromptと同じ基準にし、閾値を1か所で調整できるようにする
4. **控える**: 候補を順に見て、予想するファイルと、どれか1つの進行中のrunのファイルが同じhotspotを触れば（globが一致すれば触るとみなす）、そのpassではそのtaskをclaimせず、次の候補に進む。claimするたびに進行中のrunを読み直すので、同じpassでclaimしたrunとも重なりを見る。重ならない次の候補はそのままclaimする
5. **interruptは控えない**: 効く優先度（`effective_priority`）が`interrupt`のtaskは重なっても控えず、控えていたtaskがinterruptになれば控えを終える
6. **上限**: 控えは`[conflicts]`の`defer_max_secs`（既定3600秒）で終わる。数え始めは最初の`claim_deferred`で、supervisorが入れ替わっても続く（queueのeventから読み直す）。上限を過ぎたtaskは重なっていてもclaimし、そのtaskが次にclaimされるまで控え直さない。既定の1時間は、この queue のrunのwork時間の中央値が約24分（2026-09-26の`dagq stats`）で、邪魔なrunが作業して着地まで進むのに足り、控えたtaskの待ちがrun 2本分程度に収まる値
7. **他にclaimできるtaskが無く、slotが空いているとき**: 控えたまま待つ（直列にする）。衝突は着地待ちを数時間にするが、控えの待ちは上限の1時間までで、邪魔なrunが着地すれば次のpassでclaimされる。空いたslotは`stats`のalert `claim_deferred`で見える
8. **記録**: 控え始めたときにtaskのeventの`claim_deferred`（`reason: hot_files`、`files`（重なったhotspot）、`runs`（`[{run_id, task_id}]`）、`max_secs`、`message`、`supervisor`）を1回書き、控えが終わったときに`claim_deferral_ended`（`reason`、`why`、`deferred_secs`、`supervisor`）を書く。`why`は`cleared`（重ならなくなった、またはinterruptになった）、`expired`（上限を過ぎた）、`not_candidate`（候補から外れた: 他のsupervisorがclaimした、cancel、依存が戻った）。控えている間は書き直さない。load averageの控え（task 327の`claim_held`）はqueue全体のeventで、こちらはtaskのeventなので、2つは別のkindにし、`status`と`stats`の出し方をそろえる
9. **`status`と`stats`**: `status`は今控えているtask（taskの最新の`claim_deferred` / `claim_deferral_ended` / `run_claimed`が`claim_deferred`のもの）を`claim_deferrals`（`task_id`、`reason`、`since`、`files`、`runs`、`supervisor`）に出す。`stats`は`claim_deferrals`に、windowの中で始まった控えの`count`と`secs`、終わり方ごとの`by_end`（`cleared` / `expired` / `not_candidate` / `claimed` / `open`）、hotspotごとの控えた回数`by_file`、今の控え`deferred`を出す。控えは次の`claim_deferral_ended`か、そのtaskの`run_claimed`で終わる。空きslotがあり控えているtaskがあれば、alert `claim_deferred`（`value`は控えているtaskの数）を`idle_slots`の代わりに出す（load averageの控えのalert `claim_held`が先）
10. **読み直しの間隔**: hotspotの計算はqueueの全eventとmainの履歴を読むので、supervisorは10分ごとに読み直し、taskの予想するファイルも同じ間隔で読み直す。進行中のrunのファイルは60秒ごとか、claimの後に読み直す。hotspotが無いか、進行中のrunがどのhotspotも触らなければ、候補のファイルは読まない

## Consequences

- hotspotを触る2つのtaskが同時に走らなくなり、rebaseの衝突と着地待ちが減る見込み。効果は`stats`の`conflict_hotspots`と`claim_deferrals`の前後で見る
- 予想が外れると、実際には触らないtaskを控える（最大1時間）か、触るtaskを控えない。宣言の`--paths`が広いglob（`docs/**`など）だと控えやすくなる
- 同じqueueに2つのsupervisorが居ると、それぞれが同じtaskの`claim_deferred`を書くことがある。`stats`は後の方で前の控えを終える（`superseded`）
- 控えの設定は`dagq.toml`の`[conflicts]`に入るので、変えたら`down --wait` → `up`で読み直す
