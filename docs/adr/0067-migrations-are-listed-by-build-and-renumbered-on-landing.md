---
id: adr-0067
type: adr
title: migrationの一覧をbuild.rsがmigrations/から作り、番号の欠けと重なりをbuildと検査で止め、着地で重なった番号を機械的に振り直す
status: accepted
created: 2026-09-26
updated: 2026-09-26
accepted_on: 2026-09-26
owners:
  - hisamekms
tags:
  - runtime
  - persistence
  - operations
related:
  - adr-0003
  - adr-0029
  - adr-0034
  - adr-0045
  - adr-0049
  - design-persistence
  - design-supervisor-lifecycle
---

# ADR-0067: migrationの一覧をbuild.rsがmigrations/から作り、番号の欠けと重なりをbuildと検査で止め、着地で重なった番号を機械的に振り直す

## Context

queueのmigrationは`migrations/NNNN_<name>.sql`で、番号がschema version（`PRAGMA user_version`）になる。バイナリが知るmigrationの一覧は`src/infrastructure/schema.rs`の`MIGRATIONS`で、migrationを足すtaskはここに`include_str!`の行を1行ずつ手で足していた。`BINARY_SCHEMA`（`MIGRATIONS`の長さ）と`floor_for`、各migrationの先頭の宣言行（[ADR-0045](0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)の決定5〜7）はこの一覧の上に立つ。

2026-09-26の時点で、この形は並行するrunの着地を詰まらせていた（goal 45）。

1. **`schema.rs`は必ず衝突する。** migrationを足すtaskはどれも`MIGRATIONS`の末尾に行を足すので、並行する2つのrunは同じ行でrebaseが衝突する。`stats`の`conflict_hotspots`では`src/infrastructure/schema.rs`が7件中7件だった。衝突した着地は延期され、resumeで直される。
2. **番号も重なる。** 並行するrunはmainの次の空き番号を同じように選ぶ。ファイル名のslugが違えばgitは衝突せず、`schema.rs`の衝突を直した後に同じ番号のファイルが2つ残る。task 292は0029を0030へ振り直すのにresumeを1回使った。
3. **testが最新の番号を数字で書いていた。** `tests/queue.rs`の`assert_eq!(SqliteQueue::SCHEMA_VERSION, 32)`や`migrate --check`の`pending`の一覧のように、最新のschemaの番号を数字で持つtestは、migrationを足すたびに書き換わり、そこでも衝突した。

## Decision

1. **一覧は`build.rs`が`migrations/`から作る。** `build.rs`は`migrations/`のファイルのうち`.sql`を番号順に並べ、`$OUT_DIR/migrations.rs`に`include_str!`の配列を書く。`schema.rs`の`MIGRATIONS`はそれを`include!`する。migrationを足すtaskは`migrations/`にファイルを置くだけで、`schema.rs`を編集しない。`BINARY_SCHEMA`（一覧の長さ）、`floor_for`、宣言行の意味は変えない。
   - `build.rs`は`migrations/`を常に`rerun-if-changed`にする（これまではversionがpre-releaseのときだけだった）。ファイルを足す・消す・改名すると一覧が作り直される。
   - `.sql`のファイルが`NNNN_<name>.sql`（4桁の数字、`_`、空でない名前）でない、番号が2つ以上のファイルで重なる、番号が0001から欠けなく続いていない、のどれかなら、どのファイルかを書いてbuildを失敗させる。`.sql`でないファイルはmigrationではなく、一覧に入れない。
   - 名前と番号の規則は`src/migration_numbers.rs`の1か所に置き、`build.rs`が`#[path]`で読み込み、libraryもmoduleとしてcompileする（`src/build_id.rs`と同じ形）。規則はunit testで検査され、下の決定3もこれを使う。
2. **番号はbuildを待たずにも検査する。** `scripts/check-migration-numbers.sh`（`scripts/check-adr-numbers.sh`と同じ形）が、`migrations/*.sql`の名前、番号の重なり、0001からの欠けを検査し、問題があれば該当するファイルをstderrに書いてexit 1にする。CI（`.github/workflows/ci.yml`）はADR番号の検査の次にこれを実行する。migrationを足すtaskはverificationに含めてよい。
3. **着地で重なった番号を振り直す。** `integrate`はrebaseの後（rebase後のworktreeがcleanであることを確かめた後、scopeの検査とverificationの前）に、runが足したmigration（`main..rebased`で追加された`migrations/NNNN_<name>.sql`）の番号を、rebase後のtreeの`migrations/`でrunが足していない別のファイル（mainから来たもの）が使っているかを見る。mainの一覧ではなくrebase後のtreeで判定するので、runが既存のmigrationを改名したり、消して同じ番号で置き換えたりしても重なりとは見なさない。
   - **機械的に振り直す**: runが足したmigrationが1つだけで、その番号の4桁（例`0033`）を、runが変えた他のファイル（testやdocsなど。migrationのファイル自身は除く）のどれも含まないときは、runtimeがそのファイルを、rebase後のtreeでrunが足していないmigrationの最大の番号+1（次の空き番号）へ`git mv`し、run branchにcommitする。`migration_renumbered`（`main`、`from`、`to`、`old_number`、`new_number`、`head_before`、`head_after`）を記録してから、振り直したtreeでscopeの検査とverificationに進む。verificationは振り直した後のtreeで走るので、振り直しが壊したものがあればverificationが落ちて、従来どおり`needs_session`になる。
   - **振り直せないとき**: runが足したmigrationが2つ以上で、そのどれかの番号が埋まっているとき、または振り直すmigrationの番号をrunの他の変更が含むときは、何も書き換えずに`needs_session`にする。`integration_deferred`の`code`は新しい`migration_number_taken`で、payloadに`migrations`（runが足したmigration）、`taken`（番号が埋まっていたもの）、`next_number`（次の空き番号）、番号を含むファイルがあれば`referring`を置き、`reason`に理由と次の空き番号と直し方を書く。supervisorは他の着地の保留と同じくresumeし、sessionが番号を振り直して参照を直し、receiptを書き直す。
   - 対象は`migrations/`の直下で`NNNN_<name>.sql`の名前のファイルだけで、runが足したmigrationの番号がmainで使われていないときは何もしない（番号の欠けは振り直さない。決定1のbuildと決定2の検査が止める）。
4. **testは最新のschemaの番号を数字で書かない。** 最新のschemaは`SqliteQueue::SCHEMA_VERSION`（`BINARY_SCHEMA`）で書き、`migrate`の`applied`や`--check`の`pending`のような「ある版から最新まで」の一覧は`MIGRATIONS`と`is_compatible`から組み立てる。過去の特定のmigration（0024の下限の表、0026の互換など）を名指す数字は、migrationを足しても変わらないので残してよい。

## Alternatives

- **番号をやめて時刻やhashにする**: 番号の重なりは起きないが、`user_version`と一覧の位置の対応（ADR-0045の決定7の下限、`floor_for`）が崩れ、既存のqueueの`user_version`との対応を変えるmigrationの移行が要る。重なりの頻度は決定3で吸収できるので採らなかった。
- **一覧の行を1ファイル1行のテキストにする**: `schema.rs`の代わりに一覧のファイルを置いても、並行するrunが同じ末尾に行を足すので衝突は同じ所に移るだけである。ディレクトリそのものを一覧にすれば、足す操作が別々のファイルの作成になり衝突しない。
- **番号の重なりは常にsessionに直させる**: task 292のように、振り直しにresumeを1回使うことになる。migrationが1つで参照が無い場合がほとんどで、そこは機械的に直せる。参照があるとき（test名やdocsに番号が出るとき）は、機械的な置換が意図しない数字を変えうるので、そこだけsessionに任せた。
- **planがmigrationの番号を予約する**: ADRの番号と同じく登録時に割り当てる案。plan reviewの検査が増え、予約したtaskが取り下げられると欠けが生じる。着地の順で決まる番号を着地で振り直す方が単純である。

## Consequences

- migrationを足すtaskが`schema.rs`を編集しなくなり、`conflict_hotspots`から`schema.rs`の衝突が消える見込みである。番号が重なった着地の多くはresumeを使わずに着地する。
- `migration_renumbered`が新しいイベントのkindとして、`migration_number_taken`が新しい理由のcodeとして増える。
- 着地したmigrationの番号は、runが選んだ番号と違うことがある。runのreceiptや`Task.context`が番号を書いていても、着地したcommitとイベントが正になる。
- runの他の変更がたまたま同じ4桁の文字列（別のADRの番号など）を含むと、振り直さずに`needs_session`になる。安全側に倒した結果で、sessionが直せば着地する。
- `build.rs`は`migrations/`が読めないとbuildを失敗させる。crates.ioのpackageは`Cargo.toml`の`include`で`migrations/**`を含めているので影響しない。
