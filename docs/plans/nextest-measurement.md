---
id: plan-nextest-measurement
type: plan
title: cargo llvm-cov nextestへの切り替え前後のintegrateのverifyの所要時間と遅いtest
status: completed
created: 2026-09-26
updated: 2026-09-26
owners:
  - hisamekms
tags:
  - performance
  - testing
  - measurement
related:
  - adr-0076
  - adr-0049
  - adr-0078
---

# cargo llvm-cov nextestへの切り替え前後のintegrateのverifyの所要時間と遅いtest

[ADR-0076](../adr/0076-run-the-coverage-gate-tests-with-nextest.md)決定6の前後の測定（task 537）。goal 36（並列数を上げて得をできるようにする）の材料で、遅いtestを削るtaskはplannerがこの文書とgoal 36のnoteを見てtestごとに登録する。この文書はtaskを登録しない。

## 要点

- **nextestで流れたrunは10件に満たない。** 884b3d5の後に着地したruntimeのrunは14件あるが、`cargo llvm-cov nextest`で流れたのはtask 550と551の2件だけ（境界のtask 518を入れて3件）。残りの12件は切り替え前に登録されたtaskで、旧コマンド`cargo llvm-cov`のまま流れた（ADR-0076決定4）。そのため着地時刻での前後の比較は旧コマンド同士の比較になる。旧コマンドと新コマンドの比較はn=2〜3の暫定の値で、10件そろった後に測り直す（task 537のaskの回答A）。
- **期間での前後**: integrateのverify（`land_phases.verify`）の中央値は前302秒・後300秒で変わらない。llvm-covの段は前272秒・後286.5秒。後の期間のrunの大半が旧コマンドなので、差は切り替えの効果ではない。
- **コマンドでの比較（暫定）**: nextestのtest段（`Summary`）は`NEXTEST_TEST_THREADS=4`で200秒・170秒。同じ期間の旧コマンドのtest段（binaryごとの`finished in`の合計）の中央値は231秒。ただしnextestはtest段の後の時間（testの一覧・profrawのmerge・report）が23秒・109秒で、旧コマンドの約20秒より長い。llvm-covの段の全体は266秒・301秒で、旧コマンドの中央値286.5秒とほぼ同じ。ADR-0076が見込んだ「1着地あたり100〜150秒減」はまだ出ていない。
- **律速は並列度で割った合計**。test段の時間は、testごとの所要時間の合計を`NEXTEST_TEST_THREADS`で割った値とほぼ一致する（550: 198.9秒対200.2秒、551: 169.1秒対169.8秒）。最長のtestは21〜24秒で、それより十分長い。testのprocessはほぼ待っているだけで、4本走っていてもCPUは合計0.1〜0.4コアだった。並列度を上げればtest段は縮む見込みで、既定の8で流れた518のtest段は112秒だった。
- **遅いtestの上位10件はtest時間の合計の15%程度**。合計の76%は2秒以上のtest（約140件）が占め、その多くは2〜5秒のruntimeの統合test。上位を1件ずつ削るより、多くのruntimeのtestに共通する待ち（fixture、supervisorの起動、pollの間隔、timeoutを待つ設定）を削るほうが効く。SLOW（60秒超）のtestは無い。

## 1. 期間の境界

| 項目 | 値 |
| --- | --- |
| 境界のcommit | `884b3d5`（task 518、run `472512db`）。main上のcommit時刻は2026-09-26 15:50:04 JST、`run_integrated`はevent 11911（06:50:05Z） |
| 前の期間 | llvm-covの段の開始が2026-09-26 02:49:24Z（11:49 JST）から06:50:05Zまで。開始はtask 427（`[run.env]`の`CARGO_BUILD_JOBS`・`RUST_TEST_THREADS`）の着地（event 10408）で、`dagq stats --since 10408 --until 11911 --full`の範囲にあたる |
| 後の期間 | 884b3d5より後にllvm-covの段が終わったrun。最後はtask 439のrun `518d0612`（event 13249、10:13:57Z）で、`dagq stats --since 11911 --until 13249 --full`の範囲にあたる。測定時の`next_cursor`は13261 |
| `--parallel` | 両期間とも3。run eventsの`parallel`は2026-09-25 19:47Z（event 8625）までが4、23:34Z（event 9000）以降は3で、前の期間の開始より前に3になっている。後の期間は`supervisor_started`の`parallel`と`claim_parallel`も3 |
| 対象 | 期間内に着地したrunのうち、verificationにllvm-covを含むもの（最後のintegrateの試行）。docsだけのrunは含めない |

前の期間の開始をtask 427の着地に置いたのは、比較の前提を崩す変更を前の期間から外すためである。

- hostのRust toolchainは2026-09-26の11:19〜11:48 JSTの間にRosettaのx86からmiseのarm64に変わった。
- sccache（task 393、ADR-0049決定6）は02:46Zに有効になった。
- `CARGO_BUILD_JOBS=4`・`RUST_TEST_THREADS=4`（task 427）は02:49Zに入った。

`--parallel`が3になったのはそれより前なので、前の期間はどの点でも後の期間とそろっている。

### 比較の前提を崩す重なり

- **後の期間のrunの大半は旧コマンド**: 14件のうち12件は`cargo llvm-cov --locked --fail-under-lines 80`で流れた（ADR-0076決定4により、登録済みtaskのverificationは書き換えない）。nextestで流れたのは550（09:25Z）と551（09:58Z）だけ。
- **境界の518は並列度8**: 518は`dagq.toml`に`NEXTEST_TEST_THREADS = "4"`を足したtask自身で、`integrate`はmain checkoutの`dagq.toml`を読むので、518のverifyはnextestの既定（8並列）で流れた。testごとの合計882.7秒を8で割ると110.3秒で、Summaryの111.6秒と一致する。
- **task 528（06:30Z着地）**: workerが手元で全部の`cargo test`を流さなくなった。後の期間のhostのloadが低いのはこれも一因で、前後のloadの差をnextestの効果とは読めない。
- **task 550（09:30Z着地、8ee936b）**: dev profileのdebug情報を減らした。551以降のbuildとprofrawの大きさに効く。
- **task 551（10:03Z着地、f2d128b）**: ADR-0078でintegration testを1つのbinary `it`にまとめた。551自身のverifyは5 binary（550は47 binary）。551以降の旧コマンドのrun（439）も1 binaryで流れる。
- **検証コマンドごとの内訳**: task 509の`verification_command` eventを使った。llvm-covの段の時間は、同じ試行の直前のコマンド（clippy）のeventとllvm-covのeventの時刻の差で出した。`land_phases.verify`は各runの全試行を含むので、2試行のrunでは段の時間より長い。
- **sccacheの数字**: task 460（`docs/plans/sccache-measurement.md`）は未着地なので参照していない。

## 2. 前後の着地run数・所要時間・load

loadは`~/.local/share/dagq-hostmetrics/metrics.csv`（約30秒ごとの`load1`）の、各runのllvm-covの段の間の平均と最大である。後の期間ではstatsの`load.verify.mean`も取れ、csvの値とほぼ一致した。

| 区分 | run数 | llvm-covの段 中央値（範囲） | `land_phases.verify` 中央値（範囲） | build | test段 | test段の後 | load1 平均の中央値（範囲）／最大 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 前（旧コマンド） | 15 | 272秒（214〜474） | 302秒（219〜726） | 39秒（24〜125） | 220秒（177〜318） | 18秒（5〜31） | 10.1（5.1〜23.6）／29.5 |
| 後（全体） | 14 | 286.5秒（236〜355） | 300秒（242〜372） | — | — | — | 6.1（2.8〜10.4）／14.7 |
| 後のうち旧コマンド | 12 | 286.5秒（236〜355） | 300秒（242〜372） | 36.5秒（25〜68） | 231秒（207〜278） | 20.5秒（3〜35） | 6.1（2.9〜10.4）／14.7 |
| 後のうちnextest（550・551） | 2 | 266秒・301秒 | 284秒・313秒 | 43秒・23秒 | 200秒・170秒 | 23秒・109秒 | 6.5・2.8／8.9 |
| 境界の518（nextest、8並列） | 1 | 206秒 | 213秒 | 31秒 | 112秒 | 63秒 | 7.2／12.5 |

各列の意味は次のとおり。

- **build**: logの最後の``Finished `test` profile ... in``の値。
- **test段**: 旧コマンドではbinaryごとの`finished in`の合計、nextestでは`Summary [ … s]`の値。
- **test段の後**: 段の時間からbuildとtest段を引いた残り。testの一覧（nextestはbinaryごとに`--list`を実行する）、profrawのmerge、reportの時間が入る。logに時刻が無いので、これ以上は分けられない。

runごとの値は次のとおり（時刻はllvm-covの段の開始、JST）。

| 期間 | task | run | 開始 | コマンド | 段 | build | test段 | 後 | `land_phases.verify` | load1 平均／最大 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 前 | 345 | 54a52f8b | 11:58 | 旧 | 231 | 28 | 196 | 8 | 232 | 12.3／16.3 |
| 前 | 403 | abda1589 | 12:19 | 旧 | 214 | 32 | 177 | 5 | 219 | 7.9／10.3 |
| 前 | 335 | a7554f40 | 12:33 | 旧 | 320 | 39 | 261 | 20 | 329 | 13.0／18.0 |
| 前 | 394 | 96a9f439 | 12:46 | 旧 | 264 | 24 | 224 | 15 | 271 | 10.0／15.8 |
| 前 | 310 | 5575d5aa | 13:16 | 旧 | 259 | 35 | 208 | 15 | 428 | 9.8／14.6 |
| 前 | 358 | ecc79d4f | 13:21 | 旧 | 276 | 40 | 220 | 15 | 302 | 10.1／13.5 |
| 前 | 466 | 9498021b | 13:49 | 旧 | 474 | 125 | 318 | 31 | 501 | 23.6／29.5 |
| 前 | 510 | 40aa57cd | 13:58 | 旧 | 342 | 71 | 249 | 22 | 726 | 17.2／22.0 |
| 前 | 490 | de8c2c19 | 14:15 | 旧 | 248 | 26 | 206 | 16 | 255 | 6.1／6.9 |
| 前 | 461 | ebe5c2dd | 14:26 | 旧 | 306 | 64 | 223 | 19 | 462 | 10.8／18.5 |
| 前 | 357 | ac231167 | 14:31 | 旧 | 336 | 67 | 251 | 18 | 352 | 12.5／16.8 |
| 前 | 491 | efcff4e9 | 14:44 | 旧 | 265 | 38 | 211 | 15 | 276 | 6.4／10.4 |
| 前 | 382 | 799c6d99 | 14:49 | 旧 | 265 | 27 | 217 | 20 | 277 | 5.4／8.8 |
| 前 | 492 | b013fe1b | 15:02 | 旧 | 272 | 39 | 215 | 18 | 280 | 5.1／7.5 |
| 前 | 462 | c321a245 | 15:12 | 旧 | 353 | 66 | 258 | 29 | 366 | 10.3／16.6 |
| 境界 | 518 | 472512db | 15:46 | nextest（8並列） | 206 | 31 | 112 | 63 | 213 | 7.2／12.5 |
| 後 | 196 | 902f96b2 | 15:55 | 旧 | 286 | 46 | 219 | 21 | 296 | 6.0／8.8 |
| 後 | 197 | 498446c4 | 16:21 | 旧 | 285 | 32 | 231 | 22 | 292 | 6.7／9.8 |
| 後 | 385 | b6d92d34 | 16:33 | 旧 | 297 | 35 | 242 | 20 | 304 | 6.4／7.8 |
| 後 | 386 | 8d29f958 | 17:06 | 旧 | 286 | 35 | 229 | 22 | 294 | 4.8／5.8 |
| 後 | 429 | 1b3e865b | 17:11 | 旧 | 271 | 31 | 223 | 17 | 283 | 2.9／4.4 |
| 後 | 241 | dbafdbda | 17:36 | 旧 | 355 | 42 | 278 | 35 | 364 | 10.4／14.2 |
| 後 | 514 | 489ef2d7 | 17:44 | 旧 | 287 | 45 | 224 | 18 | 296 | 4.2／7.0 |
| 後 | 445 | 38532947 | 17:57 | 旧 | 300 | 38 | 242 | 20 | 308 | 5.5／9.5 |
| 後 | 325 | 5ec4622a | 18:09 | 旧 | 283 | 31 | 231 | 21 | 372 | 3.7／5.5 |
| 後 | 495 | 5ebae268 | 18:20 | 旧 | 319 | 54 | 243 | 22 | 336 | 6.1／12.2 |
| 後 | 550 | b2f7b36f | 18:25 | nextest | 266 | 43 | 200 | 23 | 284 | 6.5／8.9 |
| 後 | 362 | 1cf47dd2 | 18:47 | 旧 | 328 | 68 | 240 | 20 | 366 | 8.8／14.7 |
| 後 | 551 | 92ffcda6 | 18:58 | nextest | 301 | 23 | 170 | 109 | 313 | 2.8／3.5 |
| 後 | 439 | 518d0612 | 19:10 | 旧 | 236 | 25 | 207 | 3 | 242 | 6.4／9.9 |

読み方:

- 旧コマンドのtest段は、前の期間（loadの中央値10.1）でも後の期間（6.1）でも中央値220〜231秒でほぼ同じ。loadが半分になってもtest段は縮んでいないので、test段はCPUの取り合いではなく、testの中の待ちで決まっている。
- nextestの2件はtest段が31〜61秒短い（200秒・170秒対231秒）。ただし551は「test段の後」が109秒で、段の全体は301秒になり、旧コマンドより長い。551はintegration testを1つのbinary（`it`、387件）にまとめた最初のrun（全体は5 binaryで739件）で、`it`のtestのprocessごとのprofrawが大きなbinary 1本分の計数器を持つので、mergeが重くなった可能性がある（未確認。この時間は分けて測れていない。follow-up）。550（47 binary）の後の時間は23秒で旧コマンド並み、518（46 binary）は63秒。
- `land_phases.verify`はfmtとclippyを含み、2試行のrunではその分も加わる（例: 510の726秒）。

## 3. 遅いtestの上位10件（後の期間のnextestの出力）

`integrate-1-verify-3.log`（llvm-covの段）のnextestの`PASS [ … s]`行から取った。SLOW（`.config/nextest.toml`の`slow-timeout`の60秒超）の行は、550・551・518のどのlogにも無い。最長は21〜24秒。

| 順 | test（551の名前） | 551（5 binary、`it`は1本） | 550（47 binary） |
| --- | --- | --- | --- |
| 1 | `cli_version::auto_update_installs_each_runtime_landing_and_puts_a_broken_build_back` | 20.8秒 | 24.0秒 |
| 2 | `runtime_resume::a_request_lost_twice_is_asked_to_the_inbox` | 16.5秒 | 17.2秒 |
| 3 | `runtime_resume::conflict_only_resumes_are_not_counted_and_a_used_up_run_is_retried_with_its_branch` | 11.3秒 | 12.1秒 |
| 4 | `cli_version::install_hands_a_running_supervisor_over_under_its_pid_and_rolls_back` | 10.2秒 | 11.3秒 |
| 5 | `runtime_review::a_failed_review_closes_the_session_and_asks_a_person_in_the_same_step` | 9.3秒 | 12.1秒 |
| 6 | `runtime_resume::a_resumed_session_gets_its_request_only_once_its_input_box_is_ready` | 8.2秒 | 8.9秒 |
| 7 | `runtime_stall::a_session_idle_after_its_nudge_gets_one_stalled_ask_and_its_answers_are_applied` | 7.7秒 | 7.9秒 |
| 8 | `runtime_resume::a_resumed_session_that_ignores_exit_is_let_go` | 6.0秒 | 6.8秒 |
| 9 | `runtime_adopt::independent_tasks_run_concurrently_and_a_dependent_starts_after_integration` | 5.8秒 | 6.1秒 |
| 10 | `runtime_resume::a_lost_request_is_sent_again_after_no_sign_of_work` | 5.5秒 | 6.0秒 |

testの時間の分布は次のとおり（551、739件、合計676.5秒）。

| 所要時間 | 件数 | 合計 | 合計に占める割合 |
| --- | --- | --- | --- |
| 10秒以上 | 4 | 59秒 | 9% |
| 5秒以上 | 17 | 138秒 | 20% |
| 2秒以上 | 136 | 516秒 | 76% |
| 1秒以上 | 195 | 604秒 | 89% |

上位10件の合計は101.5秒で、全体の15%。test moduleごとの合計の上位は`runtime_resume` 136秒、`runtime_review` 81秒、`runtime_integrate` 68秒、`runtime_session` 38秒、`runtime_adopt` 35秒、`cli_version` 34秒、`runtime_triage` 29秒、`runtime_claim` 28秒、`runtime_stall` 28秒。

## 4. 律速の見立てとNEXTEST_TEST_THREADSの余地

| run | 並列度 | testごとの合計 | 合計／並列度 | Summary | 最長のtest |
| --- | --- | --- | --- | --- | --- |
| 518 | 8（既定） | 882.7秒 | 110.3秒 | 111.6秒 | 24.6秒 |
| 550 | 4 | 795.8秒 | 198.9秒 | 200.2秒 | 24.0秒 |
| 551 | 4 | 676.5秒 | 169.1秒 | 169.8秒 | 20.8秒 |

- **律速は「合計÷並列度」**で、最長のtestではない。3本ともSummaryが合計÷並列度と1%以内で一致し、最長のtestの4.5〜8倍ある。nextestは空いたslotに次のtestをすぐ入れるので、今のtestの構成では並列度に比例してtest段が縮む。最長のtest（約21〜24秒）が律速になるのは、並列度がおよそ30を超えてからである。
- **CPUはほぼ空いている**: 551のtest段（他のrunがほぼ動いていない時間帯、`runs`=1）では、testのprocess 4本のCPUは合計11〜39%（0.1〜0.4コア）、`cpu_idle`は61〜75%、load1は2.5〜3.5だった。testのほとんどはstubのsessionやsupervisorをpollで待っている。518（8並列、他のrunのbuildと重なっていた）ではload1が7〜12.5。ただし`test_cpu`はtestのprocessだけを数えており、testが起こす`dagq`・`git`・shのstubの子プロセスは含まない可能性がある。
- **余地**: `NEXTEST_TEST_THREADS`を8にすれば、551の構成でtest段は約170秒から約85秒になる見込み（518の実測は112秒で、そのときの合計は今より大きかった）。1着地あたり約85秒減る。上げたときの懸念は次の2つ。
  1. worker 3本のbuildとintegrateのtestが重なる時間帯にloadが上がり、cmuxのcaptureの時間切れ（goal 36の発端）が増えるおそれがある。
  2. 時間の上限を持つtest（`within`、各testのtimeout）が、待ちの重なりで不安定になるおそれがある。

  上げるなら、まず6か8に上げるtaskにして、`backend_call_failed`の件数と、integrateの検証の失敗（`verification_failed`のresume）を前後で比べるのがよい。この文書では値を変えない。
- **並列度と並べて、test段の後を見る**: nextestにしてもllvm-covの段の全体が縮まない主な理由は、「test段の後」が長くなったこと（518で63秒、551で109秒）である。並列度を上げて縮めた分を打ち消しうるので、先にこの時間の中身（testの一覧・profrawのmerge・report）を測る価値がある。

## 5. 遅いtestごとの短縮の候補

律速が合計÷並列度なので、効くのは「多くのtestに共通する待ち」を削ることで、上位の1件だけを削ってもtest段は（その秒数÷並列度）しか縮まない。候補は次のとおり。どれもコードを読んだ範囲の見立てで、実測していない。

共通の手（2秒以上のruntimeのtest（551で136件）の多くに効く）:

- **supervisorのtickとpollの間隔**: `tests/it/runtime_support`の`TEST_TICK`（50ms）と`idle_poll`、stubのshが`sleep 0.05`で待つloop、helperの`thread::sleep(20ms)`。1つのtestでsupervisorが工程を何周も回すので、tickを短くするか、event（fileの変化）で起こすようにすると、全testが一律に縮む。ただし短くするとCPUとloadは上がる。
- **fixtureの作り方**: testごとの`git init`・seedのcommit・queueのDBの作成・stubの配置。共通のtemplateのrepository（とmigrate済みのDB）を1回作ってcopyするようにすれば、processを分けたnextestでも効く。
- **supervisorの起動と終了の待ち**: testごとに`dagq supervise`を子プロセスで起こし、終わりを待っている。起動の完了を待つpollの間隔と、終了（drain）の待ちを見直す。

testごとの手:

1. `cli_version::auto_update_installs_each_runtime_landing_and_puts_a_broken_build_back`（21〜24秒）: 「docsの変更ではjobが起きない」ことを`std::thread::sleep(3s)`で待ってから否定している（`tests/it/cli_version.rs`の`auto_update`のtest）。否定の待ちを、supervisorが後続のcommitを処理したという観測（event）に置き換える。stubのbuildとsupervisorの入れ替え（execし直し）を3回待つので、handoffのpollの間隔（`--handoff-timeout`と200msのsleep）も候補。
2. `runtime_resume::a_request_lost_twice_is_asked_to_the_inbox`（16〜17秒）: `resume_timeout = 4s`・`start_wait = 1s`・`exit_timeout = 1s`の時間切れを2周待つ設計。時間切れを待つこと自体がtestの中身なので、timeoutの値を下限まで下げる（例: 4秒→1秒）のが一番効く。
3. `runtime_resume::conflict_only_resumes_are_not_counted_and_a_used_up_run_is_retried_with_its_branch`（11〜12秒）: resumeを上限まで繰り返す。同じくresumeの各timeoutとtickを下げる。
4. `cli_version::install_hands_a_running_supervisor_over_under_its_pid_and_rolls_back`（10〜11秒）: 実際に`install`でsupervisorを引き継がせる。handoffの待ちのpoll（100msのsleep）と`--handoff-timeout`を見直す。
5. `runtime_review::a_failed_review_closes_the_session_and_asks_a_person_in_the_same_step`（9〜12秒）: reviewのjobの失敗を待つ。reviewのstubの失敗までの時間と、sessionを閉じるまでの`exit_timeout`を下げる。
6. `runtime_resume::a_resumed_session_gets_its_request_only_once_its_input_box_is_ready`（8〜9秒）、8. `runtime_resume::a_resumed_session_that_ignores_exit_is_let_go`（6〜7秒）、10. `runtime_resume::a_lost_request_is_sent_again_after_no_sign_of_work`（5.5〜6秒）: どれもresumeのsessionの時間切れ（`resume_timeout`・`exit_timeout`・「作業の兆しが無い」の閾値）を待つ。`runtime_resume`は合計136秒で最大のmoduleなので、moduleの共通のbackendの設定のtimeoutを下げる1本のtaskにまとめられる可能性がある。
7. `runtime_stall::a_session_idle_after_its_nudge_gets_one_stalled_ask_and_its_answers_are_applied`（約8秒）: nudgeからstalledと判定するまでのidleの閾値を待つ。test用の閾値を下げる。
9. `runtime_adopt::independent_tasks_run_concurrently_and_a_dependent_starts_after_integration`（約6秒）: 複数のrunを実際に着地（integrate）させるので、stubの検証コマンドとgitの処理の時間が積み上がる。依存のtaskの開始の観測に必要な最小の構成に絞る。

## 測り直し

nextestで流れたrunが10件そろった時点（ADR-0076決定4により、新しく登録されたruntimeのtaskから順に増える）で、同じ方法（`verification_command` eventの時刻の差、logのbuild・Summary・その後、`metrics.csv`のload）で測り直す。測り直すときに見るのは次の点。

- 551以降の1 binary構成（ADR-0078）での「test段の後」の時間
- 上の短縮の候補の効果
