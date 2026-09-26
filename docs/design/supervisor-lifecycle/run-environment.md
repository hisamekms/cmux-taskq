---
id: design-supervisor-lifecycle-run-environment
type: design
title: "Run environment"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0040
  - adr-0076
  - adr-0049
---

# Run environment

repository rootの`dagq.toml`の`[run.env]`（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定3。ADR-0040の決定3を引き継ぐ）が、runごとの環境変数になる。読み込みは`src/infrastructure/run_env.rs`の純粋関数（`parse_run_env`と`expand`、fileを読む`load_run_env`）で、ファイルが無ければ空。

- 書式はTOMLの部分集合: 表は`[run.env]`・`[stall]`・`[conflicts]`・`[recheck]`だけを持ち、`[run.env]`の各行は`KEY = 'literal'`か`KEY = "basic"`（`\\` `\"` `\n` `\t`のescape）。`#`以降はcomment。ほかの表、表の外のkey、環境変数名でないkey、重複したkey、`DAGQ_`で始まるkey（runtimeが`DAGQ_ROLE` / `DAGQ_QUEUE`に使う）はエラーにする。
- 値の`${DAGQ_QUEUE_DIR}`はqueue directory（DBのある directory）、`${DAGQ_RUN_DIR}`はそのrunのrun directoryに展開する。ほかの`$`は書いたまま残す（shellの展開はしない）。`${DAGQ_RUN_DIR}`はtask 91で加えた（ADR-0040の決定3、ADR-0049の決定3が引き継ぐ）。
- 読むのはrepositoryのmain checkout（Git common directoryが`.git`ならその親、bareなら`supervise` / `integrate`を実行したcheckout）の作業ファイルで、run worktreeのものではない。`integrate`をどのworktreeから呼んでも同じファイルを読む（common directoryが`.git`という名前でない構成だけは、実行したcheckoutのものを読む）。検証コマンドが1件も無ければ読まない。
- 渡し先: (a) `provision`がworkerのworkspaceを作るとき、`DAGQ_ROLE` / `DAGQ_QUEUE`の後ろに`--env KEY=VALUE`で並べる（ADR-0026の仕組み）。worktreeを作る前に読むので、壊れた`dagq.toml`はprovisioningの失敗になり、workspaceは開かずsupervisorはclaimを止める。(b) `integrate`の`verification_commands`を`Command`のenvに足す（validatingは検証コマンドを実行しない）。読めないファイルは着地処理のエラーで、runは元の状態に戻る。(c) reviewのheadless実行（ADR-0049の決定2）のコマンドのenvに足す。needs_sessionのresumeが開くworkspaceには今は渡していない。
- `dagq.toml`はrepositoryにcommitされ、値はworkspaceを開く`cmux`のargvに出るので、secretは入れない。
- この repositoryでは`dagq.toml`の`[run.env]`に`RUSTC_WRAPPER = "sccache"`と`SCCACHE_IGNORE_SERVER_IO_ERROR = "1"`を置き、依存crateのcompileの結果をsccacheでrun間に共有する（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定6・7、task 393）。sccacheは人が`mise use -g sccache`で入れ、`~/.local/bin/sccache`にmiseのshimへのlinkを置く。serverと話せないときはclientがrustcを直接実行する。targetは共有しない（ADR-0040の決定3をADR-0049の決定3が引き継ぐ。ADR-0023の決定3は`CARGO_TARGET_DIR = "${DAGQ_QUEUE_DIR}/target"`を置くとしたが、task 91の着地前のreviewの指摘を受けて2026-09-23にユーザーが決めた）。理由: (a) cargoのlockはbuildだけを直列化し、その後のtest実行は分離されないので、`CARGO_BIN_EXE_dagq`をexecするtest（`tests/cli_*.rs`・`runtime_*.rs`・`location.rs`・`plugin.rs`・`e2e.rs`）が、並行する別のrunのbuildが上書きした`target/debug/dagq`を実行しうる。(b) 同時の`cargo llvm-cov`が共有の`llvm-cov-target`のprofrawを消し合い・混ぜ合い、coverageの関門が誤る。`CARGO_TARGET_DIR`はrunごとの値でもsccacheの鍵に入って依存crateまで当たらなくなるので置かず、`CARGO_INCREMENTAL`も変えない（ADR-0049の決定6）。
- この repositoryでは`[run.env]`に`CARGO_BUILD_JOBS = "4"`と`RUST_TEST_THREADS = "4"`も置き、runごとのcargoの並列度（buildのjob数と、1つのtest binaryの中のtestのthread数）を絞る（2026-09-26に人がplannerと決めた。task 427）。渡し先(a)〜(c)のすべてに効くので、workerのworkspaceのcargo、`integrate`の`verification_commands`、reviewのheadless実行が絞られる。needs_sessionのresumeが開くworkspaceには渡らないので、そこでは絞られない。理由: hostは8コア / 16GBで、並列4の運用でload averageが最大151〜204に達し、cmuxのcaptureのtimeoutが400件を超え、runのstartupの中央値が約1100秒になった（goal 36のnote 8718の基準値）。値の決め方: supervisorの`--parallel`を3に下げ、worker 3本と`integrate` 1本が同時にcargoを回しても合計16並列（コア数8の2倍）程度に収まるようにする。`--parallel`やhostを変えたら合わせて見直す。targetを共有しない理由(a)(b)は、envで並列度を絞ることには当たらない。
- 同じ考え方で`[run.env]`に`NEXTEST_TEST_THREADS = "4"`を置き、coverageの関門（`cargo llvm-cov nextest --locked --fail-under-lines 80`）でcargo-nextestが同時に走らせるtestのprocess数を絞る（[ADR-0076](../../adr/0076-run-the-coverage-gate-tests-with-nextest.md)の決定2、task 518）。nextestは`RUST_TEST_THREADS`を読まず、既定ではCPU数だけ走らせるため。`.config/nextest.toml`には並列度を書かず（CIと人の手元のnextestまで絞られるため）、60秒を超えたtestを`SLOW`と出す`slow-timeout`だけを置く。cargo-nextestはsccacheと同じく人が`mise use -g cargo:cargo-nextest`で入れて`~/.local/bin/cargo-nextest`にmiseのshimへのlinkを置くが、`[run.env]`が名指すプログラムではないので決定9の検査の対象ではなく、無いhostでは`integrate`の検証の失敗になる（ADR-0076の決定3）。
- `[recheck]`は`command`（文字列。空白だけは拒否）の1 keyだけを持ち、着地のたびにsupervisorが着地待ちのrunをmainに載せた木で実行する軽い検査になる（[Landing recheck](landing-recheck.md)、[ADR-0068](../../adr/0068-recheck-waiting-runs-after-each-landing.md)の決定2。`load_recheck_command`、`Verifier::recheck_command`）。表が無ければmerge-treeだけを見る。commandには`[run.env]`と、queue dirの`recheck/target`を指す`CARGO_TARGET_DIR`が渡る。この repositoryでは固定バイナリを`[recheck]`を知るものに入れ替えた後に`command = "cargo check --locked --all-targets"`を足す（旧バイナリは未知の表を拒むので、先に足すとqueue全体が止まる）。

## `[run.env]`が名指すプログラムの検査

[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定8・9（task 395）。`[run.env]`の`RUSTC_WRAPPER`などが実行できないと、runのcargoは`could not execute process`ですぐ失敗し、そのまま流すとworkerの検証も`integrate`の検証も全部落ちてresumeを使い切る。そこでruntimeが、cargoを走らせる前に解決できるかを見る。

- **検査する変数**: cargoがプログラムとして実行する`RUSTC_WRAPPER`・`RUSTC_WORKSPACE_WRAPPER`・`RUSTC`・`RUSTDOC`と、その`CARGO_BUILD_`付きの形の8つ（`src/infrastructure/run_env.rs`の`PROGRAM_VARIABLES`）。それ以外の変数は見ない。
- **解決の規則**（`resolve_program`・`check_programs`・`check_run_env_programs`）: 値が空なら検査しない。`/`を含む値はそのpathが実行可能なfile、含まない値は検査するプロセスのPATHの各directoryに実行可能なfileがあること。`${DAGQ_QUEUE_DIR}` / `${DAGQ_RUN_DIR}`は展開してから見る。runの無い検査（`up`・claim・`doctor`）では`${DAGQ_RUN_DIR}`を含む値は検査しない（runの前にはrun directoryに何も無いため）。`dagq.toml`が無ければ何も検査せず、今までどおりに動く。結果は`domain::run_env::RunEnvCheck`（`config`、`path`、`programs[]`の`variable` / `value` / `resolved`）。
- **`up`のpreflight**: cmux・Claude・trustの後に、`up`のPATH（supervisorに渡す`UpEnvironment.path`）で検査する（`lifecycle::Ports::run_env_programs`）。見つからなければsupervisorを起動せず、変数名・値・PATHと対処（入れるか、`dagq.toml`から外すtaskを登録する）を挙げたerrorで止まる。`dagq.toml`があるrepositoryでは`up`の出力に`run_env`（検査の結果）が付く。
- **supervisorのclaimの前**: 各fill passのclaimの前に自分のPATHで検査する（`Verifier::run_env_programs`）。見つからなければそのpassではclaimしない。走っているrun、review、resume、triageは止めない。queueの最新の`run_env_program_missing` / `run_env_program_found`と答えが変わったときだけ記録する（見つからなくなったら`run_env_program_missing`、payloadは最初に見つからなかった`variable` / `value`と`path`、全部の`programs`、`message`、`supervisor`。見つかるようになったら`run_env_program_found`）。queueの記録と比べるので、supervisorを起動し直しても繰り返さない。`dagq.toml`が読めないときは前の答えのまま（provisioningがそのfileのerrorを出す）。
- **着地の保留**: 見つからない間、passしたrunは`awaiting_integration`のままleaseを持って着地slotを待ち（`Phase::AwaitingSlot`）、統合slotは取らない。見つかれば次のpassで着地に進む。検査はpassごとに（drain・handoff中も）行う。drain・handoff・claimの停止の最中に見つからなければ、supervisorは待たずにそのrunのleaseを返し、runは`awaiting_integration`のまま人の`review and integrate`になる。`approve_landing`の`land`の答えも、見つかるまで適用しない。
- **`integrate`の検証の前**: 検証コマンドを実行する前に同じ検査をrunの`run_dir`で行い、見つからなければ検証コマンドを実行せず、`dagq.toml`が読めないときと同じく着地処理のエラーにする（runは元の状態に戻り、`needs_session`にしないのでresumeを使わない。`last_error`に変数名と値とPATHが入る）。検証コマンドが無いtaskは検査しない。
- **変更の印**: supervisorは同じ周回で`[run.env]`を展開せずに読み（`Verifier::run_env_table`）、queueの秘密のsaltで鍵付けして正規化したhashがqueueの最新の`run_env_changed`と違うときだけ`run_env_changed`（hashと変わったキーの名前。値は書かない）を記録する。`RUSTC_WRAPPER = "sccache"`の追加・削除や`CARGO_BUILD_JOBS`の変更はこの印に表れる（[変更の印](marks.md)、[ADR-0051](../../adr/0051-kpi-time-series-report-and-push.md)の決定11）。attentionにはならない。
- **attention**: `run_env_program_missing`はinbox宛てのattention（`next: install tool`）で、`watch`はこのeventで起き、`status`は最新がmissingの間`kind: run_env_program_missing`、`status: missing`、`last_error`にその`message`を出す（`run_id` / `task_id`はnull）。`run_env_program_found`で消える。
- **`doctor`**: `run_env`の欄に`config`、`doctor`を打ったプロセスのPATH（`path`）、`programs`（`resolved`は見つからなければnull）、`missing`（件数）、supervisorが最後に記録した`run_env_program_missing` / `run_env_program_found`（`supervisor_last`）を出す。`dagq.toml`が無く記録も無ければ欄ごと出さない。
- workerのworkspaceのPATHはsupervisorからは見えない。workerのPATHでだけ見つからない場合は、workerの検証が失敗してreceiptかtriageで分かる。
- 2つのkindはtaskにもgoalにも紐づかない行なので、`0032_run_env_program_events.sql`が`run_events`を作り直してCHECKに足した（[persistence](../persistence.md)）。

