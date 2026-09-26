---
id: adr-0051
type: adr
title: KPIを決まった規則でrun_eventsから導き、期間・taskの種類・変更の印で比べ、目標割れを判定し、supervisorが日次のHTMLとJSONのレポートを書き、ホストの設定のコマンドにpushし、KPIのfindingから作る改善のproposalの数に上限を付ける
status: accepted
created: 2026-09-26
updated: 2026-09-26
accepted_on: 2026-09-26
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - observer
  - operations
related:
  - adr-0029
  - adr-0032
  - adr-0034
  - adr-0045
  - adr-0047
  - adr-0049
  - adr-0062
  - design-supervisor-lifecycle-stats
  - design-supervisor-lifecycle
---

# ADR-0051: KPIを決まった規則でrun_eventsから導き、期間・taskの種類・変更の印で比べ、目標割れを判定し、supervisorが日次のHTMLとJSONのレポートを書き、ホストの設定のコマンドにpushし、KPIのfindingから作る改善のproposalの数に上限を付ける

## Context

`dagq stats`（[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定5、[stats](../design/supervisor-lifecycle/stats.md)）は、終わったrunの直近50件をそのつど集計する。次のことができない。

- **期間ごとの推移。** 日・週の値を並べて、前の期間と比べる手段が無い。`--since` / `--until`（task 466）で窓は切れるが、窓を並べるのは人の手作業になる。
- **変更の前後の比較。** supervisorの`--parallel`を4→3にした（task 427）、sccacheを入れた（ADR-0049の決定6）、hostのRust toolchainをRosettaのx86からmiseのarm64にした、などの前後を比べるには、人が変更の時刻とcursorを控えておく必要があった（goal 36のnote 8718・8810、task 460の文書での前後比較）。変更がいつ入ったかがqueueに残らない。supervisorの起動・停止はrun_eventsに載らない（[domain-model](../design/domain-model.md)）。
- **taskの種類を分けた集計。** docsだけのtaskとruntimeのtaskでは`work`や`wait_to_land`の桁が違う。今の中央値は混ざったまま出るので、taskの混ざり方が変わるだけで数字が動く。
- **目標との比較。** 目標値の置き場所も、目標を割ったと判断する規則も無い。
- **人が見る画面と、画面の前に居ない人への通知。** cmuxの通知はinbox宛てで、KPIを見る画面は無い。[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定22は`dagq report`を「CLIの出力が揃ってから、別のgoalで決める」とした。goal 40がそれにあたる。
- **KPIの悪化から改善へ。** observerはADR-0047の決定18〜21でfindingを書き、決定19の経路でruntimeのplannerがproposalを作るが、KPIの悪化を種類として持たず、改善のproposalが同時にいくつ動くかにも柵が無い。

人の決定（2026-09-26、goal 40のconstraints）:

- pushはruntimeがホストの設定に書いたコマンドを呼び、まとめをstdinで渡す。ntfy・Slackなどは設定例として書き、runtimeは特定のサービスに依存しない。
- 人がKPIを見るのはローカルのHTML（queueのディレクトリに日次で書き、JSONも並べる）で、外部のサービスには送らない。
- KPIから生まれる改善はADR-0047の決定19・20どおりplan reviewのpassで`ready`にするが、改善のproposalの同時の数に上限を付け、優先度は`normal`以下にする。
- webhookのURLなどのsecretは、commitされる`dagq.toml`に置かない。
- KPIはtaskの種類（docs / plugin / runtimeなど）を分けて集計する。
- LLMで数字を作らない。集計は決まった規則で行う。

2026-09-26に人がinbox経由で、前後比較の交絡の扱い（下の決定14〜16）と、自動の印にhostのRust toolchainと`[run.env]`のhashを足すか（決定10・11）をこのADRで決めることを加えた（desk(spike)の検討の案6・7。task 460が文書で手で行った前後比較を自動にする）。

関係する他のtask（このADRが前提にし、重ねて記録しないもの）:

- **task 196**: `tasks`に明示の`kind`列を足す（`add --kind` / `edit --kind`。値の集合は[ADR-0029](0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)の変更の種類）。
- **task 197**: claimのrun eventにbuild識別子（[ADR-0045](0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)の決定2）・Claude Codeのversion・`parallel`・使用中のslot数・load averageを記録し、`integrate`の検証コマンドごとの所要時間を記録する。
- **goal 35（ADR-0048）のtask 385・386**: sessionのkindごとの開いている時間と、transcriptから読むsessionの稼働時間（turnの合計）。
- **task 394**: `wait_to_land`の工程別の内訳（`land_phases`。着地済み）。
- **task 468**: askのanswerと、answerが適用されるまでの時間。

ADR-0047は後で[ADRの棚卸し](../plans/adr-inventory.md)のtask 451が書くADR-0064に丸ごと置き換わる予定である。このADRはADR-0047の決定番号で引く。ADR-0064が`accepted`になった後は、ADR-0064の対応表でADR-0047の決定番号から対応する決定を辿る。ADR-0044は`superseded`（後継はADR-0047で、決定1〜23は同じ番号）なので、goal 40の記述の「ADR-0044の決定22」などはADR-0047の同じ番号の決定と読む。

## Decision

**原則。** KPIはrun_events・asks・findings・tasksから決まった規則で再導出し、KPIのための集計の表は持たない（ADR-0049の決定5と同じ）。runの区間や工程の区切りは`stats`と同じ関数を使い、KPIのために別の区切りを定義しない。新しく記録するのは、runの無い時点の変化（変更の印）と、runの無い時点の値の標本（決定3の`candidates`）だけにする。runごとの属性はtask 197のclaim時の記録から読み、同じ値を二重に記録しない。以下の27点を決める。eventのkindの名前、JSONのフィールド名、コマンドの細かい引数の形は、ここで名前を挙げたものも含めて実装taskが決めてよい（決めた名前はdesign文書に書く）。

### I. KPIの定義

1. **KPIは、流れ・手戻り・人の負担・基盤・改善の回り方・sessionの時間の6群にし、次の一覧を初めの集合にする。**
   値の規則は決定2〜4、層別は決定5〜7。中央値と90パーセンタイル（p90）は`stats`と同じ規則にする（中央値は偶数個なら中央2つの平均の切り捨て、p90はnearest-rank: 昇順でceil(0.9×n)番目）。率は分母が0なら値をnullにする。

   | 群 | KPI | 規則 | 良い向き |
   | --- | --- | --- | --- |
   | 流れ | `landings` | 期間に`run_integrated`を記録した着地の数 | 大きい |
   | 流れ | `lead_time` | taskが最初に`ready`になった時刻（plan reviewの`ready`、`ready --bypass-review`、または復旧jobのretryで`ready`に戻ったときは最初の`ready`のまま）から、そのtaskを`completed`にした`run_integrated`まで。中央値とp90 | 小さい |
   | 流れ | `phase.<工程>` | 着地したrunの`startup`・`work`・`validate`・`wait_to_land`（`stats`の区間）の中央値とp90。reviewの時間は`land_phases`の工程として読む | 小さい |
   | 流れ | `land_phase.<工程>` | 着地したrunの`land_phases`（task 394）の工程ごとの中央値・p90と、長い裾（`tail_runs`）の中での工程ごとの`tail_total`の割合 | 小さい |
   | 流れ | `verify_command.<コマンド>` | `integrate`の検証コマンドごとの所要時間（task 197の記録）の中央値とp90。コマンドの文字列で分ける | 小さい |
   | 流れ | `slot_usage` | 決定3 | 大きい |
   | 流れ | `candidates` | 決定3 | 目標の範囲（多すぎれば詰まり、0で空きslotがあれば飢え） |
   | 手戻り | `first_pass_rate` | 期間に`completed`になったtaskのうち、runが1つだけで、そのrunの`resumes`・`revise_requested`・`integration_deferred`がどれも0だったものの割合 | 大きい |
   | 手戻り | `revise_rate` | 着地したrunのうち`revise_requested`が1回以上あったものの割合 | 小さい |
   | 手戻り | `conflict_rate` | `integrate`を試みたrunのうち、rebaseの衝突で延期（`integration_deferred`の理由が衝突）されたことが1回以上あるものの割合 | 小さい |
   | 手戻り | `verification_failed_rate` | `integrate`の検証の試行のうち、検証コマンドが失敗したものの割合 | 小さい |
   | 手戻り | `resumes_per_run` | 終わったrunの`resume_started`の数の平均と、理由ごとの内訳（`stats`の`resume_outcomes`） | 小さい |
   | 手戻り | `failed_rate` | 終わったrunのうち`failed` / `interrupted`で終わったものの割合 | 小さい |
   | 人の負担 | `asks_per_landing` | 期間の`ask_opened`の数 ÷ `landings`。人が要る理由の分類（ADR-0047の決定41）ごとの内訳も出す | 小さい |
   | 人の負担 | `attentions_per_landing` | 期間にattentionになったevent（`domain`の`event_attention`がattentionと判定するもの）の数 ÷ `landings` | 小さい |
   | 人の負担 | `ask_wait` | `ask_opened`から`ask_answered`までの中央値とp90（人が答えたものだけ。runtimeが閉じたものは除く）。answerの適用までの時間はtask 468の記録から同じ形で足す | 小さい |
   | 基盤 | `backend_failures_per_run` | 期間の`backend_call_failed`の数 ÷ 終わったrunの数。`op`ごとの内訳 | 小さい |
   | 基盤 | `max_load_avg` | 決定3 | 小さい |
   | 基盤 | `auto_repairs` | 期間の`auto_repaired`の数（ADR-0047の決定45の`layer`・`repair`ごと） | 参考（向きを持たない） |
   | 改善 | `findings_open` | 期間の終わりの時点で`open` / `proposed`のfindingの数（ADR-0047の決定18）と、期間に新しく作られた数・`resolved`になった数 | 小さい |
   | 改善 | `finding_resolve_time` | 期間に`resolved`になったfindingの、最初に見た時刻から`resolved`までの中央値とp90 | 小さい |
   | 改善 | `improvement_proposals` | 期間の終わりの時点で動いている改善のproposalの数（決定25の数え方） | 参考 |
   | session | `session_open.<kind>` | goal 35のtask 385が記録するsessionのkind（worker・resume・revise・inbox・plannerなど）ごとの開いている時間。workerのkindはrunごとの中央値とp90、常駐・オンデマンドのkindは期間の合計 | 小さい（worker）、参考（その他） |
   | session | `session_active.<kind>` | goal 35のtask 386がtranscriptから読む稼働時間（turnの合計）の、同じ形の値。`session_open`との比（稼働の割合）も出す | 参考 |

   - **流れのKPIは、着地したrunを着地の時刻でその期間に入れる。** 手戻り・基盤の率は、終わりのevent（`stats`の`finished_event_id`）の時刻で入れる。askは`ask_opened`の時刻、findingは状態が変わった時刻で入れる。
   - **記録の無い値はnullにし、0と区別する。** 385・386・197・468の記録が入る前のrunは、そのKPIの標本に数えない（nは減る）。
   - KPIを足すときは、この表の規則の形（入力のevent、分子と分母、期間への入れ方、良い向き）を決めて足す。規則を変えずにKPIを足すことは、このADRの範囲の実装として行ってよい。既存のKPIの規則を変えるときは、このADRを置き換える。

2. **KPIの計算はdomainの純粋関数1つにまとめ、CLI・レポート・observerが同じものを使う。**
   入力はevents・tasks（`kind`、状態）・asks・findings・変更の印・今の時刻・期間の指定で、出力はKPIの値と層別と比較（決定8、14〜16）と目標の判定（決定18）。`stats`の区間と`land_phases`を計算する関数を呼び、同じ規則を2か所に書かない。

3. **`slot_usage`と`max_load_avg`はtask 197のclaim時の記録から導き、`candidates`だけを新しい標本として記録する。**
   - **`slot_usage`**: 期間の中で、runがslotを占めた時間の合計 ÷ （生きているsupervisorの`parallel`の合計 × supervisorが生きていた時間）。runの占有はclaimからslotを離れるまで（着地・失敗・中断。[ADR-0062](0062-runs-waiting-for-a-person-leave-the-slot.md)で人の答えを待つためにslotを外れた間は除く）。`parallel`とsupervisorの生きていた区間は、決定10のsupervisorの起動・入れ替え・停止の印から読む。task 197がclaim時に記録する使用中のslot数と`parallel`は、runごとの層別（決定6の`slot`の帯）に使い、`slot_usage`の分母を作るためにもう一度記録しない。
   - **`max_load_avg`**: 期間のclaim時のload average（task 197）と、`backend_call_failed`の`load_avg`の最大。claim時のload averageの中央値とp90も並べる。
   - **`candidates`**: claimできる`ready`のtaskの数（`graph`の`candidates`）と、空きslotの数。どちらもrunの無い時点の値なので、task 197の記録からは導けない。supervisorは周回のたびにこの2つを計算し、前に記録した値から変わったときだけ標本のeventを1件記録する（値が変わらない間は書かない）。KPIは期間の中の時間で重み付けた平均と最大、「空きslotがあるのに`candidates`が0で`ready`のtaskが残っていた時間」（`stats`の`idle_slots`のalertを時間で測ったもの）を出す。

4. **goal 35とtask 394の値はKPIに含め、集計するtask 430はそれらの着地を待つ（依存にする）。記録の無いrunの値はnullにする。**
   `session_open`・`session_active`（task 385・386）、`land_phase`（task 394）、`verify_command`とclaim時の属性（task 197）、`ask_wait`の適用までの時間（task 468）を入力にするKPIは、KPIの実装（task 430）がそれらのtaskに依存してclaimされるので、記録を自分で作らない。それらが着地した後も、記録が入る前に終わったrunは標本に数えず、記録が1つも無い期間の値はnullにする。

### II. taskの種類と層別

5. **taskの種類はtask 196の`kind`列だけで決め、`kind`がnullのtaskは`unknown`に数える。**
   - 第一の根拠はtask 196が`tasks`に足す明示の`kind`（`add --kind` / `edit --kind`。値の集合はADR-0029の変更の種類）で、KPIはtaskの今の`kind`を使う（`kind`を変えられるのは`draft` / `submitted`の間だけなので、claimの後は変わらない）。
   - `kind`がnullのtask（task 196より前に登録されたtaskなど）は、宣言した`--paths`・着地の差分のパス・verificationの中身のどれからも推さず、`unknown`という種類に数える。推す規則は196の規則とは別の分類の規則になり、同じtaskが根拠によって別の種類に数えられる。`unknown`の件数はレポートに出るので、古いtaskの割合が分かる。
   - 種類ごとの集計に加えて、全体（`all`）も出す。前後比較の既定の比べ方は決定15。

6. **KPIは種類に加えて、runの属性で層別できる。**
   層別の軸は、`kind`（決定5）と、task 197のclaim時の属性から導く`build`（build識別子）・`parallel`・`slot`（claim時の使用中のslot数 ÷ `parallel`を`low`（<0.5）/ `mid`（0.5〜<1）/ `full`（1）の帯にしたもの）・`load`（claim時のload average ÷ hostの論理コア数を`low`（<1）/ `mid`（1〜<2）/ `high`（2〜<4）/ `extreme`（4以上）の帯にしたもの）・`toolchain`（決定10）・`claude`（Claude Codeのversion）。hostのコア数はclaim時に記録されていなければKPIの計算時のhostの値を使う。帯の境界の値は実装taskが`[kpi]`の設定（決定17）で変えられるようにしてよい。

7. **runの属性を持たないKPIは、層別しない。**
   `candidates`・`slot_usage`・`findings_open`・`session_open`のinbox / plannerなど、runに紐づかないKPIは`kind`などの軸で分けず、全体の値だけを出す。askはrunかtaskに紐づくものだけを`kind`で分け、紐づかないもの（taskの無い`blocked`）は`all`にだけ数える。

### III. 期間と比較

8. **期間は日と週にし、前の期間と比べる。**
   - **日**はhostのlocal timezoneの0時から24時間、**週**はISO週（月曜0時から7日）。timezoneはKPIの計算の入力で、既定はhostの`TZ`（無ければsystemの設定）。
   - 各期間の値に、前の同じ長さの期間の値と、その差と比（`delta`・`ratio`）を並べる。日の値には、直前の7日の日ごとの値の中央値（週の基準）も並べる。
   - 期間の指定は`--period day|week`と、区切りを動かす`--at <cursor>`（その時点を含む期間）、`--last N`（直近N期間を並べる）。任意の窓は`--since` / `--until`（`stats`と同じcursor。task 466）で切る。まだ終わっていない今日・今週は、途中の値として`partial: true`を付けて出し、目標の判定（決定18）には使わない。

9. **`dagq kpi`が集計を返す読み取りのコマンドで、JSONを既定にする。**
   `dagq kpi [--period day|week] [--last N] [--at <cursor>] [--since <cursor>] [--until <cursor>] [--kind KIND]... [--by kind|build|parallel|slot|load|toolchain|claude]... [--compare <markか区間>] [--goal ID]`。`--compare`は決定14の前後比較。`--human`などの読みやすい出力の形は実装taskが決める。observer・planner・plan reviewも同じコマンドを読む（ADR-0047の決定22と同じく、observerに許す読み取りのコマンドに加える）。

### IV. 変更の印

10. **変更には、runの属性から導く印と、eventとして記録する印の2種類を使い、同じ変化を二重に記録しない。**
    分け方は「その値がrunごとに決まるか、runの無い時点で変わるか」にする。印は「いつ変わったか」、claim時の属性は「そのrunが何で動いたか」を表す。

    | 変化 | 印の種類 | 出どころ |
    | --- | --- | --- |
    | build識別子（固定バイナリの入れ替え） | 導く印 | task 197のclaim時のbuild識別子が、claimの順で前のrunと変わったところ |
    | Claude Codeのversion | 導く印 | task 197のclaim時の記録 |
    | `parallel` | 導く印 | task 197のclaim時の記録 |
    | hostのRust toolchain（`rustc`のversionとhostの`arch`） | 導く印 | task 197のclaim時の記録に足す属性（下の項） |
    | supervisorの起動・引き継ぎ（ADR-0045の決定10）・停止 | 記録する印 | supervisorが記録する。payloadに自分のbuild識別子・`parallel`・`mode`・自動更新の有無 |
    | main checkoutの`dagq.toml`の`[run.env]`のhash | 記録する印 | 決定11 |
    | 人が決めた変更（設定、運用、hostの変更など） | 記録する印 | `dagq mark`（決定12） |

    - **toolchainはclaim時の属性にする。** `rustc`のversionとarchはrunのbuildに効く値で、hostの`mise`の更新などdagqを通らない経路で変わるので、runtimeが「変わった時刻」を知る手段はclaimのときに読むことしかない。claim時にmain checkoutで`rustc -vV`を実行し、`release`と`host`を記録する（読めなければnull）。task 197が`ready`のまま着地した後に足す場合は、後続のtaskがclaim時の記録に属性を足す（このADRが決めた属性として扱い、197の記録と同じeventに載せる）。
    - **導く印はeventを書かない。** `dagq kpi`はclaimの順にrunの属性を並べ、前のrunと値が変わった最初のrunのclaimの時刻を印の時刻にする（印のkindは`derived:<属性>`、前後の値を持つ）。属性の記録の無いrunは飛ばし、nullから値への変化は印にしない。
    - **記録する印と導く印が同じ変化を表すときは1つにまとめる。** supervisorの起動の印（新しい`parallel`とbuild識別子を持つ）の後の最初のclaimで同じ属性が変わっていれば、導く印は出さず、記録する印の時刻を区切りにする。
    - supervisorの起動・停止は今はrun_eventsに載らない（[domain-model](../design/domain-model.md)）。この印のためにsupervisorの起動・引き継ぎ・`down`での停止を記録する（staleになったsupervisorは停止の印を書けないので、`kpi`は最後のheartbeatが止まった記録か次の起動の印で区間を閉じる）。

11. **`[run.env]`のhashの変化を記録する印にし、sccacheの有無はこれで拾う。**
    supervisorは起動時と周回のたびに（`[run.env]`を読む既存の経路で）、main checkoutの`dagq.toml`の`[run.env]`の表を正規化した内容（キーで並べた`KEY=VALUE`の並び）のhashを求め、最後に記録したhashと違うときだけ印を1件記録する。payloadはhashと、前のhashから変わったキーの名前の一覧（値は書かない。`[run.env]`にsecretを書く運用を想定しないが、印を通して値を広めない）。`RUSTC_WRAPPER = "sccache"`の追加・削除、`CARGO_BUILD_JOBS`の変更などはこの印に表れる。`[run.env]`以外の表（`[stall]`、`[kpi]`など）の変化は印にしない。必要なら人が`dagq mark`で残す。`[run.env]`はrunごとの属性にしない: claim時の値だけでは、そのrunの`integrate`が変更後の値で検証されたことが表せない。

12. **人は`dagq mark`で印を残す。**
    `dagq mark <label> [--note TEXT] [--at <cursor>]`。`label`は短い名前（例: `parallel 4→3`、`host arm64`）、`--note`は説明。`--at`を付ければ過去の時刻の印として残せる（変更を入れてから印を付け忘れたとき。時刻はcursorの規則で、印のevent自体は今の時刻で記録し、payloadに効いた時刻を持つ）。`dagq marks [--since] [--until]`で記録する印と導く印を時刻の順に並べる。印は取り消せないが、`dagq mark --retract <id>`で「取り消した」ことを別の印として記録でき、`kpi`は取り消された印を区切りに使わない。inbox・planner・人のどのsessionからも打てる。observerとjobには打たせない。

13. **変更の印はKPIの区切りにだけ使い、runの状態を変えない。**
    印はrun_eventsに記録し（runもtaskも持たないevent）、attentionにしない。

### V. 前後比較と交絡

14. **前後比較は印を境に2つの区間を作り、区間の間と中にある印をすべて並べて出す。**
    - `dagq kpi --compare <mark>`は、その印の前と後に同じ長さ（既定7日。`--window`で変える）の区間を作る。`--compare <cursor>..<cursor>,<cursor>..<cursor>`で2区間を明示できる。
    - 出力には、比べる2区間のそれぞれの中にある印と、2区間の間にある印を、記録する印も導く印もすべて時刻の順に並べる（`confounders`）。比べたい印以外の変化が同じ区間にあることを、数字の横で示す。task 460が文書で手で行った確認を自動にする。
    - 区間の中に比べたい印と別の印があっても、区間は自動では縮めない。縮めたいときは人が`--compare`で区間を明示する。

15. **同じ区間の中でも、種類・`parallel`・loadの帯・build識別子で層別して出す。**
    前後比較の出力は、全体（`all`）に加えて、`kind`ごと、`parallel`ごと、`load`の帯ごと、`build`ごとの値を区間ごとに並べる。前後で層の混ざり方が変わったときに、全体の差が層の入れ替わりで出たのか、同じ層の中でも変わったのかを読めるようにする。作業時間（`phase.work`など）の前後比較の要約は、既定では`runtime`の種類だけで行う（種類で桁が違うため。`--kind`で変えられる）。

16. **印が近すぎて区間が作れないときは、区間を作らずに「重なった変更」としてまとめる。**
    - 印と印の間の区間に入る標本（その区間で終わったrunの数）が最小標本数（決定19）に満たないとき、その2つの印は区切らず、1つの「重なった変更」（`overlapping`）にまとめ、含まれる印を並べる（例: task 393と427の着地は3分差で、間の着地は0 runだった）。3つ以上が続けば1つにまとめる。
    - `--compare`でまとめた印の片方を指したときは、まとめた変更全体の前後で比べ、「この比較は含まれる印を分けられない」ことを出力に示す。
    - 各区間と各層には、`n`（標本数）・中央値・p90・範囲（最小と最大）を必ず出す。`n`が最小標本数に満たない層・区間は値を出しても差の判定（改善・悪化の表示、目標の判定）をしない（`judged: false`と理由`small_sample`）。

### VI. 目標

17. **目標値は、repositoryの方針なら`dagq.toml`の`[kpi]`に、hostの事情ならホストの設定に置き、ホストの設定が優先する。**
    - **repositoryの方針**（commitされる）: main checkoutの`dagq.toml`の`[kpi.targets]`。`first_pass_rate`・`revise_rate`・`asks_per_landing`など、hostを変えても変わらない目標。runtimeが読むのはmain checkoutの作業ファイル（`[run.env]`と同じ）。
    - **hostの事情**（commitしない）: ホストの設定（決定22の`host.toml`）の`[kpi.targets]`。`phase.startup`・`phase.work`・`max_load_avg`など、hostの性能で決まる目標。同じKPIと層が両方にあればホストの設定の値を使い、どちらの値を使ったかをKPIの出力に書く。
    - 書式は、KPIと層ごとの表にする。例:

      ```toml
      [kpi]
      min_samples = 5          # 決定19。既定5
      breach_periods = 3       # 決定18。日の期間の連続数。既定3

      [kpi.targets."phase.work"]
      kind = "runtime"         # 省略すると all
      stat = "median"          # median / p90 / value
      max = 3600               # 上限（秒）。下限は min

      [kpi.targets.first_pass_rate]
      min = 0.6
      ```

    - 目標の無いKPIは目標の判定をせず、値と比較だけを出す。既定の目標値はruntimeに埋め込まない（repositoryとhostで違うため）。この repositoryの初めの目標値は、KPIの実装が着地した後にplannerが人と`dagq.toml`に書く。

18. **目標割れは、判定できる期間が続けて目標を外れたときにし、1期間の外れは「外れ」とだけ示す。**
    - 日の期間で、完結した期間（`partial`でない）のうち、標本数が最小標本数以上で目標を外れた期間が`breach_periods`（既定3）回続いたら、そのKPIとその層は**目標割れ**になる。週の期間は2回続けば目標割れにする（`breach_weeks`、既定2）。
    - 標本数が足りない期間は、判定しない期間として飛ばし、連続を切らない（数えもしない）。目標を満たした判定できる期間が1回あれば連続は切れ、目標割れは**解消**になる。
    - 1期間だけ外れたものは`missed`として出力とレポートに示すが、目標割れにはしない。
    - 判定の状態（`ok` / `missed` / `breach` / `not_judged`）と、目標割れが始まった期間・続いた期間の数を出力に持つ。判定は`kpi`の計算の中で毎回導き、状態の表は持たない。目標割れの始まりと解消は、push（決定23）とobserver（決定24）のためにsupervisorがeventとして記録する（同じKPIと層の同じ状態を二度記録しない）。

19. **最小標本数は`min_samples`（既定5）にし、判定・比較・区間づくりのすべてに同じ値を使う。**
    標本はKPIの分母の単位（runのKPIならrun、askのKPIならask、`first_pass_rate`ならtask）で数える。`[kpi]`の`min_samples`・`breach_periods`・`breach_weeks`はrepositoryの方針として`dagq.toml`に置き、ホストの設定の`[kpi]`で上書きできる（hostの標本の揺れ方に合わせるため）。`max_improvement_proposals`（決定25）だけはホストの設定で上書きしない。

### VII. 日次レポート

20. **supervisorは日に1回、queueのディレクトリにKPIのレポートをHTMLとJSONで書く。**
    - **時機**: 日の期間が終わった後の最初の周回（hostのlocal timezoneで0時を過ぎた後）に、前日の分を書く。supervisorが居なかった日は、次に起動したときに書いていない日の分をまとめて書く（遡るのは既定7日まで）。書いたことは`report_written`（kindの名前は実装taskが決める）としてeventに記録し、同じ日を二度書かない。週の期間が終わった日には週の分も書く。同時に書くのは1つのsupervisorだけにする（observerの起動と同じく、queueで1つ）。run slotを使わず、LLMを使わない。
    - **場所とファイル名**: `<queue dir>/reports/`の下に`daily/YYYY-MM-DD.json`と`daily/YYYY-MM-DD.html`、`weekly/YYYY-Www.json`と`weekly/YYYY-Www.html`（ISO週）。最新を開きやすいよう、`<queue dir>/reports/index.html`を書くたびに書き直し、日・週のレポートへのリンクを新しい順に並べる。ファイルは同じディレクトリの一時ファイルに書いてからrenameする。
    - **中身**: JSONは`dagq kpi --period day --at <その日>`と同じ形（KPIの値、層別、前の期間との比較、目標と判定、期間の中の印、`n`）に、生成したbuild識別子と時刻を足したもの。HTMLはJSONと同じ値を表と小さなグラフ（直近の期間の推移）で見せる。目標割れと`missed`を上に並べ、期間の中の印を推移の上に示す。
    - **自己完結のHTML**: 外部のscript、CSS、font、CDN、画像を読まない。CSSとSVGとデータは1ファイルに埋め込み、JavaScriptは使わなくても読めるようにする（使うなら埋め込みだけ）。queueのディレクトリの外に何も送らない（人の決定）。
    - **保持**: 日のレポートは既定90日、週のレポートは既定104週残し、古いものは書くときに消す。日数はホストの設定の`[report]`で変えられる。JSONは集計の結果の写しで、正はrun_eventsなので、消しても`dagq kpi`で作り直せる。

21. **人は`dagq report`で同じレポートを手で出す。**
    `dagq report [--period day|week] [--at <cursor>] [--out <path>]`は、supervisorと同じ関数で指定した期間のHTMLとJSONを書く（既定は`<queue dir>/reports/`の同じ名前で、`partial`の今日・今週も書ける。`partial`のレポートはファイル名に印を付けて、完結した日のファイルを上書きしない）。書いたpathを出力する。`--print json`でファイルを書かずにJSONを標準出力に出してもよい。これがADR-0047の決定22が先送りした`dagq report`にあたる。決定22の他の項（`findings`、`events --full`、`timeline`、`observe --history`）は変えない。

### VIII. push

22. **pushの設定はホストごとのファイルに置き、`dagq.toml`には置かない。**
    - **場所**: queueごとの`<queue dir>/host.toml`と、hostの全queueに効く`$XDG_CONFIG_HOME/dagq/host.toml`（`XDG_CONFIG_HOME`が無ければ`~/.config/dagq/host.toml`）。両方にあれば、表ごとにqueueのファイルが優先する。どちらもrepositoryの外にあり、commitされない。webhookのURLやtokenはここか、ここに書いたコマンドが読む環境変数・ファイルに置く。
    - **書式**:

      ```toml
      [push]
      command = ["/Users/me/.local/bin/dagq-push-ntfy"]   # argvの配列。shellを通さない
      timeout_secs = 30          # 既定30
      daily = true               # 日次のまとめを送る。既定true
      breach = true              # 目標割れの即時通知を送る。既定true
      max_breach_per_day = 3     # 即時通知の1日の上限。既定3

      [report]
      keep_daily_days = 90
      keep_weekly_weeks = 104

      [kpi.targets."phase.startup"]   # hostの事情の目標（決定17）
      stat = "median"
      max = 600
      ```

    - `[push]`が無いか`command`が空なら、runtimeは何も呼ばず、何も記録しない（レポートは書く）。
    - runtimeは`command`をshellを通さずに実行し、環境変数に`DAGQ_PUSH_KIND`（`daily` / `weekly` / `breach` / `resolved`）、`DAGQ_QUEUE`、`DAGQ_REPORT_HTML`・`DAGQ_REPORT_JSON`（書いたレポートのpath。無いときは空）を渡す。

23. **pushは日次のまとめと目標割れの即時通知を、決まった形でstdinに渡し、連発を抑える。**
    - **stdinの中身**: 1つのJSONオブジェクト（UTF-8、最後に改行）。`kind`、`queue`、`period`、`title`（1行の要約。例: `dagq 2026-09-26: 着地 12、目標割れ 1`）、`text`（数行のプレーンテキスト。サービスの本文にそのまま使える）、`breaches`（KPI・層・値・目標・続いた期間）、`report_html`・`report_json`のpath。受け取るコマンドはJSONを読んでも`text`だけを使ってもよい。
    - **日次のまとめ**: 日のレポートを書いた直後に1回（週のレポートを書いた日は週の分も1回）。
    - **目標割れの即時通知**: 目標割れの始まり（決定18の記録）ごとに1回。同じKPIと層の目標割れが続いている間は送らず、日次のまとめに載せるだけにする。解消した後に再び目標割れになれば、また送る。1日（local timezone）の即時通知が`max_breach_per_day`に達したら、それ以降の分は送らずに次の日次のまとめにまとめる。解消（`resolved`）は即時には送らず、日次のまとめに載せる。
    - **失敗の扱い**: 終了コードが0でないか、`timeout_secs`を過ぎた（process groupごと止める）ら失敗にする。失敗は`push_failed`とは別のkind（例: `kpi_push_failed`。名前は実装taskが決める）で、終了コード・stderrの末尾（上限付き）・試行の回数を記録する。再試行は同じ内容で2回まで、1分・5分の間隔を空けて行う。3回とも失敗したら、その内容は送らずに捨て、inbox宛てのattentionにする（人が要る理由の分類はADR-0047の決定41の`recovery_failed`。runtimeの再試行で直らず、pushのコマンドかその先のサービスを人が直す）。同じ失敗のattentionは、成功するまで新しく作らない。pushの失敗はレポートの書き込み、着地、claimを止めない。
    - **設定例**（design文書と`plugin`のskillに載せる。runtimeはどちらにも依存しない）:

      ```sh
      #!/bin/sh
      # dagq-push-ntfy: ntfyにtitleとtextを送る。topicはこのファイルか環境変数に置く
      json=$(cat)
      title=$(printf '%s' "$json" | jq -r .title)
      printf '%s' "$json" | jq -r .text |
        curl -fsS -H "Title: $title" --data-binary @- "https://ntfy.sh/${DAGQ_NTFY_TOPIC:?}"
      ```

      ```sh
      #!/bin/sh
      # dagq-push-slack: SlackのIncoming Webhookに送る。URLはrepositoryの外のファイルから読む
      url=$(cat "$HOME/.config/dagq/slack-webhook-url")
      jq '{text: (.title + "\n" + .text)}' | curl -fsS -H 'Content-Type: application/json' --data-binary @- "$url"
      ```

### IX. observerと改善のproposal

24. **ADR-0047の決定18のfindingに種類`kpi`を足し、observerは目標割れの継続をfindingにする。**
    - 種類`kpi`のfindingは、対象（`target`）を`queue`、`subject`を`<KPI>/<層>`（例: `phase.work/kind=runtime`）にする。ADR-0047の決定18の一致の規則（種類・対象・`subject`が同じ閉じていないfindingは1件）がそのまま効くので、同じKPIと層の目標割れは1件にまとまる。`summary`・`detail`には値・目標・続いた期間・期間の中の印（決定14の`confounders`）を書き、根拠のevent IDには目標割れの始まりのeventを入れる。
    - observerは入力に`dagq kpi`（直近の日と週の判定）を読み、`breach`の状態のKPIと層について`kpi`のfindingを作るか更新する。`missed`と`not_judged`はfindingにしない。目標割れが解消したら、observerは根拠（解消した期間）を付けてfindingを`resolved`にできる（ADR-0047の決定18の`resolved`の規則のまま）。数字はobserverが作らず、`kpi`の出力をそのまま引く。
    - ADR-0047の決定21の「変化の無いときは起動しない」の判定では、このADRで足すeventのうち、目標割れの始まり・解消と変更の印（`[run.env]`のhashと`dagq mark`、supervisorの起動・引き継ぎ・停止）は「observer自身のもの以外のevent」として数え、KPIの記帳のevent（決定3の`candidates`の標本、決定20のレポートを書いた記録、決定23のpushの成功の記録）は数えない。記帳のeventだけではobserverを起こさない。pushの失敗はattentionとしてinboxに届くので、これも数えない。
    - ADR-0047の決定19の経路(a)でproposalを求める印を付けるかはobserverが判断する（影響と、目標割れが続いた期間から）。ADR-0047の決定19の経路(b)のaskにはしない（目標割れはそれだけでは待っても解けない詰まりではない）。

25. **改善のproposal（findingから作ったproposal）の同時の数に上限を付ける。**
    - **数え方**: ADR-0047の決定19でfindingに紐づけてsubmitしたproposalのうち、taskがすべて終わって（`completed` / `canceled`）いないもの。findingの種類（`kpi`に限らない）を問わず、全部を1つの上限で数える。
    - **上限**: `dagq.toml`の`[kpi]`の`max_improvement_proposals`（既定2）。repositoryの方針として置き、ホストの設定では変えない。
    - **超えた分の扱い**: supervisorは、動いている改善のproposalの数が上限に達している間、ADR-0047の決定19のruntimeのplannerを新しく立てない。proposalを求める印の付いた`open`のfindingは`open`のまま待ち、proposalが1つ終わって数が上限を下回ったら、決定19の順（印の古い順）で次のfindingのplannerを立てる。待っているfindingは`dagq findings`で見え、待っている理由（上限）を出す。plannerがすでに立った後に上限を超える（同時に立った）ことはない: plannerを立てる判定と記録を同じ`BEGIN IMMEDIATE`の中で行い、その中で数える（ADR-0047の決定19と同じ）。
    - 人が`planner`で直接作ったproposalと、followupのdraftの採否（ADR-0047の決定16）は数えず、止めない。

26. **改善のproposalのtaskの優先度は`normal`以下にする。**
    findingから作ったproposalのtaskは、plannerが`low`か`normal`を付ける（既定`normal`）。plan reviewは、findingに紐づいたproposalに`high`以上のtaskがあれば、ADR-0047の決定11の`lower_priority`（jobが自分でしてよい修正）で`normal`に下げてからpassにする（reviseにはしない。優先度の引き下げだけで計画の意図は変わらないため）。人は`set-priority`で上げてよい（ADR-0049の決定4の「優先度は人が意図して付ける」）。改善のproposalのgoalは`rank`（goal 13）で他のgoalより前に置かない。

### X. ADR-0047との関係

27. **このADRはADR-0047を置き換えず、決定を変えない範囲で足す。**
    - 足すもの（ADR-0047の決定の中身を書き換えず、項を足す）:
      - 決定18のfindingの種類に`kpi`（決定18は種類を「観測の都合で足してよい」としている）。
      - 決定21の入力に`dagq kpi`、起動を省く判定で数えないeventにKPIの記帳のevent（このADRの決定24）。
      - 決定22の`dagq report`（決定22が「別のgoalで決める」とした項を、このADRが引き受ける）。
      - 決定19のruntimeのplannerを立てる条件に改善のproposalの上限（このADRの決定25）。決定19の「印の古い順」「1件あたり3回」「`BEGIN IMMEDIATE`で再検査」は変えない。
      - 決定10・11のplan reviewの検査に、findingに紐づいたproposalの優先度を`lower_priority`で`normal`に下げること（このADRの決定26）。決定11の`actions`の種類は増やさない。
      - 決定17のinboxに届くものに、pushの失敗のattention（このADRの決定23、分類は決定41の`recovery_failed`）。
    - ADR-0047の決定4がobserverに許すコマンドに、読み取りの`dagq kpi`と`dagq marks`を加える。`dagq mark`・`dagq report`（ファイルを書く）はobserverに許さない。
    - ADR-0047がADR-0064に丸ごと置き換わった後は、この決定の「ADR-0047の決定N」をADR-0064の対応表で読み替える。ADR-0064がこれらの決定の番号や中身を変えるなら、このADRも合わせて置き換える。

### 実装の範囲

実装は未着手。goal 40の後続taskが持つ: 変更の印（決定10〜13。task 429）、`dagq kpi`と前後比較（決定1〜9、14〜19。task 430）、日次レポートと`dagq report`（決定20・21）、push（決定22・23。task 432）、observerと改善の上限（決定24〜26）、plugin の skill と AGENTS.md の運用の記述。task 197へのtoolchainの属性の追加（決定10）は、197が着地した後の小さなtaskにしてよい。

## Alternatives

- **KPIの表を持ち、集計の結果を保存する。** 読むのは速くなるが、規則を変えたときに過去の値が古い規則のまま残る。run_eventsから再導出すれば、規則を変えても過去の期間を同じ規則で並べ直せる。レポートのJSONは写しとして残すが正にしない。
- **`kind`がnullのtaskを、宣言した`--paths`や着地の差分のパスから推す。** 古いtaskも種類に数えられるが、task 196の規則とは別の分類の規則を持つことになり、同じtaskが根拠によって別の種類になる。`unknown`に数えれば、古いtaskの割合が見えるだけで規則は1つのまま。
- **supervisorの周回ごとにKPIの標本（slotの使用、load）をeventに記録する。** 時間の分解能は上がるが、task 197のclaim時の記録と同じ値を別の時刻で二重に持つ。`candidates`だけはrunの無い時点の値なので、変わったときだけ記録する。hostの連続の記録（load・メモリの時系列）は、必要になれば別のgoalでqueueのdirのファイルとして持つ（eventにしない）。
- **toolchainと`[run.env]`の両方を記録する印にする。** toolchainはdagqを通らずに変わるので、runtimeが変化の時刻を知るにはどのみち読みにいく必要があり、claimのときに読むのがrunに効いた値として正確。`[run.env]`はrunのclaimの後に`integrate`で効くので、claim時の属性では表せず、印にする。
- **前後比較で、比べたい印以外の印を含まないように区間を自動で縮める。** 交絡は減るが、区間の長さが比較ごとに変わり、`n`が小さくなって判定できないことが増える。印を並べて示し、縮めるかは人が`--compare`で決める。
- **目標割れを1期間の外れで即時に通知する。** 早く気づけるが、日ごとの揺れで通知が連発する。3期間の連続と1日の上限で抑え、1期間の外れはレポートの`missed`で見せる。
- **pushをruntimeがntfyやSlackのAPIに直接送る。** 設定は少なくなるが、特定のサービスに依存し、secretの置き場所と形式をruntimeが持つことになる。人の決定のとおり、ホストのコマンドにstdinで渡す。
- **レポートを外部のサービス（静的サイト、dashboard）に送る。** 画面の前に居なくても見られるが、人の決定でローカルのHTMLに限った。外に出るのはpushのコマンドが送るまとめだけ。
- **改善のproposalの上限をfindingの種類ごとに持つ。** 種類ごとの偏りは防げるが、同時に動く改善の総量（slotと人の注意）を抑えるという目的には、1つの上限が分かりやすい。

## Consequences

- KPIは決まった規則で出るので、同じ期間を誰が何度出しても同じ値になる。規則を変えるときはこのADRを置き換え、過去の期間も新しい規則で並べ直せる。
- 種類・属性の層別と`confounders`の一覧により、parallelの変更とsccacheの導入のように近い時期の変更を、人が時刻を控えずに見分けられる。ただし、交絡を自動で取り除くことはしない。まとめられた「重なった変更」は分けて評価できないので、分けたい変更は間を空けて入れる運用が要る。
- `kind`がnullの古いtaskは`unknown`に入るので、task 196より前の期間の種類別の値は薄くなる。前後比較では`all`と`unknown`の割合を合わせて読む。
- supervisorの起動・停止、`[run.env]`のhash、`candidates`の変化、目標割れの始まり・解消、レポートとpushの記録が、run_eventsに新しく載る。どれも変化のときだけ書くので、件数は日に数十件の規模に収まる。
- ホストの設定のファイルが新しくでき、`dagq.toml`とは別に読む場所が増える。secretはrepositoryに入らないが、hostごとの設定をhostの移行のときに人が持ち運ぶ必要がある。
- pushのコマンドが壊れていてもqueueは止まらず、inboxに1件のattentionが出るだけになる。
- 改善のproposalは同時に2つまでしか動かないので、KPIの悪化が多いときは改善が順番待ちになる。待っているfindingは`dagq findings`で見え、人は上限を変えるか、plannerで直接proposalを作れる。
- observerはKPIの数字を作らず`kpi`の出力を引くので、LLMが数字を作らないという制約を守る。
