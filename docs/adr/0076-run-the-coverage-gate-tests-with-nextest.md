---
id: adr-0076
type: adr
title: integrateのcoverageの関門のtestをcargo-nextestでbinaryをまたいで並列に流し（cargo llvm-cov nextest）、cargo-nextestは人がhostに入れる
status: accepted
created: 2026-09-26
updated: 2026-09-26
accepted_on: 2026-09-26
owners:
  - hisamekms
tags:
  - operations
  - performance
  - testing
related:
  - adr-0029
  - adr-0047
  - adr-0049
  - design-supervisor-lifecycle
---

# ADR-0076: integrateのcoverageの関門のtestをcargo-nextestでbinaryをまたいで並列に流し（cargo llvm-cov nextest）、cargo-nextestは人がhostに入れる

## Context

[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定1で、`verification_commands`は`integrate`がrebase後に1回だけ走らせる。runtime（`src/`）を触るtaskの標準のverificationは`cargo fmt --all --check`・`cargo clippy --locked --all-targets -- -D warnings`・`cargo llvm-cov --locked --fail-under-lines 80`で（AGENTS.mdのテストの制約）、着地待ちのうち`integrate`の検証の工程は中央値271秒かかる。

その大半は`cargo llvm-cov`のtestの実行である。`cargo test`（と、それを包む`cargo llvm-cov`）はtest binary（`src/lib.rs`のunit testと、20本を超える`tests/*.rs`）を1本ずつ直列に流し、並列になるのは1つのbinaryの中のtestだけ（`[run.env]`の`RUST_TEST_THREADS = "4"`まで）。2026-09-26の`integrate-1-verify-3.log`（`cargo llvm-cov`の段）4本では次のとおりだった。

- instrumentedのbuildは24〜40秒。
- test binaryごとの`finished in`の合計は220〜261秒。大きいのは`runtime_resume`約40秒、`cli`約24秒、`runtime_integrate`約18秒、`lifecycle`約15秒など。多くはsupervisorやstubのsessionをpollで待つ時間で、CPUを使っていない。
- sccache（ADR-0049の決定6）は依存crateのcompileにだけ効き、testの実行には効かない。

binaryの中の並列はそのbinaryで一番遅いtestで頭打ちになり、binaryをまたいでは並列にならないので、待ちの多いtestが直列に積み上がっている。goal 36（並列数を上げて得をできるようにする）の着地待ちの内訳（ADR-0049の決定5）を縮めるには、この部分を並列にするのが一番大きい。

[cargo-nextest](https://nexte.st/)はtestを1件ずつ別のprocessで流し、binaryをまたいで並列に実行する。cargo-llvm-covは`cargo llvm-cov nextest`でnextestを包み、processごとのprofrawを集めて同じ関門（`--fail-under-lines`）をかけられる。見込みは、verifyのtestの部分が220〜260秒から90〜130秒（1着地あたり100〜150秒減）。数字は実装のtaskで実測する。

制約:

- goal 36のconstraintsとADR-0049の決定7・8: hostに入れるツールは人が入れ、workerは`brew` / `cargo install`でhostを変えない。ツールが無いhostでの失敗の仕方をADRで決める。
- runごとの`CARGO_TARGET_DIR`を共有しない（AGENTS.mdの理由(a)(b)）。本ADRはtargetの扱いを変えない。
- 本ADRはADR-0049を置き換えず、決定を足す。ADR-0049の決定（検証を`integrate`の1回にする、`[run.env]`、sccache、決定9の検査）はそのまま有効。

## Decision

**原則。** 変えるのはcoverageの関門のtestの流し方だけで、関門の中身（どのtestを流し、行カバレッジ80%を下回れば落とす）は変えない。workerが手元で回す検証と、既に登録されたtaskの検証は変えない。hostのツールはsccacheと同じく人が入れ、入る前には切り替えない。

1. **(a) coverageの関門の標準のverificationを`cargo llvm-cov nextest --locked --fail-under-lines 80`にする。**
   - runtime（`src/`・`tests/`・`migrations/`）を触るtaskを登録するとき、plannerは`cargo llvm-cov --locked --fail-under-lines 80`の代わりにこれをverificationに入れる（AGENTS.mdのテストの制約とdagq skillの推奨の組み合わせを後続taskで改める。決定7）。fmtとclippyと`--evidence e2e`はそのまま。
   - `cargo llvm-cov nextest`は`cargo llvm-cov`と同じtest binary群（`src/lib.rs`のunit testと`tests/*.rs`）を流し、1件でも落ちれば失敗する。nextestはdoctestを流さないが、このcrateにdoctestは無い。doctestを足すtaskは、そのときdoctestを関門に含める方法を決める。
   - `#[ignore]`の`tests/e2e.rs`は今までどおり流れない（nextestも既定でignoredのtestを飛ばす）。e2eはworkerがworktreeで`cargo test --locked --test e2e -- --ignored`で流す（ADR-0049、AGENTS.md）。
   - **workerが手元で回す`cargo test --locked`（AGENTS.mdの「変更後に必ず通す」の3本）は変えない。** workerの環境にnextestを求めず、workerの検証はツールの有無に左右されない。`integrate`の検証が落ちてresumeされたrunでworkerが落ちたコマンドを再現するときは、`cargo llvm-cov nextest`をそのまま流してよい（hostに入っている前提。決定3）。
   - testは1件ずつ別processになるので、1つのbinaryの中のtestが同じprocessの状態を共有していることに頼るtestは壊れうる。今のtestの共有状態（`tests/common/mod.rs`の待ちの監視、`tests/runtime_support/mod.rs`のstubの表）はprocessの中で閉じており、process 1つにtest 1件でも成り立つ見込みだが、実装のtaskが全testを流して確かめる。`common::within`の時間切れの報告（test名は実行中のthreadの名前から取る）がnextestの下でもtest名を出すことも確かめる。
2. **(b) nextestの並列度は`dagq.toml`の`[run.env]`に`NEXTEST_TEST_THREADS = "4"`で置き、`.config/nextest.toml`には並列度を書かない。**
   - nextestは`RUST_TEST_THREADS`を読まず、既定ではCPU数（このhostで8）だけtestのprocessを同時に走らせる。nextestの並列度はprocessの数で、1つのtestはほぼ1 threadで走る。
   - 値の決め方は`RUST_TEST_THREADS = "4"`と同じ（AGENTS.md、task 427）: hostは8コア / 16GBで、supervisorの`--parallel`を3にし、worker 3本と`integrate` 1本が同時にcargoを回しても合計16並列（コア数の2倍）程度に収める。`integrate`の中で同時に動くtestを、今の`RUST_TEST_THREADS`と同じ4本までにすれば、hostにかかる負荷の上限は今と変わらず、binaryをまたいで待ちが重なる分だけ速くなる。
   - 多くのtestはpollで待っていてCPUを使わないので、4より大きくしても負荷は上がりにくい見込みがある。実装のtaskは4で入れて実測し（決定6）、loadと所要時間を見て上げる場合は`dagq.toml`を変えるtaskにする。`--parallel`やhostを変えたら`CARGO_BUILD_JOBS`・`RUST_TEST_THREADS`と合わせて見直す。
   - `[run.env]`に置く理由: host 1台でrunを並べるときの絞り込みは、`CARGO_BUILD_JOBS`・`RUST_TEST_THREADS`と同じ場所に同じ理由で並べたほうが、見直すときに1か所で済む。`NEXTEST_TEST_THREADS`は`.config/nextest.toml`の`test-threads`より優先されるので、`[run.env]`の値が`integrate`の検証に効く。`.config/nextest.toml`に4と書くと、CIや人が手元で流すnextestまで絞られる。CIと`dagq`を通さないcargoは`dagq.toml`を読まないので、nextestの既定（CPU数）で流れる。
   - `.config/nextest.toml`を置くのは、並列度以外にnextestの既定から変えたいものがあるときだけにする。今決めるのは決定6の`slow-timeout`だけで、retry（落ちたtestの再実行）は既定の0のままにし、不安定なtestをretryで隠さない。`fail-fast`は既定のままにする。
   - needs_sessionのresumeが開くworkspaceには`[run.env]`が渡らない（AGENTS.md）ので、そこでworkerが`cargo llvm-cov nextest`を再現すると8並列で流れる。resumeでの再現はまれで短いので、`RUST_TEST_THREADS`と同じく受け入れる。
3. **(c) cargo-nextestは人がmiseのglobalで入れ、`~/.local/bin`にmiseのshimへのlinkを置く。runtimeの検査は足さず、人が入れたことを確かめてから切り替える。**
   - 入れるのは人で、`mise use -g cargo:cargo-nextest`で入れ（miseの短い名前の登録は無く、cargo backendで入れる）、`ln -s ~/.local/share/mise/shims/cargo-nextest ~/.local/bin/cargo-nextest`を置く。理由はsccache（ADR-0049の決定7）と同じで、supervisor（と`integrate`の検証）のPATHは`up`の時点で固定されるので、nextestを更新して古いversionのdirectoryが消えても、`~/.local/bin`のlink（miseのshim）で今のversionを引ける。`cargo`は`cargo nextest`をPATHの`cargo-nextest`で解決し、`cargo llvm-cov nextest`もそれを呼ぶ。workerは入れない（goal 36のconstraints）。
   - **切り替えの順**: (1) 人がcargo-nextestとlinkを入れ、supervisorと同じPATHの解決先（`~/.local/bin/cargo-nextest`）で`cargo nextest --version`が通ることを確かめる。(2) その後に、`.config/nextest.toml`（要るなら）・`dagq.toml`の`NEXTEST_TEST_THREADS`・AGENTS.md・CIを改めるtaskを着地させる（決定7）。(3) それ以降に登録するtaskから新しいverificationを使う。入る前に新しいverificationのtaskを登録しない。
   - **見つからないとき**: `cargo llvm-cov nextest`はcargo-nextestが無いと`no such command: nextest`系のerrorで失敗し、`integrate`の検証の失敗として`needs_session`になる。runtimeはこれを事前に検知しない。ADR-0049の決定9の検査は`[run.env]`が名指すプログラムだけを見るもので、`verification_commands`の中のcargoのsubcommandまで推すのは別の仕組みになり、今は切り替えの順（上の(1)）と、決定4の旧コマンドの併存で足りる。見つからない状態で流れても、workerはhostにツールを入れられないので、resumeされたworkerは直そうとせず`failed`のreceiptか`dagq ask`で「cargo-nextestが無い」と返し（AGENTS.mdのworker節に後続taskで1文足す。決定7）、resumeの上限（ADR-0047）と合わせて繰り返しは有限で人に届く。
   - **戻し方**: hostからcargo-nextestを外す、または別のhostで動かすときは、plannerは旧コマンド（`cargo llvm-cov --locked --fail-under-lines 80`）でtaskを登録すればよい。`integrate`はtaskごとの`verification_commands`を流すので、切り替えも戻しもtask単位で、runtimeの変更は要らない。
   - `dagq doctor`にnextestの解決先を出す欄は今は足さない。`[run.env]`以外のツールを`doctor`や`up`のpreflightで検査する必要が出たら（見つからないことによる`needs_session`が実際に起きたら）、ADR-0049の決定9の「必要なツールを宣言する表」を足すADRを書く。
4. **(e) 既に登録されたtaskのverification（`cargo llvm-cov --locked --fail-under-lines 80`）は書き換えず、そのまま有効とする。**
   - どちらも同じtest binary群を流し、同じ80%の行カバレッジで落とす同じ関門で、違うのはtestの流し方（直列のbinaryか、process単位の並列か）と所要時間だけ。旧コマンドのtaskは遅いだけで、正しさは変わらない。
   - plan reviewは、旧コマンドを持つtaskを旧コマンドであることを理由にreviseしない。新しく登録するtaskには決定1のコマンドを使う（決定3の切り替えの後）。
   - AGENTS.mdの「llvm-covを含めるtaskでは`cargo test --locked`を重ねない」は、`cargo llvm-cov nextest`にも同じ理由で当てはまる（同じtestを2回流すだけ）。
5. **(d) CI（`.github/workflows/ci.yml`）も同じコマンドにする。**
   - `cargo llvm-cov`のstepを`cargo llvm-cov nextest --locked --fail-under-lines 80`にし、cargo-nextestは`taiki-e/install-action@cargo-nextest`で入れる（cargo-llvm-covと同じ入れ方）。CIは`dagq.toml`を読まないので、runnerのCPU数で並列に流れる。
   - 関門を`integrate`とCIで同じコマンドにすれば、processごとに分かれることで初めて出るtestの問題（決定1）をどちらでも同じように見つけられ、片方だけ通る状態を作らない。
   - CIの切り替えも決定3の(2)のtaskで行う。CIのrunnerにはinstall-actionで毎回入るので、hostの前提とは関係しない。
6. **(f) 遅いtestを削る次の手は、実装のtaskの実測を入力にして、goal 36の下で遅いtestごとの別taskとして扱う。**
   - `.config/nextest.toml`の`[profile.default]`に`slow-timeout = { period = "60s" }`を置き、60秒を超えたtestをnextestが`SLOW`と出すようにする（打ち切りはしない。testの上限は`tests/common/mod.rs`の`within`が持つ。AGENTS.mdのテストの制約）。nextestはtestごとの所要時間を出すので、`integrate-<attempt>-verify-N.log`から遅いtestの上位を読める。
   - 実装のtask（決定3の(2)）は、切り替えの前後で`integrate`の検証の`cargo llvm-cov`の段の所要時間を比べ（ADR-0049の決定10と同じく、同じ並列数の期間の10 run以上で、load averageを添える）、nextestの出力から所要時間の上位のtestを挙げてgoal 36のnoteに残す。
   - 遅いtestを削るtask（pollの間隔、stubの待ち、fixtureの作り方など）は、その上位の一覧を見てplannerがtestごとに登録する。着地待ちとtestの計測を扱うtask 514・515・509の数字と同じnoteに並べ、重なる計測を二度しない。nextestの下では全体の所要時間は一番遅いtestと、並列度で割った合計のどちらか大きいほうで決まるので、削る順は単独で長いtest（今は`runtime_resume`の中の長いtestなど）から見る。
   - JUnitなどの機械が読む出力を`stats`に取り込むかは、この一覧で足りないと分かったときに決める。今は足さない。
7. **後続taskは1本で、決定3の(1)（人のinstall）の後に着地させる。**
   - 変えるもの: `.config/nextest.toml`（決定6の`slow-timeout`）、`dagq.toml`の`[run.env]`の`NEXTEST_TEST_THREADS = "4"`（決定2、コメントに理由）、`.github/workflows/ci.yml`（決定5）、AGENTS.md（テストの制約・推奨の組み合わせ・workerの節の`cargo llvm-cov`の記述を決定1・4に合わせ、cargo-nextestの入れ方と見つからないときの返し方（決定3）を足す）、dagq skillなどplugin文書にある推奨のverificationの記述、docs/designの該当箇所。`src/`は変えない。
   - 検証: そのtask自身のverificationに`cargo llvm-cov nextest --locked --fail-under-lines 80`を入れ、`integrate`がnextestで関門を通せることをその着地で確かめる（`dagq.toml`はmain checkoutに反映されてから効くので、並列度4での測定は次の着地から）。決定1の共有状態と`within`の報告の確認、決定6の前後の測定もこのtaskで行う。
   - 決定3の(1)が済んでいないときは、plannerはこのtaskを`ready`にしない（入る前にこのtaskのverificationを`integrate`が流すと決定3の失敗になる）。

## Alternatives

- **`cargo test`のまま`--test-threads`や`RUST_TEST_THREADS`を上げる**: binaryの中の並列が増えるだけで、binaryは直列のまま。遅いbinaryの一番遅いtestで頭打ちになり、20本超のbinaryの合計は縮まない。
- **`cargo test`をbinaryごとに別processで並べるscriptをrepositoryに置く**（`cargo test --test X`を並列に起動する）: nextestが既にしていることを自前で作ることになり、`cargo llvm-cov`のprofrawの集め方（`cargo llvm-cov --no-report`を並べて`cargo llvm-cov report`）とtestの一覧の保守がrepositoryの仕事になる。binary単位の並列は一番遅いbinaryで頭打ちになる。
- **test binaryを1本にまとめる**（`tests/*.rs`を1つのcrateのmoduleにする）: binaryの中の並列で全testが並ぶが、testの大きな組み替えになり、`CARGO_BIN_EXE_dagq`を使うtestと使わないtestが同じprocessで状態を共有するようになる。nextestはtestの構成を変えずに同じ効果を得る。
- **`.config/nextest.toml`の`test-threads = 4`で絞る**: 1か所で済むが、CIと人の手元のnextestまで4に絞られる。絞る理由はhost 1台でrunを並べることなので、他の絞り込みと同じ`[run.env]`に置く。
- **並列度をCPU数（nextestの既定）のままにする**: 速い見込みはあるが、worker 3本と同時に`integrate`が8 processを走らせ、task 427で絞った合計の上限を超える。4で入れて実測で上げる。
- **workerの手元の`cargo test`もnextestにする**: workerの繰り返しの検証も速くなりうるが、workerの環境にnextestを前提し、hostに無いときworkerの検証まで止まる。workerのtestは1 binaryずつ回す使い方も多く、関門は`integrate`の1回（ADR-0049の決定1）なので、効果の大きい`integrate`だけを変える。
- **runtimeが`verification_commands`からcargoのsubcommandを推して事前に検査する**: 見つからないときの`needs_session`を防げるが、commandの文字列を解釈する仕組みを足すことになり、決定3の切り替えの順で防げる。実際に起きたらADR-0049の決定9に宣言の表を足す。
- **見つからないときに旧コマンドへ落とすwrapperを置く**: 失敗はしないが、遅いほうで流れていることに誰も気づかない（ADR-0049の決定8と同じ理由）。
- **既存のtaskのverificationを一括で書き換える**: 速くなるが、登録済みのtaskの検証を人とplan reviewの外で変えることになる。旧コマンドも同じ関門なので、書き換える必要が無い。
- **`retries`で不安定なtestを再実行する**: 関門は通りやすくなるが、並列にしたことで出る競合（決定1）を隠す。落ちたら直す。

## Consequences

- runtime（`src/`）を触るtaskの`integrate`の検証の`cargo llvm-cov`の段は、testの部分が並列になり、1着地あたり100〜150秒縮む見込み。数字は後続taskが測り、goal 36のnoteに残す。
- hostにcargo-nextestとmiseのshimへのlinkが要る。無いhostでは、新しいverificationを持つtaskの`integrate`が失敗し、workerは直せずに人に返す。そのhostでは旧コマンドでtaskを登録すれば今までどおり動く。
- `integrate`の検証が同時に動かすtestのprocessは最大4（`NEXTEST_TEST_THREADS`）で、今の`RUST_TEST_THREADS`の4 threadと同じ上限。1つのtestがprocessを1つ使うので、processの起動とprofrawのfileの数が増え、`cargo llvm-cov`のreportの集計が少し重くなる。
- coverageの関門は旧コマンドと新コマンドの2つの書き方を持つ。どちらも有効で、plan reviewは区別しない。
- testが1件ずつ別processになるので、今後書くtestはbinaryの中の他のtestとprocessの状態を共有することに頼れない。`tests/common/mod.rs`のfixtureと`within`はprocessの中で閉じているので、そのまま使える。
- workerの手元の検証、e2e、sccache、targetの扱い（runごと）は変わらない。
