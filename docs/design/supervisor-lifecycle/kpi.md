---
id: design-supervisor-lifecycle-kpi
type: design
title: "`kpi`"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-stats
  - design-supervisor-lifecycle-marks
  - design-supervisor-lifecycle-run-environment
  - adr-0051
  - adr-0049
  - adr-0048
---

# `kpi`

`dagq kpi [--period day|week] [--last N] [--at <cursor>] [--since <cursor>] [--until <cursor>] [--kind KIND]... [--by AXIS]... [--compare <mark|cursor|A..B,C..D>] [--window DAYS] [--goal ID]`は、[ADR-0051](../../adr/0051-kpi-time-series-report-and-push.md)の決定1〜9・14〜19の実装（task 430）。KPIを決まった規則でrun_eventsから再導出し、日（hostのlocal timezoneの0時から）かISO週（月曜0時から）ごとに並べ、taskの種類とclaim時の属性で層別し、前の期間と比べ、目標で判定し、変更の印の前後を比べる読むだけのコマンドで、JSONを返す。KPIの表は持たない。

- 集計は`domain::kpi::kpi`（events、task→goal、task→kind、今の時刻、UTCからのoffset、hostの論理コア数、`[kpi]`の設定を受ける純粋関数）が行う。期間ごとの窓は`domain::stats::stats`をその窓（`--since`/`--until`と同じcursorの`(start, end]`）で`full`に呼んで得るので、runの区間（`startup`・`work`・`validate`・`wait_to_land`）、`land_phases`、`retries`、runごとのsession、askの数、`backend_failures`、`auto_repairs`、sessionのkindごとの窓の値は[stats](stats.md)と同じ関数から出る。KPIのために足したのは、`stats`の中の同じ走査を使う読み口`stats::asks::human_waits`（人が答えたaskの`ask_opened`→`ask_answered`と→適用の秒。`runtime_closed`は除く）と`stats::measures::verification_durations`（`integrate`の検証コマンドごとの秒の並び。`verification_commands`と同じeventの選び方を共有する）だけ。
- `application::kpi::kpi`が`Queue`の`all_events`・`task_goals`・`task_kinds`を読み、`src/compose.rs`の`OneShot::kpi_of(queue, db, query)`が今の時刻、timezone（`infrastructure::clock::local_utc_offset`。`TZ`、無ければsystemのzoneで、今の時刻のoffsetを全期間に使う。夏時間の切り替えをまたぐ期間は1時間ずれる）、コア数（`available_parallelism`）、設定を渡す。
- 読み取りのコマンドなので、read-onlyの接続で開き、observerとheadlessのjob（review・triage・plan review）も打てる（`main.rs`の`reads_only`と`observer_access`）。

## 期間と比較

- 既定は日で、`--at`（無ければ今）を含む期間を最後に`--last`（既定7）期間を古い順に並べる（`periods`）。各期間は`label`（`YYYY-MM-DD`か`YYYY-Www`）、`start` / `end`（UTC）、`partial`（まだ終わっていない。値は途中までで、目標の判定に使わない）、`runs`（その期間に終わったrun）、`kpis`、`details`、`unavailable`、`marks`（その期間に効いた印。[変更の印](marks.md)の`marks`と同じ形）、`comparison`を持つ。
- `comparison`はKPIと層ごとに、前の同じ長さの期間の値（`previous`）、差（`delta`）、比（`ratio`。前が0ならnull）、日なら直前7日の日ごとの値の中央値（`baseline_7d`）、判定できたか（`judged`）とその理由（`reason`: `no_value` / `small_sample` / `partial`。まだ終わっていない期間は値と差を出すが判定しない）、良い向きのあるKPIの`verdict`（`improved` / `worsened` / `unchanged`）。比べる値は、値（`value`）を持つKPIはその値、分布だけのKPIは中央値。どちらかの`n`が`min_samples`に満たなければ判定しない（数えるKPI（`landings`など）は`n`によらず判定する）。
- `--since` / `--until`は1つの窓（`label: window`、`period: window`）を出し、同じ長さの直前の窓と比べる。`--at`とは併用できない。
- `--kind`は`kind=`の層をその種類だけに絞って出す（`all`と他の軸は残る）。比較の`summary`の種類にもなる。

## KPIと層

`kpis`はKPIの名前→層→値。層は`all`、`kind=<docs|plugin|runtime|ci|unknown>`（`kind`がnullのtaskは推さずに`unknown`）と、`--by`の軸`build=`（build識別子）・`parallel=`・`slot=`（claim時の使用中のslot ÷ `parallel`が`low` <0.5 / `mid` <1 / `full`）・`load=`（claim時のload average ÷ hostの論理コア数が`low` <1 / `mid` <2 / `high` <4 / `extreme`。コア数はclaim時に記録されていないので計算時のhostの値）・`toolchain=`（`rustc`のreleaseとhost）・`claude=`。記録の無い属性は`unknown`。値は`n`（標本数）と、値のKPIは`value`、分布のKPIは`median`・`p90`・`min`・`max`（中央値とp90は`stats`と同じ規則）。記録の無い値はnullで、0と区別する。

| KPI | 規則 | 層 |
| --- | --- | --- |
| `landings` | 期間に`run_integrated`で終わったrunの数 | runの層 |
| `lead_time` | taskが最初に`ready`になった`task_status_changed`から着地まで | runの層 |
| `phase.startup` / `phase.work` / `phase.validate` / `phase.wait_to_land` | 着地したrunの`stats`の区間 | runの層 |
| `land_phase.<工程>` / `land_phase.push` | 着地したrunの`land_phases`。長い裾の工程ごとの割合は`details.land_phase_tail_share` | runの層 |
| `first_pass_rate` | 期間に完了したtask（着地したrun）のうち、runが1つで、そのrunの`resume_started`・`revise_requested`・`integration_deferred`が0のもの | runの層 |
| `revise_rate` | 着地したrunのうち`revise_requested`があったもの | runの層 |
| `conflict_rate` | `integrate`を試みたrunのうち`rebase_conflict`で延期されたことがあるもの | runの層 |
| `verification_failed_rate` | 終わったrunの`integration_rebased`（検証に進んだ試行）のうち、`verification_failed`で延期されたもの | runの層 |
| `resumes_per_run` | 終わったrunの`resume_started`の平均。理由ごとは`details.resume_outcomes` | runの層 |
| `failed_rate` | 終わったrunのうち`failed` / `interrupted` | runの層 |
| `session_open.worker` / `session_active.worker` | 終わったrunのworker sessionの開いている時間・稼働時間（task 385・386）。記録の無いrunは標本にしない | runの層 |
| `asks_per_landing` | 期間の`ask_opened` ÷ `landings`。taskのあるaskは種類でも分ける。理由ごとは`details.asks_by_reason_category` | `all`と`kind=` |
| `attentions_per_landing` | 期間の`event_attention`がattentionと判定するevent ÷ `landings` | `all` |
| `ask_wait` / `ask_apply_wait` | 人が答えたaskの`ask_opened`→最初の`ask_answered` / →答えを適用したevent（task 468）。`stats`の`asks.times`と同じく、答え（適用）の時刻でその期間に入れる（ADR-0051の決定1の「askは`ask_opened`の時刻」は数（`asks_per_landing`）に当て、待ちは終わった時点で数える） | `all` |
| `backend_failures_per_run` | 期間の`backend_call_failed` ÷ 終わったrun。`op`ごとは`details.backend_failures_by_op` | `all` |
| `max_load_avg` | 期間のclaim時のload averageと`backend_call_failed`の`load_avg`の最大（`value`）と、claim時の分布 | `all` |
| `auto_repairs` | 期間の`auto_repaired`の数。`layer`ごとは`details.auto_repairs_by_layer` | `all` |
| `verify_command.<コマンド>` | `integrate`の検証コマンドごとの秒 | `all` |
| `slot_usage` | runがslotを占めた時間 ÷ （`parallel` × supervisorが生きていた時間）。runはclaimから終わり（`run_integrated`、`succeeded` / `failed` / `interrupted`）まで、`run_waiting_started`→`run_slot_regained`を除く。supervisorは`supervisor_started`から、同じsupervisorの`supervisor_stopped`か次の`supervisor_started`（どのsupervisorでも）か今まで。runの占有はsupervisorが生きていた区間に限る（印の記録より前と、終わりの記録が無いrunを数えない）。supervisorを複数同時に動かすと分母が小さく出る。staleになって停止の印も次の起動の印も無いsupervisorは今まで生きていたことになり、分母が大きく出る（heartbeatは記録に残らない）。`n`は占有のあったrun | `all` |
| `candidates` | `candidates_sampled`（`candidates`・`free_slots`・`ready`）の時間で重み付けた平均（`value`）と最大、空きslotがあるのに`candidates`が0で`ready`が残った秒（`details.candidates.starved_secs`） | `all` |
| `findings_open` / `finding_resolve_time` | 期間の終わりに`open` / `proposed`のfinding（`finding_recorded`と`finding_status_changed`から）、期間に`resolved`になったものの最初の記録からの秒。記録・解決の数は`details.findings` | `all` |
| `improvement_proposals` | 記録が無い（ADR-0051の決定25の数え方の実装が無い）ので値はnull | `all` |
| `session_open.<kind>` / `session_active.<kind>` / `session_active_ratio.<kind>` | worker以外のsessionの、窓に重なった時間の合計と稼働の割合（`stats`の`sessions.by_kind`） | `all` |

- `unavailable`は記録の無いKPIとその理由: `candidates`の`no_samples`（`candidates_sampled`を記録するsupervisorはまだ無い）、`improvement_proposals`の`not_recorded`。
- `--goal`はそのgoalのtaskのrun・ask・findingだけを数える（`slot_usage`の分母はqueue全体のまま）。

## 目標（`targets`）

- 設定は`[kpi]`（`min_samples` 既定5、`breach_periods` 既定3、`breach_weeks` 既定2、`max_improvement_proposals` 既定2）と`[kpi.targets."<KPI>"]`（`kind`（省略で`all`）、`stat`（`median` / `p90` / `value`。省略で値のKPIは`value`、分布のKPIは`median`）、`min` / `max`の少なくとも一方）。同じKPIの別の種類の目標は`[kpi.targets."<KPI>".<label>]`に書く。解析は`infrastructure::kpi_config`。
- 置き場所はmain checkoutの`dagq.toml`（repositoryの方針。`load_kpi_settings`）と、host.toml（`<queue dir>/host.toml`と`$XDG_CONFIG_HOME/dagq/host.toml`（無ければ`~/.config/dagq/host.toml`）。queueのファイルがキーごと・目標ごとに優先。`load_host_kpi`。host.tomlの他の表（`[push]`・`[report]`）はここでは読み飛ばす）。host.tomlの値がdagq.tomlより優先し、`max_improvement_proposals`だけはdagq.tomlの値。どこから来たかは`config.sources`と各目標の`source`（`repository` / `host`）。既定の目標値は持たない。
- 判定は`--last`の期間と、その前の日なら7日・週なら2週（`breach_periods` / `breach_weeks`の2倍の方が長ければその数）も含めて古い順に行う。完結した期間で、値があり`n`が`min_samples`以上（数えるKPIは`n`によらない）のものだけを判定し、目標を外れた判定が`breach_periods`（週は`breach_weeks`）回続けば`breach`、続きが足りなければ`missed`、最後の判定が目標内なら`ok`、1つも判定できなければ`not_judged`。判定できない期間は連続を切らず数えもしない。`streak`・`breach_since`と、並べた期間ごとの`value`・`n`・`judged`・`reason`（`partial` / `no_value` / `small_sample`）・`met`を出す。目標割れの始まりと解消のevent、push、observerのfindingは後続のtaskが持つ。

## 前後比較（`compare`）

- `--compare <event id>`はその印（`dagq mark`の`--at`の印は効いた時刻。導く印はそれを読んだclaimの`run_claimed`のevent ID（`marks`の`detail.claim_event`）で指す）、`--compare <時刻のcursor>`はその時刻を境に、前後に`--window`日（既定7）の窓を作る。`--compare A..B,C..D`は2つの窓を明示する（前の窓が後の窓より前で、どちらも始まりが終わりより前）。
- 印の並び（取り消された印と取り消しの印を除く、記録する印と導く印）を時刻の順にたどり、前の印からその印までに終わったrunが`min_samples`に満たなければ同じ「重なった変更」にまとめる（3つ以上も1つに。`domain::kpi::compare::overlapping_groups`）。境の印がまとまった変更に入っていれば、その最初の印の前と最後の印の後で比べ、`split.separable: false`で「含まれる印を分けられない」ことを示す。
- 出力は`split`（境の時刻と印）、`before` / `after`（窓と、そこで終わったrunの数、`partial`）、`confounders`（境の変更以外で、2つの窓の中と間にある印を時刻の順に、`position`: `before` / `between` / `after`）、`overlapping`（範囲にかかる重なった変更のまとまり）、`strata`（KPI→層→`before`・`after`の値（`n`・中央値・p90・範囲）と`comparison`と同じ差と判定。層は`all`と`kind=`・`parallel=`・`load=`・`build=`。後の窓が今を越えていれば`partial`で判定しない）、`summary`（`--kind`の種類、既定`runtime`の`kind=`の層の`lead_time`・`phase.*`・`land_phase.*`）。区間は自動では縮めない。
