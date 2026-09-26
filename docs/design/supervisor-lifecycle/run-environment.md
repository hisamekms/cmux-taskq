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
---

# Run environment

repository rootの`dagq.toml`の`[run.env]`（[ADR-0040](../../adr/0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)の決定3）が、runごとの環境変数になる。読み込みは`src/infrastructure/run_env.rs`の純粋関数（`parse_run_env`と`expand`、fileを読む`load_run_env`）で、ファイルが無ければ空。

- 書式はTOMLの部分集合: `[run.env]`の表だけを持ち、各行は`KEY = 'literal'`か`KEY = "basic"`（`\\` `\"` `\n` `\t`のescape）。`#`以降はcomment。ほかの表、表の外のkey、環境変数名でないkey、重複したkey、`DAGQ_`で始まるkey（runtimeが`DAGQ_ROLE` / `DAGQ_QUEUE`に使う）はエラーにする。
- 値の`${DAGQ_QUEUE_DIR}`はqueue directory（DBのある directory）、`${DAGQ_RUN_DIR}`はそのrunのrun directoryに展開する。ほかの`$`は書いたまま残す（shellの展開はしない）。`${DAGQ_RUN_DIR}`はtask 91で加えた（ADR-0040の決定3）。
- 読むのはrepositoryのmain checkout（Git common directoryが`.git`ならその親、bareなら`supervise` / `integrate`を実行したcheckout）の作業ファイルで、run worktreeのものではない。`integrate`をどのworktreeから呼んでも同じファイルを読む（common directoryが`.git`という名前でない構成だけは、実行したcheckoutのものを読む）。検証コマンドが1件も無ければ読まない。
- 渡し先: (a) `provision`がworkerのworkspaceを作るとき、`DAGQ_ROLE` / `DAGQ_QUEUE`の後ろに`--env KEY=VALUE`で並べる（ADR-0026の仕組み）。worktreeを作る前に読むので、壊れた`dagq.toml`はprovisioningの失敗になり、workspaceは開かずsupervisorはclaimを止める。(b) `integrate`の`verification_commands`を`Command`のenvに足す（validatingは検証コマンドを実行しない）。読めないファイルは着地処理のエラーで、runは元の状態に戻る。(c) reviewのheadless実行（ADR-0040の決定2）のコマンドのenvに足す。needs_sessionのresumeが開くworkspaceには今は渡していない。
- `dagq.toml`はrepositoryにcommitされ、値はworkspaceを開く`cmux`のargvに出るので、secretは入れない。
- この repositoryではtargetを共有せず、`dagq.toml`も置かない（ADR-0040の決定3。ADR-0023の決定3は`CARGO_TARGET_DIR = "${DAGQ_QUEUE_DIR}/target"`を置くとしたが、task 91の着地前のreviewの指摘を受けて2026-09-23にユーザーが決めた）。理由: (a) cargoのlockはbuildだけを直列化し、その後のtest実行は分離されないので、`CARGO_BIN_EXE_dagq`をexecするtest（`tests/cli.rs`・`runtime.rs`・`location.rs`・`plugin.rs`・`e2e.rs`）が、並行する別のrunのbuildが上書きした`target/debug/dagq`を実行しうる。(b) 同時の`cargo llvm-cov`が共有の`llvm-cov-target`のprofrawを消し合い・混ぜ合い、coverageの関門が誤る。buildの共有はsccacheなど安全な方法を別途検討する。
