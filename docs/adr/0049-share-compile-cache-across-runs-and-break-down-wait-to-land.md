---
id: adr-0049
type: adr
title: 検証をintegrateの1回にし、reviewをsupervisorの工程にし、dagq.tomlでrunのenvを渡してsccacheでcompileの結果をrun間で共有し、taskの5段階の優先度と解放数でclaim順を決め、statsで詰まりを数える（ADR-0040を統合）
status: accepted
created: 2026-09-26
updated: 2026-09-26
accepted_on: 2026-09-26
supersedes:
  - adr-0040
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - operations
  - performance
related:
  - adr-0008
  - adr-0022
  - adr-0026
  - adr-0027
  - adr-0029
  - adr-0038
  - adr-0040
  - adr-0042
  - adr-0045
  - adr-0047
  - design-supervisor-lifecycle
  - design-domain-model
  - design-persistence
  - design-provider-lifecycle
---

# ADR-0049: 検証をintegrateの1回にし、reviewをsupervisorの工程にし、dagq.tomlでrunのenvを渡してsccacheでcompileの結果をrun間で共有し、taskの5段階の優先度と解放数でclaim順を決め、statsで詰まりを数える（ADR-0040を統合）

## Context

[ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)（2026-09-25）は実行効率のために5つを決めた: 検証を`integrate`の1回にする、reviewをsupervisorの工程にしてpassなら着地する、`dagq.toml`でrunのenvを渡す、taskの優先度と解放数でclaim順を決める、`stats`で詰まりを数える。決定3には「この repositoryではtargetを共有せず、`dagq.toml`も置かない（2026-09-23のユーザーの決定）。buildの共有はsccacheなど安全な方法を別途検討する」とある。

goal 36（並列数を上げて得をできるようにする）がその検討を求めた。並列数は4で、8コアのhostでload averageが12〜22に達している。runごとにworktreeとtargetが新しいので、runを1本増やすと依存crateを含むbuild・test・llvm-covが丸ごと1本増える。targetの共有は次の2つの理由でやめた（ADR-0040の決定3、AGENTS.md）。

- (a) cargoのlockはbuildだけを直列化し、その後のtest実行は分離されないので、`CARGO_BIN_EXE_dagq`をexecするtest（`tests/cli.rs`・`runtime.rs`・`location.rs`・`plugin.rs`・`e2e.rs`）が、並行する別のrunのbuildが上書きした`target/debug/dagq`を実行しうる。
- (b) 同時の`cargo llvm-cov`が共有の`llvm-cov-target`のprofrawを消し合い・混ぜ合い、coverageの関門が誤る。

ADR-0040の後に次の変更が入った。

- 決定4の優先度（`add --priority`、`set-priority`、効く優先度）はgoal 28で実装された。goalの`rank`はまだ無い。
- `dagq.toml`に`[run.env]`以外の表が加わった・加わることになった: `[stall]`（[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定33。ADR-0043から引き継ぎ、実装済み）と`[conflicts]`（goal 31で実装。ADRは無く、設計は[supervisor-lifecycle](../design/supervisor-lifecycle.md)のConflict thresholds）。`[resume]`・`[exit]`・`[disk]`はADR-0047の決定24・25・44が決め、まだ実装されていない。
- ADR-0040が参照した[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)と[ADR-0024](0024-retire-maintainer-into-jobs-and-observer.md)はADR-0047（とADR-0041・ADR-0044を経て）に置き換えられた。ADR-0019の決定N（1〜6）はADR-0047の決定23+Nになった。

着地待ち（`wait_to_land`、最初の`validation_finished`→`run_integrated`）は直近159 runで中央値6分・上位10%が66分以上・1時間超が17本で、合計を長い裾が占める。今の`stats`ではreview・revise・merge-treeの事前判定・人の判断待ち・`integrate`の順番待ちと検証・push・resumeのどれに時間を使ったか分からない（goal 36のdescription）。

### 測定（2026-09-26、このhost）

sccache 0.18.0（`mise use -g sccache`で入っている）で、この repositoryの同じcommit（`6193376`）をscratchpadに5本cloneし、sccacheのcacheとserverをこのrepositoryのqueueや既存のcacheと分けて（`SCCACHE_DIR`と`SCCACHE_SERVER_PORT`を測定専用にして）測った。run 1本のbuildを再現するため、clone（= worktree）ごとに既定のtarget（`<clone>/target`）を使った。測定中もこのhostでは他のrunが走っていて、load averageは15〜26だった。所要時間はその影響で±25%ほど揺れるので、hit率を主な数字とし、時間は目安とする。

| コマンド | sccacheなし | sccache・空のcache | sccache・別のcloneが温めたcache | 温めたcacheのhit |
| --- | --- | --- | --- | --- |
| `cargo test --locked --no-run` | 169秒 / 206秒 | 258秒 | 204秒 / 223秒 | 57 / 64 |
| `cargo clippy --locked --all-targets -- -D warnings` | 75秒 | 157秒 | 47秒 | 43 / 50 |
| `cargo llvm-cov --locked --no-report -- --list` | 268秒 / 232秒 | 275秒 | 143秒 / 201秒 | 57 / 64 |

- hitの分母はsccacheがcacheできたcompile（依存crateのrlib）。cacheできないものは、`incremental`（workspaceのcrate。devのprofileはincrementalが既定）12件、`crate-type`（bin、proc-macro、build script）17件で、どのcloneでも毎回compileした。温めたcacheで外れた7件は、build scriptの`OUT_DIR`（targetの中の絶対path）を使うcrate（`serde_core`・`serde`・`serde_json`など）だった。
- workspaceのcrate（`dagq`とtestのbinary）はworktreeのpathが違うのでrun間では当たらない。`CARGO_INCREMENTAL=0`にしてもhitは増えない（依存1つの小さなcrateを2つのdirectoryでbuildした別の測定で、workspaceのcrateはmissのままだった。`SCCACHE_BASEDIRS`をclientごとに渡しても同じ）。
- `CARGO_TARGET_DIR`をcloneごとに明示すると、依存crateも含めてhitが0になった（最初の測定で、`cargo test`の64件すべてがmiss）。sccacheは`CARGO_`で始まる環境変数をcacheの鍵に含めるので、runごとに違う値の`CARGO_TARGET_DIR`を渡すと共有できない。
- `cargo llvm-cov` 0.9.1は自分を`RUSTC_WRAPPER`にしてworkspaceのcrateだけに`-C instrument-coverage`を付け、外側の`RUSTC_WRAPPER`（sccache）を呼び継ぐ。依存crateは計測なしでcompileされてcacheに当たり、計測付きのworkspaceのcrateは毎回compileされる。llvm-covの鍵は`cargo test`の鍵とは別で、llvm-cov同士で当たる。
- 3種のコマンドを3本のcloneで回した後のcacheは115MBだった。

依存crateの共有で、1 runのbuildのうち依存のcompileは省けるが、workspaceのcrateのcompileとlinkは残る。所要時間の差は、`cargo test`では測定の揺れより小さく、`clippy`では75秒→47秒、`llvm-cov`では2組で268秒→143秒と232秒→201秒（-47%と-13%）だった。loadの揺れが大きく、縮む幅はこの測定では言い切れない。確かなのは、依存crateの57件のcompileを省けることで、その分のCPUが並行するrunに回る。`llvm-cov`は`integrate`が必ず1回走らせる検証（決定1）なので、縮めば着地の直列の区間も縮む。導入後の数字は決定10で取り直す。

[ADR-0042](0042-adr-is-superseded-whole-and-deprecation-date-is-deprecated-on.md)の決定2により、ADR-0040の決定3を変える本ADRはADR-0040を丸ごと置き換える（2026-09-26にユーザーが統合を選んだ。ask 94）。決定1〜5はADR-0040の決定1〜5を同じ番号で書き直したもので、決定3にbuildの共有を、決定5に`wait_to_land`の内訳を足した。決定6〜11はbuildの共有を決める。

## Decision

**原則。** 同じcommitに対する高価な処理は1回にし、判断を含まない待ちはsupervisorの工程にする。claim順は人が付けた優先度を最初に、並列度（解放数）を次に見る。runの間で共有するのは内容で鍵を引くcacheだけにし、runが書き込む場所（worktree、target、profraw）は共有しない。run_eventsのkindは追加だけで、既存のkind名とpayloadは変えない。schemaを変えるtaskは`user_version`を上げて`migrations/`に追加する。

### I. 検証・review・run env・claim順・stats（ADR-0040の決定1〜5）

1. **`validating`はreceiptの照合だけを行い、`verification_commands`は`integrate`のrebase後に必ず1回走らせる。**（ADR-0040の決定1を変えずに引き継ぐ。実装済み）
   - `validating`が見るのは、receiptの整合（形式、`run_id`、`result`）、commitがrun branchのheadで`base_commit`の子孫であること、worktreeがcleanであること、taskが要求するevidence（ADR-0047の決定28）と、taskの`paths`（[ADR-0029](0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)）だけ。`verification_command`のeventは`validating`では記録しない。
   - `integrate`はrebaseの後、headが動いたかどうかにかかわらず`verification_commands`を1回実行し、出力を試行ごとの`integrate-<attempt>-verify-N.log`に残す。task 48の「rebaseがno-opなら再検証しない」判定は廃止した。event kindの`integration_verification_skipped`は過去のeventを読むために残し、記録はしない。`integrate`の出力の`verification_skipped`fieldも残し、常に`false`を返す。
   - 検証の失敗はrunを`needs_session`にし、supervisorの自動resume（ADR-0047の決定24）が同じsessionで直す。runを作り直さないので、作業を捨てない。
   - 1つのcommitに対する`verification_commands`の実行は`integrate`の1回だけで、`validating`の所要時間はreceiptとGitの照合だけになる。
2. **reviewをsupervisorの工程にし、passならsupervisorが着地させる。**（ADR-0040の決定2を変えずに引き継ぐ。実装済み）
   - `awaiting_integration`になったrunに対し、supervisorは`review ID`と同じ関数で`review.md`を書き、`review_started`を記録する。
   - review本体は`AgentProvider`のportのheadless実行で行う。Claudeでは`claude -p`をrun dirの設定（`settings`）で起動し、promptに`review.md`のpathとverdictのJSON schemaを渡し、stdoutのJSONを読む。reviewのためのcmux workspaceは作らない。reviewの間もworkerのsessionとworkspaceは閉じずに残す（[ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)の決定1）。
   - reviewが見る観点は[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定3の疑義と同じ: receiptや差分が受け入れ条件と食い違う、taskの指示にない変更を含む、判断を含むレビューの指摘がある。
   - verdictは`pass | revise | concern`（reviseの規則はADR-0027の決定2・3）で、`reasons`と`summary`とともに`review_finished`のpayloadに記録する。
   - **pass**: `git merge-tree`で衝突を事前判定し（ADR-0027の決定4）、衝突しなければsupervisorが着地させる。着地は`integrate`と同じland関数と単一の着地slotを使い、push（ADR-0047の決定26）まで行う。rebaseの衝突や検証の失敗は`integrate`と同じく`needs_session`になり、自動resumeの後は承認済みとして再び着地に進む。
   - **revise**: 生きているworkerのsessionに指摘を返す（ADR-0027）。
   - **concern**: `approve_landing`のask（options `land` / `send_back` / `cancel`）を作り、`reasons`と`summary`をquestionに載せて待つ。askはinboxが人に見せ、人の答えをsupervisorが適用する（`land`は着地、`send_back`は`needs_session`に戻してresumeで直させる、`cancel`はtaskを取り消す）。
   - **headless実行の失敗**（起動できない、非0終了、timeout、stdoutがschemaに合わない）: runを`awaiting_integration`のまま残し、`review_failed`を記録する。inboxが人に知らせ（`review by hand`）、人が手でreviewして`integrate`を呼ぶ。
3. **repository rootの`dagq.toml`の`[run.env]`をworkerと検証に渡す。この repositoryではtargetを共有せず、`[run.env]`でsccacheを渡してcompileの結果を共有する。**（ADR-0040の決定3を引き継ぎ、「`dagq.toml`も置かない」を改め、表の追加の記述を今の状態に合わせ、resumeのworkspaceにも渡すことにした。`[run.env]`の読み込みと3つの渡し先は実装済み、resumeへの渡しとsccacheの配置は後続task）
   - `[run.env]`はキーが環境変数名、値が文字列の表。`dagq.toml`のほかの表（`[stall]`・`[resume]`・`[exit]`・`[disk]`・`[conflicts]`）はそれぞれのADRとgoalが決めたもので、表を足すときはADRで決める。
   - 値には`${DAGQ_QUEUE_DIR}`（そのrepositoryのqueue directory）と`${DAGQ_RUN_DIR}`（そのrunのrun directory）を展開できる。それ以外の変数は展開しない。
   - supervisorはworkerのworkspaceを作るときに`workspace --env`で渡し（[ADR-0026](0026-identify-workspaces-by-uuid-env-and-queue-group.md)）、`integrate`が検証コマンドを実行するときに`Command`のenvで渡す。reviewのheadless実行にも同じenvを渡す。`needs_session`のresumeが開くworkspace（今は渡していない。supervisor-lifecycleのRun environment）にも同じenvを渡す。resumeは`integrate`の検証が失敗した後の普通の経路で、渡さないとresumeしたworkerだけがsccacheなしでbuildし、`RUSTC_WRAPPER`の違いで`integrate`の検証のbuildとも食い違う。
   - runtimeが読むのはmain checkoutの作業ファイルの`dagq.toml`で、runのworktreeのものではない。
   - この repositoryでは`CARGO_TARGET_DIR`を共有しない。理由はContextの(a)(b)で、2026-09-23のユーザーの決定を引き継ぐ。compileの結果の共有は決定6〜9のとおりsccacheで行い、そのために`dagq.toml`を置く（決定11の後続taskが置く）。
4. **taskに5段階の優先度を持たせ、claim順を「効く優先度 → goalのrank → 解放数 → ID」にする。**（ADR-0040の決定4を変えずに引き継ぐ。優先度は実装済み、goalのrankは未実装）
   - **優先度は5段階のenum**にする。

     | 名前 | DBの整数 | 意味 |
     | --- | --- | --- |
     | `interrupt` | 4 | 割り込み。他のreadyより必ず先（例: goal 28のtask） |
     | `urgent` | 3 | 運用を止めている不具合 |
     | `high` | 2 | 早め。他の作業の前提になる整理 |
     | `normal` | 1 | 既定 |
     | `low` | 0 | 後回し。条件付きの提案など |

     CLIの引数とJSONの出力は名前（小文字）で扱い、数値は受け付けない。DBでは整数で持ち、既定値は1（`normal`）、CHECK制約で0〜4に限る。domainのenumは同じ順（`low` < `normal` < `high` < `urgent` < `interrupt`）の`Ord`を持つ。
   - **付け方と変え方**: `add --priority LEVEL`で付け、`set-priority TASK LEVEL`で変える。変えられるのはtaskが`draft` / `ready`の間だけで、claimされた後（`in_progress`以降）は拒否する。走っているrunを割り込みで止めることはしない。優先度の変更は次のclaimにだけ効く。
   - **効く優先度**: 自分と、自分を推移的に待っている`ready`のtaskの優先度の最大値。待っているtaskは解放数（`unblocks`）と同じ依存グラフ（task依存と、[ADR-0038](0038-task-depends-on-a-goal-until-it-is-achieved.md)のgoal依存を所属taskへ展開したもの）で辿る。`draft`・`canceled`・`completed`のtaskと、draftのgoalに属するtaskからは継承しない。draftに退避したtaskや流す予定の無いtaskが依存元を押し上げないため。優先度の高いreadyのtaskが待っている依存元は、その優先度で先にclaimされる。
   - **claim順**は次の順に比べる。
     1. 効く優先度の降順
     2. goalの`rank`（goal 13で入れる。未実装の間は飛ばす）
     3. 解放数（`unblocks`）の降順。依存の推移閉包で、未完了のtaskだけを数える
     4. IDの昇順
   - **順序の判定は1か所**: application / domainの純粋関数1つにまとめ、`candidates`・`graph`・supervisorの`fill_slots`が共有する。`graph`は未完了（`completed` / `canceled`でない）taskの依存木と各taskの解放数、claim順に並べた`candidates`、`critical`の鎖を返す。
   - 順序は`graph`で再現できるので、登録順と違う順でclaimしても`claim_reordered`は記録しない。
   - **飢餓への対策は入れない**: 優先度の低いtaskがいつまでもclaimされない（飢餓）ことへの対策（待ち時間で優先度を上げるagingなど）は入れない。優先度は人が意図して付けるものなので、低い優先度のtaskが待つことも人が分かって選んでいる。待ちが問題になれば人が`set-priority`で上げられる。必要になれば、observerが「readyのまま長く待つtask」をnoteにする形で足す。
5. **`stats [--since <cursor>]`で時間と閾値超えを返し、着地待ちを工程別の内訳でも返す。**（ADR-0040の決定5を引き継ぎ、`wait_to_land`の内訳を足した）
   - run単位: claim→receipt（作業）、receipt→`validation_finished`（validating）、`validation_finished`→`integrated`（着地待ち、`wait_to_land`）、resume回数、reviewのverdict。
   - goal単位: 上の各区間の合計と中央値。
   - 閾値超え: `awaiting_integration`が15分を超えたrun、3回目の`needs_session`、60分答えられていないask、同じtaskの`failed`が2回、作業時間がそのgoalの中央値の2倍を超えたrun、空きslotがあるのにcandidatesがゼロの状態。
   - `--since`は`status` / `events --after`と同じcursorを受ける。集計はrun_eventsから再導出し、新しい表は持たない。observerは`stats --since <前回のcursor>`を入力にする（ADR-0047の決定4。ADR-0024の決定4から引き継がれたもの）。ADR-0047とgoal 31が足した項目（`running_alerts`・`auto_repairs`・`conflict_hotspots`など）もそのまま残す。
   - **`wait_to_land`の内訳**: 着地待ちを工程（review、revise、merge-treeの事前判定、人の判断待ち、着地slotの順番待ち、`integrate`の検証、push、resumeなど）に分けた時間を足す。区切りのeventと項目名は、goal 36のruntimeのtaskが今の`stats`の設計に合わせて決める。既存の出力項目は変えず、足すだけにする（ADR-0047の決定45の`stats`の追加と同じ扱い）。

### II. compileの結果をrun間で共有する（goal 36）

6. **runごとのtargetはそのままにし、sccacheを`RUSTC_WRAPPER`に置いて依存crateのcompileの結果をrun間で共有する。**
   - `dagq.toml`の`[run.env]`に`RUSTC_WRAPPER = "sccache"`と`SCCACHE_IGNORE_SERVER_IO_ERROR = "1"`を置く。workerのworkspace（resumeのものを含む）、`integrate`の検証、reviewのheadless実行に渡る（決定3）。
   - **`CARGO_TARGET_DIR`は渡さない**。targetは今までどおりworktreeの`target/`で、runごとに別。`[run.env]`で`CARGO_TARGET_DIR`をrunごとの値にすると、sccacheは`CARGO_`で始まる環境変数を鍵に含めるので、依存crateも含めて当たらなくなる（Contextの測定）。
   - **`CARGO_INCREMENTAL`は変えない**。workspaceのcrateはworktreeのpathが違うのでrun間では当たらず、incrementalを切ってもhitは増えない。一方、worker自身が同じworktreeで繰り返すbuildはincrementalで速くなる。
   - **cacheできるもの・できないもの**: 共有されるのは依存crateのrlibのcompileだけ。workspaceのcrate、bin・proc-macro・build script、link、testの実行、profrawの生成と集計は毎回そのrunのtargetの中で行う。`CARGO_BIN_EXE_dagq`はrunのtargetでlinkされたものだけを指し、`llvm-cov-target`とprofrawもrunのtargetの中にあるので、Contextの(a)(b)は起きない。
   - **clippyとllvm-cov**: `cargo clippy`はsccacheを通り、clippy同士で当たる。`cargo llvm-cov`は自分の`RUSTC_WRAPPER`からsccacheを呼び継ぎ、計測しない依存crateだけが当たる。どちらの鍵も`cargo test`とは別なので、cacheには同じ依存crateが用途ごとに入る。
   - **置き場所と上限**: sccacheの既定（macOSでは`~/Library/Caches/Mozilla.sccache`、上限10GB、serverのport 4226）を使い、`SCCACHE_DIR`・`SCCACHE_CACHE_SIZE`・`SCCACHE_SERVER_PORT`は`[run.env]`に置かない。この repositoryのcommit 1つ分の3種のbuildで115MBなので、10GBで依存の更新を多数またいでも足り、超えればsccacheが古いものから消す。serverは最初にcompileを頼んだclientが起動するので、`[run.env]`で置き場所を変えても、既に動いているserverの設定が使われ、runによって効いたり効かなかったりする。既定にそろえれば、人が手元で打つcargoのbuildとも同じcacheを温め合う。cacheの中身はcompilerと引数とinputの内容で鍵を引くので、共有しても出力は変わらない。
   - **並行するrunの安全性**: cacheのdirectoryを読み書きするのは1つのserverプロセスだけで、並行するrunのcargoは各自のclientからserverに頼む。serverは出力を頼んだrunのtargetに書くので、runが別のrunの出力を上書きすることはない。serverは最初のclientが起動するとdaemonになってclientから切り離され、workerのsessionの外で動き続ける（既定で10分idleなら終わる）。Claude Codeが`/exit`の前に確かめるのは自分が`run_in_background`で起動した処理なので、`/exit`の確認画面（AGENTS.mdのworker節）の原因にならない見込みで、導入後の最初のrunで確かめる（決定11の(3)）。
   - **serverが壊れたとき**: `SCCACHE_IGNORE_SERVER_IO_ERROR=1`で、serverと話せないとき（serverの異常終了、clientとserverのversionの食い違いなど）はclientがその場でrustcを直接実行する。cacheが効かないだけで、buildと検証は失敗しない。
7. **sccacheは人がmiseのglobalで入れ、`RUSTC_WRAPPER`はPATHで引く名前にし、`~/.local/bin`にmiseのshimへのlinkを置いてsupervisorの固定されたPATHでも解決できるようにする。**
   - 入れるのは人で、`mise use -g sccache`で入れる（goal 36のconstraints。workerは`brew` / `cargo install`でhostを変えない）。同時に人が`ln -s ~/.local/share/mise/shims/sccache ~/.local/bin/sccache`を置く。
   - `dagq.toml`には`RUSTC_WRAPPER = "sccache"`とPATHで引く名前だけを書き、host固有の絶対pathを書かない。`dagq.toml`はcommitされるので、別のhostでもそのhostのPATHで解決できる。
   - **PATHの事情**: `mise activate`はtoolのversionごとのinstall directory（例: `~/.local/share/mise/installs/sccache/0.18.0/sccache-v0.18.0-aarch64-apple-darwin`）をPATHに並べる。workerのworkspaceはcmuxが開くたびにlogin shellのPATHを作るので常に今のversionを引くが、supervisor（と、supervisorが走らせる`integrate`の検証とreview）のPATHは`up`の時点のもので固定される。sccacheを更新して古いversionのdirectoryが消えると、そのdirectoryはPATHから黙って飛ばされ、後ろの`~/.local/bin`のlinkに落ちる。linkはmiseのshimで、shimは呼ばれるたびにmiseのglobalの設定から今のversionを引くので、supervisorを入れ替えなくても解決できる。古いdirectoryが残っていれば古いversionが使われるが、cacheの共有と正しさは変わらない（clientとserverのversionが食い違ってserverと話せなければ、決定6の`SCCACHE_IGNORE_SERVER_IO_ERROR`で直接compileする）。
   - `~/.local/bin`はsupervisorのPATHにも入っている（固定バイナリ`~/.local/bin/dagq`をPATHで引いているのと同じ前提。AGENTS.md）。
   - sccacheを`up`の後に入れた・消したときの判定は決定9の検査で行う。
8. **ツールが無いhostでは、runtimeがbuildの前に検知して止め、黙って別のbuildに落とさない。CIは`dagq.toml`を読まないので影響を受けない。**
   - **CI**（`.github/workflows/ci.yml`）と、dagqを使わずに打つ`cargo`は`dagq.toml`を読まないので、sccacheが無くても今までどおりbuildする。
   - **dagqのruntime**（sccacheの無いhostでこのrepositoryのqueueを動かす人）: `[run.env]`の`RUSTC_WRAPPER`が解決できないと、cargoは`could not execute process`ですぐ失敗する。そのまま流すと、workerの検証は失敗し、`integrate`の検証は`needs_session`になってresumeを使い切る。そこで決定9の検査で、`up`はsupervisorを起動せず、supervisorはclaimを止め、`integrate`は検証コマンドを実行せずに止まる。どれも何が見つからないか（変数名・値・探したPATH）と対処（sccacheを入れる、または`dagq.toml`から外すtaskを登録する）を示す。
   - 見つからないときに黙ってrustcに落とすwrapper（repositoryにscriptを置き、sccacheがあれば使い、無ければrustcを直接呼ぶ）は置かない（Alternatives）。
9. **`[run.env]`が名指すプログラムは、既知の変数から推して、`up`のpreflight・supervisorのclaimの前・`integrate`の検証の前・`doctor`で解決できるかを検査する。**
   - **検査する変数**: cargoがプログラムとして実行する既知の変数。`RUSTC_WRAPPER`・`RUSTC_WORKSPACE_WRAPPER`・`RUSTC`・`RUSTDOC`と、その`CARGO_BUILD_`付きの形（`CARGO_BUILD_RUSTC_WRAPPER`・`CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER`・`CARGO_BUILD_RUSTC`・`CARGO_BUILD_RUSTDOC`）。一覧はruntimeの1か所（`run_env`の読み込みと同じmodule）に持つ。`dagq.toml`に必要なツールを宣言する表は今は足さない。cargo以外のツールを`[run.env]`で名指す必要が出たら、そのとき表を足すADRを書く。
   - **解決の規則**: 値が空なら検査しない（cargoでは空の`RUSTC_WRAPPER`はwrapperなし）。`/`を含む値はそのpathが実行可能なfileであること、含まない値は検査するプロセスのPATHの中に実行可能なfileがあること。`${DAGQ_*}`は展開してから見る。
   - **`up`のpreflight**: 既存のpreflight（queue、repository、cmux、Claude、trust）と並べて、`up`のPATHで検査する。見つからなければsupervisorを起動せず、変数名・値・PATHを挙げたerrorで止まる。`up`のPATHは、`up`が起動するsupervisorのPATHであり、同じlogin shellから作られるworkerのworkspaceのPATHにも近い。
   - **supervisorのclaimの前**: 起動時と各fill passのclaimの前に、自分のPATHで検査する（fileの有無を見るだけなので安い）。見つからなければそのpassではclaimしない。走っているrun、review、resumeは止めない。見つからない状態に変わったときに1回だけ`run_env_program_missing`（payload: `variable`、`value`、`path`）を記録し、inbox宛てのattention（`install tool`）にする。見つかるようになれば`run_env_program_found`を記録してclaimを再開し、attentionは消える。
   - **`integrate`の検証の前**: 検証コマンドを実行する前に同じ検査をし、見つからなければ検証コマンドを実行せず、`dagq.toml`が読めないときと同じく着地処理のエラーにする（runは元の状態に戻り、`needs_session`にしないのでresumeを使わない）。`last_error`に変数名と値を書く。supervisorは`run_env_program_missing`の間はpassしたrunの着地に進まず、runを`awaiting_integration`のまま置いて着地slotを空ける（ADR-0047の決定44の`disk_low`と同じ扱い）。着地を繰り返し試みて同じエラーを積まないため。`run_env_program_found`の後に着地を再開する。
   - **`doctor`**: `run_env`の欄に、各変数の値と、`doctor`を打ったプロセスのPATHで解決したpath（見つからなければnull）と、supervisorが最後に記録した`run_env_program_missing` / `run_env_program_found`を出す。状態は変えない。
   - workerのworkspaceのPATHはsupervisorから直接は見えない。workerのPATHでだけ見つからない場合は、workerの検証が失敗してreceiptかtriage（ADR-0047）で分かる。`up`のPATHで検査していることがその大半を先に防ぐ。
10. **効果は、同じcommitの別々のworktreeでのcold / warmのhit率と所要時間と、有効にした前後の`integrate`の検証の所要時間で測る。**
    - **導入前**（本ADR）: Contextの測定。scratchpadに同じcommitのcloneを作り、測定専用の`SCCACHE_DIR`と`SCCACHE_SERVER_PORT`で、sccacheなし / 空のcache / 別のcloneが温めたcacheの3通りに`cargo test --locked --no-run`・`cargo clippy --locked --all-targets -- -D warnings`・`cargo llvm-cov --locked --no-report -- --list`を回し、`sccache --show-stats`のhit・miss・cacheできない理由と所要時間を記録した。
    - **導入後**（決定11の後続task）: `dagq.toml`を置いた後の着地から、`integrate`の`verification_command`のevent（`integrate-<attempt>-verify-N.log`）で`cargo llvm-cov`の所要時間を、有効にする前の同じ数（10 run以上）の着地と比べる。あわせて`sccache --show-stats`のhit率と、`stats`のclaim→receipt（作業）の中央値を前後で比べる。loadが所要時間を大きく揺らすので、前後の比較には同じ並列数の期間を使い、測った時間帯のload averageを添える。数字はgoal 36のgoal review（goal acceptanceの(1)）とnoteに残す。
11. **AGENTS.mdの「targetを共有せず、`dagq.toml`も置かない」の項は、`dagq.toml`を置く後続taskで次のように改める。**
    - 残すこと: runごとの`CARGO_TARGET_DIR`を共有しないことと、その理由(a)(b)。
    - 改めること: 「`dagq.toml`も置かない」を、「`dagq.toml`の`[run.env]`で`RUSTC_WRAPPER = "sccache"`と`SCCACHE_IGNORE_SERVER_IO_ERROR = "1"`を渡し、依存crateのcompileの結果をrun間で共有する（ADR-0049）」にする。`CARGO_TARGET_DIR`と`CARGO_INCREMENTAL`を`[run.env]`に置かない理由（決定6）を1行添える。
    - 足すこと: sccacheは人が`mise use -g sccache`で入れ、`~/.local/bin/sccache`にmiseのshimへのlinkを置くこと（決定7）。見つからなければ`up`とclaimと`integrate`が止まり、inboxに`install tool`が出ること（決定9）。
    - 後続taskの順: (1) runtimeが決定9の検査と決定5の`wait_to_land`の内訳を入れる（`--evidence e2e`とllvm-covの検証を付ける）。決定3のresumeへの`[run.env]`の受け渡しも同じtaskで入れる。(2) (1)の入った固定バイナリ`~/.local/bin/dagq`に人が入れ替え、人がsccacheとlinkを入れたことを確かめてから、`dagq.toml`を置き、AGENTS.mdと[supervisor-lifecycle](../design/supervisor-lifecycle.md)のRun environmentを改める。検知の無い古いsupervisorの下で`dagq.toml`を置かないため。(3) 決定10の導入後の測定をし、最初のrunでsccacheのserverが`/exit`の確認画面を出さないこと（決定6）を確かめる。

## Alternatives

- **`validating`で検証を残す**（ADR-0040から）: worker直後に失敗を見つけられるが、同じcommitに対して2回走り、1回2〜6分のCPUと時間を毎run払う。失敗は`integrate`の`needs_session`と自動resumeで同じsessionが直せるので、2回分のコストに見合わない。
- **reviewを常駐sessionのsubagentのままにする**（ADR-0040から）: 実装は要らないが、sessionが起きてレビューを回すまで着地が進まず、着地待ちが人とsessionの都合に左右される。
- **claim順を登録順のままにし、plannerが登録順で並列度を調整する**（ADR-0040から）: 依存の追加やcancelで最適な順が変わるたびに登録し直すことになる。解放数はqueueから再計算できるので、runtimeが持つ。
- **優先度を自由な整数にする**（ADR-0040から）: 付けるたびに他のtaskより少し大きい数を選ぶ上げ合いになり、値の意味（どの数ならどれくらい急ぐか）が定まらない。人とplannerが同じ基準で付けられるよう、意味を名前で持つ段階にする。
- **-3〜3などの範囲付きの整数にする**（ADR-0040から）: 上げ合いの上限はできるが、各値の意味は依然として決まらず、名前の無い数字を覚えることになる。5段階で運用の場面（割り込み、運用停止、前提の整理、既定、後回し）を覆えるので、段階に名前を付けたenumにする。
- **goalのrankを優先度より先に比べる**（ADR-0040から）: goalの間の順序を常に優先するので、goalの中の急ぎのtask（運用を止めている不具合など）がrankの低いgoalに属すると後ろに回る。優先度は人がtask単位で付ける明示の指示なので、最初に比べる。goal 13のtask 112（draft）はrankを先頭に置く前提で書かれているので、goal 13を始めるときにこの順に合わせて作り直す。
- **優先度を継承しない**（ADR-0040から）: 急ぎのtaskがreadyでも、その依存元が`normal`なら依存元は他の`normal`の後ろに回り、急ぎのtaskはいつまでも着手できない。依存元にも同じ優先度を付けて回る手間は、依存が深いほど増える。
- **draftのtaskからも継承する**（ADR-0040から）: draftに退避したtaskや、まだ流すか決めていないtaskが依存元を押し上げる。goal 28のために37のtaskをdraftに退避したような運用で、退避したtaskの優先度が残りのclaim順を乱す。
- **`add`のときだけ付けられる**（ADR-0040から）: 状況が変わって急ぐことになったtaskに優先度を付けるには、登録し直すことになる。draft / readyの間は依存やgoalの所属と同じく変えられるようにする。
- **走っているrunを割り込みで止める**（ADR-0040から）: `interrupt`のtaskのために走行中のrunを止めると、作業を捨てるかsessionの保存と再開が要る。slotが空くのを待てば次のclaimで先頭に来るので、割り込みはclaim順だけにする。
- **待ち時間で優先度を上げる（aging）**（ADR-0040から）: 飢餓は防げるが、人が付けた優先度の意味が時間で変わり、どのtaskが次に取られるかを`graph`だけで読めなくなる。
- **`CARGO_TARGET_DIR`を共有する**（ADR-0023の決定3の当初の案）: workspaceのcrateまでcacheが効くが、Contextの(a)(b)でtestとcoverageの関門が誤りうる。
- **runごとの`CARGO_TARGET_DIR`を`[run.env]`で明示する**（例: `${DAGQ_RUN_DIR}/target`）: sccacheの鍵に入り、hitが0になる（測定）。既定のworktreeの`target/`で足りる。
- **`CARGO_INCREMENTAL=0`を`[run.env]`に置く**: sccacheがworkspaceのcrateもcacheの対象にするが、worktreeのpathが違うのでrun間では当たらず、workerの繰り返しのbuildからincrementalを奪うだけになる。
- **sccacheの`SCCACHE_BASEDIRS`でpathの違いを消す**: sccache 0.18にはあるが、runごとに違うpathを1つのserverの設定に並べることになり、runのたびにserverの設定が変わる。clientごとの値としてdirectoryを渡した測定でもworkspaceのcrateは当たらなかった。
- **cacheの置き場所・上限・portを`[run.env]`で決める**（例: `SCCACHE_DIR = "${DAGQ_QUEUE_DIR}/sccache"`と専用のport）: queueごとにcacheを分けて管理できるが、portと組にしないと既に動いているserverの設定に負け、組にすると人の手元のbuildとcacheを温め合えず、portの番号をrepositoryごとに決めて回ることになる。10GBの既定で足りるので既定にそろえる。
- **他の共有の方法**: targetのうち依存crateの部分だけを読み取り専用で共有する仕組み（cargoのbuild directoryの分離など）は、runtimeが依存の更新とcacheの整合を自分で管理することになり、Contextの(a)(b)と同じ種類の取り違えを作りうる。内容で鍵を引くsccacheはその管理が要らない。hostの外に置くcache（sccacheのS3・GHAなど）は今のhost 1台の運用では要らない。
- **`RUSTC_WRAPPER`にmiseのshimの絶対pathを書く**（`/Users/<user>/.local/share/mise/shims/sccache`）: 更新に強いが、commitされる`dagq.toml`にhost固有のpathが入り、runtimeは`$HOME`を展開しない（決定3）。展開を足すのは決定3を変えることになり、PATHで引く名前とlinkで同じことができる。
- **versionを固定したinstall directoryの絶対pathを書く**: sccacheを更新して古いversionを消すと、supervisorを入れ替えるまで`integrate`の検証が壊れる。versionを上げるたびに`dagq.toml`を変えるtaskが要る。
- **repositoryの`mise.toml`でsccacheを管理する**: versionをrepositoryで固定できるが、miseを使わない人やCIにmiseを求めることになり、runのworktreeはrunごとに新しいpathなので、miseがworktreeの`mise.toml`を信頼するかを聞いて止まる（trust）。shimもcwdの`mise.toml`を読むので、信頼されていないworktreeでは解決に失敗しうる。hostのツールは人がglobalで入れる（goal 36のconstraints）ほうが単純なので採らない。
- **見つからないときにrustcへ落とすwrapperをrepositoryに置く**: 失敗はしないが、cacheが効いていないことに誰も気づかず、build時間が黙って倍になる。wrapperのpathはworktreeごとに違い、`dagq.toml`から指すにはmain checkoutのpathの展開が要る。検知して止めれば、人は入れるか外すかを選べる。
- **必要なツールを`dagq.toml`の表（例: `[run.require]`）で宣言させる**: cargo以外のツールも検査できるが、今必要なのは`RUSTC_WRAPPER`だけで、変数から推せる。宣言と`[run.env]`の二重管理になるので、必要になったときにADRで足す。
- **見つからないときに警告だけでclaimを続ける**: 流したrunが検証で必ず失敗し、resumeとtriageを消費してから人に届く。claimの前に止めるほうが安い。

## Consequences

- ADR-0040は`superseded`になり、本ADRが検証の1回化・supervisorのreview・run env・claim順・statsの現行の決定と、compileの結果の共有を1本で持つ。ADR-0040の「この repositoryでは`dagq.toml`も置かない」は採らない。
- 他のADR・design文書・AGENTS.mdがADR-0040の決定N（1〜5）を参照している箇所は、本ADRの同じ番号の決定として読める（番号をそろえた。既存ADRの本文は書き換えない）。AGENTS.mdとdesign文書の参照は、決定11の後続taskで改める。
- taskの`priority`の列、`add` / `set-priority`、`list` / `show` / `graph`の優先度の出力はADR-0040のとおり入っている（goal 28）。`candidates`・`graph`・supervisorのclaimが同じ純粋関数の順を使うので、plannerは`graph`で次にclaimされるtaskを読める。低い優先度のtaskは、それより高い効く優先度のreadyのtaskがある限りclaimされない。飢餓はruntimeが防がず、人とobserverが気づく前提になる。goal 13のrankは、効く優先度の次・解放数の前に比べる位置で入る。
- runtimeは決定9の検査（`up`・claim・`integrate`・`doctor`）と、決定5の`wait_to_land`の内訳を実装する。event kindは`run_env_program_missing` / `run_env_program_found`を足し、既存のkindは変えない。
- hostにsccacheとmiseのshimへのlinkが要る。無いhostでこのrepositoryのqueueを動かすと、`up`とclaimと`integrate`が止まり、入れるか`dagq.toml`から外すかを人が選ぶ。CIとdagqを使わない`cargo`には影響しない。
- 共有で省けるのは依存crateのcompileだけで、workspaceのcrateのcompile・link・testの実行は残る。runを1本増やしたときのCPUの増分は小さくなるが無くならないので、並列数を上げるかは決定10の導入後の数字（build時間、load、着地待ち）で人が判断する（goal 36のacceptanceの(4)）。
- 依存crateを更新するtaskの後の最初のrunは、cacheに無いのでsccacheなしより遅い（空のcacheの測定で、cacheへの書き込みの分だけ遅くなった）。2本目からは当たる。
- sccacheのserverはhostに1つ常駐し（idleで終わる）、cacheは既定の場所に最大10GB溜まる。消すときは`sccache --stop-server`と既定のcache directoryの削除で、runtimeは触らない。
