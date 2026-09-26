---
id: adr-0078
type: adr
title: e2eとplugin以外のintegration testを1つのtest binary（tests/it）にまとめ、testファイルの行数の制約はファイル単位のまま残す
status: accepted
created: 2026-09-26
updated: 2026-09-26
accepted_on: 2026-09-26
owners:
  - hisamekms
tags:
  - performance
  - testing
related:
  - adr-0076
  - design-overview
---

# ADR-0078: e2eとplugin以外のintegration testを1つのtest binary（tests/it）にまとめ、testファイルの行数の制約はファイル単位のまま残す

## Context

2026-09-26に人がplannerと、workerのtranscriptの時間の内訳（task 528の後にclaimしたruntimeの7 run）を見た。workerが手を動かす31分のうち、関係するtestだけの`cargo test`が7.6分（24%）で、1 runに12〜33回流し、1回50〜230秒の大半がcompileとlinkだった。integrateの検証（`cargo llvm-cov nextest`、[ADR-0076](0076-run-the-coverage-gate-tests-with-nextest.md)）も約5分かかる。

`tests/`にはtestファイルが45本あり、42本が`mod common;`を読む。cargoは`tests/*.rs`の1ファイルごとに1つのtest binary（1本 約13〜14 MB）を作るので、`tests/common`のcompileとdagqのlibへのlinkを42〜45回繰り返していた。同じgoal（goal 50）のtask 550でdev profileのdebug情報を減らした（binaryの合計は7%減）が、binaryの数は減らない。

goal 47で、末尾にtestを足す形の大きなファイルが別々のtaskの追記で衝突するのを避けるため、`tests/*.rs`と`tests/common/*.rs`を3,000行以下に保つ規則（`scripts/check-test-file-lines.sh`）を入れ、testを機能ごとのファイルに分けた。binaryの数が増えたのはこのためで、ファイルを分けること自体は残したい。

## Decision

1. **integration testを1つのtest binaryにまとめる**: `tests/e2e.rs`と`tests/plugin.rs`以外のintegration testは`tests/it/`に置き、`tests/it/main.rs`がファイルごとに`mod <ファイル名>;`を並べる1つのtest binary `it`にする。testの名前はmodule pathが付いて`runtime_claim::foo`のようになり、1ファイルのtestは`cargo test --locked --test it runtime_claim::`で絞る。`Cargo.toml`は`autotests = false`にし、`[[test]]`は`it`（`tests/it/main.rs`）・`e2e`・`plugin`の3本にする。testを足すときは`tests/it/<機能>.rs`に足し、新しいファイルは`main.rs`に`mod`を足す。
2. **e2eとpluginは別のbinaryのまま**: `--test e2e`（cmuxが要る`#[ignore]`のtest）と`--test plugin`（pluginの文書のtaskのverify）は名前で呼ばれているので変えない。
3. **helperの置き場所**: 複数のファイルで使うhelperは`tests/common`に置く。`plugin.rs`も`mod common;`で読む（`common::Bounded`）ので`tests/`の直下に残し、`tests/it/main.rs`は`#[path = "../common/mod.rs"] mod common;`で読む（`tests/it`の中からは`crate::common`）。runtimeのtestだけが使うfixtureとhelperは`tests/it/runtime_support`（`crate::runtime_support`、`watchdog!`のmacroのため`#[macro_use]`）に置く。
4. **行数の制約はファイル単位のまま**: `tests/`の下の全`.rs`（`tests/*.rs`・`tests/common/*.rs`・`tests/it/*.rs`・`tests/it/runtime_support/*.rs`）をどれも3,000行以下に保つ。`scripts/check-test-file-lines.sh`は`find tests -name '*.rs'`で再帰的に拾う。1つのbinaryにまとめても、衝突を避けるのはファイルを分けることなので、制約は置き場所が変わるだけで緩めない。
5. **testの中身は変えない**: ファイルは`git mv`で移し、各ファイルの`mod common;` / `mod runtime_support;`を`use crate::common;` / `use crate::runtime_support;`に替えた（`common`を使わないruntimeのファイルでは`mod common;`は`runtime_support`のためだけにあったので消した）。中身で変えたのは、移動で相対pathと名前が変わった2か所だけ: `queue_migration.rs`と`runtime_claim.rs`の`include_str!("../migrations/...")`を`../../migrations/...`に、`cli_version.rs`の`a_wait_past_its_limit_fails_with_the_test_and_the_condition`が自分のbinaryを`--exact deadline_probe`で呼び直して`test deadline_probe timed out`を探すところを`cli_version::deadline_probe`に。

### 同じprocessで走るようになることの扱い

`cargo test --test it`では、これまで別のbinary（別のprocess）だった全ファイルのtestが1つのprocessのthreadで走る。

- process全体の状態を変えるtestは無いことを確かめた。`tests/`（`e2e.rs`と`plugin.rs`を除く）に`set_var`・`remove_var`・`set_current_dir`は無い。子processの環境は`Command::env`で渡しており、testのprocessの環境は変えない。
- process内のstaticは`tests/common`の`OPEN`・`NEXT`（`within`の見張り）と`runtime_support`の`STUBS`（fixtureのdirごとのstub）で、どれも同じbinaryの複数のtestが同時に使う前提で作られている（以前から1つのbinaryの中のtestは並列に走っていた）。
- `within`の上限を過ぎるとtest binaryごとexit 101で終わるので、`cargo test --test it`では同じbinaryの残りのtestもそこで止まる（失敗の報告はtestの名前と待っていた条件で変わらない）。
- integrateのcoverageの関門`cargo llvm-cov nextest`はtestを1件ずつ別のprocessで流す（ADR-0076）ので、関門ではtest間でprocessの状態を共有しない。`cargo test`でだけ同じprocessになる。

## 計測

同じworktreeで、変える前（base 8ee936b）と後を続けて測った。環境は`RUSTC_WRAPPER=sccache`・`CARGO_BUILD_JOBS=4`・`RUST_TEST_THREADS=4`、host は8コア。loadは`sysctl vm.loadavg`の1/5/15分で、計測の前→後。

`src/lib.rs`に空行を1行足した直後の`cargo test --locked --no-run`（warmなbuildの後、1回ごとに1行ずつ足す）:

| | 1回目 | 2回目 |
|---|---|---|
| 前（1巡目） | 13.3秒（11.00 7.48 7.50 → 11.77 7.81 7.62） | 9.9秒（11.77 7.81 7.62 → 11.38 7.87 7.64） |
| 後（1巡目） | 5.4秒（4.37 7.17 7.82 → 4.42 7.13 7.81） | 5.0秒（4.42 7.13 7.81 → 5.10 7.23 7.84） |
| 前（2巡目） | 12.2秒（6.42 7.35 7.86 → 6.06 7.24 7.81） | 16.2秒（6.06 7.24 7.81 → 9.18 7.87 8.03） |
| 後（2巡目） | 6.4秒（18.00 10.34 8.92 → 16.88 10.23 8.89） | 7.1秒（16.88 10.23 8.89 → 16.34 10.33 8.94） |

後の2巡目はloadが前より高い（16〜18）のに前の半分ほどで、load の揺れを超えた差がある。

test binary（`cargo test --locked --no-run --message-format=json`が出す`executable`。libとbinのunit test、`dagq`のbinを含む）:

- 前: 48本、合計643,033,104 bytes（613.2 MiB）
- 後: 6本、合計90,432,632 bytes（86.2 MiB）

testの実行（build済み）:

- 前: `cargo test --locked`に`--test <ファイル>`を43本（e2eとplugin以外）並べて267.0秒（load 8.74 7.75 7.62 → 4.72 8.84 8.46）、386 passed・1 ignored
- 後: `cargo test --locked --test it`で222.5秒（load 7.80 8.55 8.41 → 9.64 10.02 9.11）、386 passed・1 ignored

testが前後で同じこと: 前の`cargo test --locked -- --list`の「binary名: test名」（e2eとplugin以外）を`<binary>::<test名>`に読み替えた集合387件と、後の`--test it`の「module path::test名」387件が一致した。`e2e`（9件）・`plugin`（11件）・unit test（340件）は名前ごと一致した。

## Alternatives

- **ファイルごとのbinaryのまま`tests/common`をcrateにする**: compileは1回になるが、linkはbinaryごとに残る。計測で大きいのはlinkとbinaryの数なので採らない。
- **e2eとpluginもまとめる**: `--test e2e`と`--test plugin`は文書・taskのverify・CIで名前で呼ばれていて、e2eはcmuxが要る。変える得が小さく、呼び出しを全部直す必要があるので採らない。
- **行数の制約を外す**: 1つのbinaryになってもファイルを分ける理由（別々のtaskの追記の衝突）は変わらないので外さない。

## Consequences

- workerの関係するtestの絞り方は`cargo test --locked --test it <ファイル名>::`になる（AGENTS.mdとdagq skillの`reference/scope.md`）。登録済みのtaskの本文にある`--test <ファイル名>`は、workerがこの形に読み替える。
- `tests/`を触る並行のrunは移動と衝突する。rebaseはgitのrename検出で移動先に載ることが多いが、載らなければそのrunがresumeで直す。
- 1つのbinaryの中の変更（どのファイルのtestを変えても）で`it`全体がcompileし直しになる。`tests/common`や1ファイルのcompileは前も各binaryで起きていたので、合計では減る。
- `tests/it`のどれか1ファイルのcompile errorで、integration test全体が流せなくなる（前はそのファイルのbinaryだけだった）。
- runtimeの中で`--test <名前>`を手がかりにする処理は精度が落ちる: statsのworker作業の内訳（`src/domain/worktime.rs`）は`--test it`を絞った実行として数え、関連taskの手がかり（`src/domain/related.rs`）は多くのtaskで`it`を拾う。直すかは別のtaskで決める。
- `Cargo.toml`の`include`は`tests/`を含まないので、`cargo package`は明示した`[[test]]`について「ignoring test ... not included」の警告を出す（publishは通る）。
