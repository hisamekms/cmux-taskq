# AGENTS.md

dagq は cmux と Git worktree で依存関係付きの開発タスクを実行する Rust runtime。文書の分類は [docs/README.md](docs/README.md)。この repository 自身の開発タスクも dagq で流す（ドッグフーディング）。

CLI の使い方（登録・起動・監視・レビューと着地・復旧）は plugin の skill が持つ。この文書はこの repository でだけ必要な注意を書く。

## セッション開始時に読む

1. `dagq list` と、担当タスクの `dagq show ID`。タスクの一覧・状態・依存・run 履歴はキューだけが持つ
2. [docs/plans/current.md](docs/plans/current.md) の現在のステップと完了条件
3. 触る範囲の `docs/design/*.md`

## 作業中

- タスクは planner が `dagq add` で登録し、`dagq submit` で proposal として plan review に出す。`ready` にするのは plan review job だけで（人が明示したときの `ready --bypass-review` と triage の retry を除く）、ready になった task を supervisor が claim する。経過と次の一手はキュー（`show ID` の run 履歴と receipt）が持つ
- 本番 queue（この repository の queue DB）の登録・参照・操作は、supervisor・inbox・planner（人が開いたものも runtime が立てたものも）のどの session でも必ず固定バイナリ `~/.local/bin/dagq` で行う。`target/debug` や `target/release` のバイナリは queue を開いただけで schema を黙って migrate し、古い schema のまま走っている固定バイナリの supervisor と実行中の run を `unsupported queue schema version` で壊すので、本番 queue には使わない。session 開始時に `which dagq` が `~/.local/bin/dagq` に解決することを確認する
- 新しいビルドの動作確認と、Git worktree・cmux workspace のスモークは使い捨て repository の queue で行う。この repository の queue DB や実行中の runtime バイナリ（`~/.local/bin/dagq`）を作業成果で勝手に置き換えない
- 固定バイナリの更新は `~/.local/bin/dagq` を入れ替えてから `up` を叩けばよい。runtime が version の違う supervisor を drain して（走行中の run の完了を待って）入れ替える（[ADR-0014](docs/adr/0014-up-replaces-a-supervisor-of-another-binary-version.md)）。入替そのものはユーザーに報告してから行う。ただし version は `CARGO_PKG_VERSION` なので、`Cargo.toml` の version を上げずに build し直したバイナリは同じ version を名乗り、`up` は入れ替えずに reuse する。リリースをまたがない差し替えでは version を上げるか `down --wait` で明示的に止めてから入れ替える。[ADR-0045](docs/adr/0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)（ADR-0014 を置き換え）が build 識別子での判定・待たない引き継ぎ・明示の migrate・`dagq install`・`up --auto-update` を決めたが、その実装が入った固定バイナリへの最初の入れ替え（旧バイナリで `down --wait` → 置き換え → 新バイナリで `dagq migrate` → `up`）までは、この項と上の「本番 queue には固定バイナリだけを使う」規則のとおりに運用する
- repository root の `dagq.toml` の `[run.env]` が run の worker workspace・`integrate` の検証コマンド・review の headless 実行に env として渡る（[ADR-0049](docs/adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md) 決定 3、書式は [supervisor-lifecycle の Run environment](docs/design/supervisor-lifecycle/run-environment.md)）。この repository では `dagq.toml` の `[run.env]` で `RUSTC_WRAPPER = "sccache"` と `SCCACHE_IGNORE_SERVER_IO_ERROR = "1"` を渡し、依存 crate の compile の結果を run 間で共有する（ADR-0049 決定 6。server と話せないときは rustc を直接実行し、cache が効かないだけで build は失敗しない）。一方、run ごとの `CARGO_TARGET_DIR`（target）は共有しない（2026-09-23 のユーザー決定を ADR-0049 が引き継ぐ）。理由: (a) cargo の lock は build だけを直列化し、その後の test 実行は分離されないので、`CARGO_BIN_EXE_dagq` を exec する test（`tests/cli.rs`・`runtime_*.rs`・`location.rs`・`plugin.rs`・`e2e.rs`）が、並行する別の run の build が上書きした `target/debug/dagq` を実行しうる。(b) 同時の `cargo llvm-cov` が共有の `llvm-cov-target` の profraw を消し合い・混ぜ合い、coverage の関門が誤る。`CARGO_TARGET_DIR` と `CARGO_INCREMENTAL` は `[run.env]` に置かない（run ごとの `CARGO_TARGET_DIR` は sccache の鍵に入って依存 crate も当たらなくなり、incremental を切っても run 間の hit は増えず worker の繰り返しの build が遅くなるだけ。決定 6）。sccache は人が `mise use -g sccache` で入れ、`ln -s ~/.local/share/mise/shims/sccache ~/.local/bin/sccache` で mise の shim への link を置く（`up` の時点で固定される supervisor の PATH でも、sccache の更新後に解決できるようにするため。決定 7）。worker は host にツールを入れない。見つからなければ `up` は supervisor を起動せず、supervisor は claim と着地を止め、`integrate` は検証コマンドを実行せずに止まり、inbox に `install tool`（`run_env_program_missing`）が出る（決定 8・9。`dagq doctor` の `run_env` で解決先を見られる）。また `[run.env]` の `CARGO_BUILD_JOBS = "4"` と `RUST_TEST_THREADS = "4"` で run ごとの cargo の並列度を絞る（2026-09-26 に人が planner と決めた。task 427）。理由: host は 8 コア / 16GB で、並列 4 の運用で load average が最大 151〜204 に達し、cmux の capture の timeout が 400 件を超え、run の startup の中央値が約 1100 秒になった（goal 36 の note 8718 の基準値）。値の決め方: supervisor の `--parallel` を 3 に下げ、worker 3 本と `integrate` 1 本が同時に cargo を回しても合計 16 並列（コア数 8 の 2 倍）程度に収まるようにする。`--parallel` や host を変えたら合わせて見直す。target を共有しない上の理由 (a)(b) は、env で並列度を絞ることには当たらない。needs_session の resume が開く workspace には `[run.env]` が渡らないので、そこでは絞られない。CI と dagq を通さない `cargo` は `dagq.toml` を読まないので影響を受けない。runtime が読むのは run の worktree ではなく main checkout の作業ファイルの `dagq.toml` なので、worktree で変えた `dagq.toml` は着地して main checkout に反映されてから効く。main checkout の `dagq.toml` は全 run の build と `integrate` に効くので、壊すと queue 全体が止まる

## 変更後に必ず通す

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

worker が手元で流すのはこの 3 本と、task の verify のうち `cargo llvm-cov` 以外（fmt・clippy・`cargo test --locked --test plugin` など）。`cargo llvm-cov`（coverage の関門）は `integrate` が rebase 後に 1 回だけ流すので、worker は流さない（`integrate` の検証が落ちて resume された run では、落ちたコマンドを手元で流して再現してよい）。runtime（`src/`）を変えた run の e2e は今までどおり worker が流す（下の「テストの制約」と「worker」）。worker の prompt は verification_commands を integrate が流すものとして見せ、手元の検証はこの文書の指示に従わせる（task 510）

## テストの制約

- unit test: 行カバレッジの合計を 80% 以上に保つ（`cargo-llvm-cov`、行基準、全体）。下回る変更は着地しない。門番は task の `verification_commands`（`integrate` が rebase 後に worker の receipt を信用せず 1 回だけ実行する。validating では実行しない）と CI で、worker は手元で `cargo llvm-cov` を回さない（task の verify に含まれていても、worker が流すのは「変更後に必ず通す」の 3 本と llvm-cov 以外の verify。例外は `integrate` の検証が落ちて resume された run で、落ちたコマンドを再現してよい）。runtime（`src/`）を触る task を `dagq add` するときは verification に `cargo llvm-cov --locked --fail-under-lines 80` を含め、`--evidence e2e` も付ける（receipt の `e2e` が evidence 付きの `passed` でない run は validating で `needs_session`（`evidence_missing`）になり、supervisor の resume が不足分を補わせる。ADR-0019 決定 5）。llvm-cov を verification に含める task では `cargo test --locked` を verification に重ねない。`cargo llvm-cov` は `cargo test` と同じ test binary 群（`src/lib.rs` の unit test と `tests/*.rs`。この crate に doctest は無い）を全部実行し、1 件でも落ちれば失敗するので、両方を並べても integrate の直列の検証で同じ test が 2 回走る（約 100 秒）だけで検出力は増えない。llvm-cov を含めない task（docs・plugin の文書など）は、必要なら `cargo test --locked` を verification に残す。worker が手元で回す「変更後に必ず通す」の 3 本はこれと別で、変えない
- 変更の種類で verification を軽くしてよい。条件は `--paths` で変えてよいパスを宣言すること（[ADR-0029](docs/adr/0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)）。宣言外のパスを変えた run は validating で `needs_session`（`scope_violation`）になり、`integrate` も rebase 後の差分を同じく検査して着地させないので、軽い検証のまま `src/` の変更が入ることはない。`--paths` を付けない task は今までどおり制限されない。推奨の組み合わせ（glob は repository root 起点で、`*` は 1 階層、`**` は任意の深さ。詳細は dagq skill の `reference/scope.md`）:
  - docs だけ: `--paths 'docs/**' --paths '*.md' --verify 'cargo fmt --all --check'`（fmt も要らなければ検証なし）
  - ADR を書く（docs だけ）: 上に `--verify 'sh scripts/check-adr-numbers.sh'` を足す（「文書のルール」の ADR 番号の割り当て）
  - plugin の文書・skill: `--paths 'plugins/**' --paths 'docs/**' --paths '*.md' --verify 'cargo test --locked --test plugin'`（`tests/plugin.rs` が skill の大きさと参照を検査するので test を残す。plugin の文書を読む test はこれだけ）
  - runtime（`src/`・`tests/`・`migrations/`）: `--paths` なしで fmt / clippy / `cargo llvm-cov --locked --fail-under-lines 80` と `--evidence e2e`（上の llvm-cov の規則どおり `cargo test --locked` は重ねない）
  - 種類が混ざる task は重い方の検証にする。task が宣言外のパスを本当に必要とするなら、worker は `failed` の receipt に必要なパスを書き、planner が `--paths` を広げて登録し直す（draft / ready のうちは `set-paths TASK --paths ...` / `--none` で変えられる）
- e2e test: ハッピーパスを `tests/e2e.rs` に置く。実バイナリ・実 Git・実 cmux を使い、Claude の代わりに受け入れ条件どおり commit と receipt を書く stub スクリプトを provider にする。cmux が必要なので `#[ignore]` とし、runtime（`src/`）を変えた run では worker が worktree で `cargo test --locked --test e2e -- --ignored` を実行し、receipt の `e2e` に evidence を書く。inbox も planner も自分では再実行しない
- test の待ちには上限を付ける。poll の loop は deadline を持ち、上限の無い待ち（thread の join、stub の session の終了、`dagq` の子プロセスの `output()`）は `tests/common/mod.rs` の `within`（fixture が持つ test 全体の `common::test()` と、1 つの待ちの `STEP_LIMIT`）で包む。上限を過ぎると test binary が test の名前と待っていた条件を stderr に出して exit 101 で失敗し、`cargo test | tail` が戻らなくなることはない（task 324）
- 実 Claude を含む経路は自動化せず、手動スモーク（[docs/design/manual-smoke.md](docs/design/manual-smoke.md)）で確認する

## 文書のルール

- 人の判断は ADR・Goal の記述・`Task.context`・receipt の `summary` に残す（作業記録のジャーナルは [ADR-0036](docs/adr/0036-delete-frozen-work-records.md) で削除した）
- 決定は `docs/adr/` に追加する。既存 ADR は書き換えない
- ADR は `accepted` だけが現在の決定で、`superseded` なら `superseded_by` を辿り、`deprecated` は後継なしの廃止（日付は `superseded_on` ではなく `deprecated_on`）。決定を変えるときは古い ADR を丸ごと置き換える統合 ADR を書く（[ADR-0042](docs/adr/0042-adr-is-superseded-whole-and-deprecation-date-is-deprecated-on.md)、索引は [docs/adr/README.md](docs/adr/README.md)）
- 実装を変えたら `docs/design/` の該当文書と `updated` / `last_verified` を更新する
- ステップの状態が変わったら `docs/plans/current.md` を更新する
- frontmatter は [docs/frontmatter.md](docs/frontmatter.md) に従う
- ADR の番号は task の登録時に planner が仮に書き、plan review が検査する。並行する run が同じ番号を取ると、slug が違うので git では衝突せずに両方着地し、後続 task の番号参照もずれる（2026-09-24 に ADR-0035 が重なり、task 214 で 0036 に付け替えた）
  - ADR を書く task を登録するときは、planner が main の `docs/adr/` の次の空きを選び、description に「ADR-NNNN（docs/adr/NNNN-slug.md）を書く」と書き、`--verify 'sh scripts/check-adr-numbers.sh'` を付ける。他の未完了の task や proposal の割り当ての棚卸しは planner が抱えず、plan review がその番号が main・未完了の task・他の submitted の proposal の割り当てと重ならないかを検査し、重なれば revise で planner に振り直させる
  - 後続 task は ADR を番号と path で参照する
  - worker は割り当てられた番号を使う。main でその番号がすでに埋まっていれば、自分で振り直さず `dagq ask` にする
  - `scripts/check-adr-numbers.sh` は `docs/adr/` の番号の重複と、frontmatter の `id` が `adr-<ファイルの番号>` と食い違う ADR を検出して exit 1 にする（CI も実行する）。重複で `integrate` が落ちた run は resume され、worker が番号を振り直し、振り直した番号を receipt の summary に書く

## タスクを閉じるとき

- タスクの完了はキューが持つ。`integrate` が run を `integrated`、タスクを `completed` にする

## コミット

- run session は自分の run branch `dagq/<run-id>` にコミットする。main への着地は `dagq integrate` だけが行い（1 タスク 1 squash commit）、push は integrate が行う（`push_failed` の attention が inbox に出たら、人の指示で原因を直して `git push origin main`）
- メッセージは `feat:` / `fix:` / `docs:` / `test:` の接頭辞、本文は何をなぜ変えたか。着地時の commit メッセージはタスクの title と receipt の summary から runtime が作る

## 役割: supervisor と worker と planner と inbox と observer

役割はこの 5 つ（[ADR-0044](docs/adr/0044-findings-proposals-from-findings-and-quiet-observer.md) の決定 1、[docs/design/overview.md](docs/design/overview.md) の用語集）。runtime の `supervise` プロセスが **supervisor**（claim・worker の起動・validating・run ごとの headless の review / triage の job・submitted の proposal ごとの headless の plan review job・resume・着地・runtime が立てる planner の起動・後始末）、run ごとに worktree で作業する Claude session が **worker**、proposal（goal と task の束）ごとのオンデマンドの session が **planner**（人が `dagq plan` で開くものと、supervisor が立てるものがある）、人に届くもの（ask と attention）の窓口になる唯一の常駐 session が **inbox**、supervisor が timer で起動する headless の job が **observer**。常駐の planner と goal 22 の follow-up triage job（ADR-0037）は ADR-0041 で廃止した（ADR-0044 が引き継ぐ）。以前の常駐 session（ADR-0010〜0023 に出てくる英字の役割名）は ADR-0024 で退役し（ADR-0044 が引き継ぐ）、既存 ADR のその記述は overview の用語集で読み替える。

同じ commit に対する verification は `integrate` の 1 回が正で、validating は receipt・commit・clean・要求 evidence だけを見て `verification_commands` を実行しない。`integrate` は rebase の有無に関わらず rebase 後に必ず `verification_commands` を実行し（試行ごとの `integrate-<attempt>-verify-N.log`）、失敗すれば run は `needs_session` になって supervisor が resume する（[ADR-0049](docs/adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md) 決定 1）。

### 起動と停止（`up` / `down`）

cold start は repository の中で1行。`up` は supervisor と inbox だけを開き、planner は開かない（ADR-0044 の決定 6）。planner は計画ごとに人が `dagq plan --plugin-dir <この repository>/plugins/claude-dagq` で開く（打つたびに新しい `[dagq]planner#<id>` を開き、複数同時に開ける。`dagq planners` で一覧）。当面は in-cmux mode で運用する（cmux の socket password を設定していないので launchd mode は preflight で止まる。[ADR-0011](docs/adr/0011-cmux-socket-password-and-in-cmux-fallback.md)）。`up` / `down` / `plan` / 固定バイナリの更新は、人が inbox か planner の session から打つ（手順は plugin の `dagq-recover` skill の section 5）。

```sh
dagq up --in-cmux --claude ~/.local/bin/claude --plugin-dir <この repository>/plugins/claude-dagq
```

- `--claude` を明示するのは、cmux の terminal の PATH では session ごとの shim（`$TMPDIR/cmux-cli-shims/<surface id>/claude`）が先に解決され、`up` がそれを supervisor の `--claude` に固定してしまうため。`up` は path を実体（`~/.local/share/claude/versions/<version>`）に解決して固定するので、Claude Code を更新したら `down --wait` → 同じ `up` で解決し直す
- in-cmux mode に自動再起動はない。supervisor が止まると inbox の `watch` が `restart supervisor`（`supervisor_stopped`）として人に知らせる。`[dagq]supervisor` workspace の画面を読んで閉じ、同じ `up` を打ち直す（`down --wait` は drain の後に workspace を閉じるところまで行う）
- `up` が開くのは `[dagq]inbox`（と in-cmux mode の `[dagq]supervisor`）。inbox の session の中から `up` を打てば inbox は `skipped`、それ以外からなら開くか `reused`、生きている supervisor は `reused` になる。`down` は inbox も planner も閉じない。旧バイナリの `up` が開いた常駐の `[dagq]planner` は、新しい `up` が `session_workspaces` の行を忘れるだけなので、人が unpin して閉じる
- workspace の title は表示専用で、runtime は title で workspace を探さない（[ADR-0026](docs/adr/0026-identify-workspaces-by-uuid-env-and-queue-group.md)）。`up` は inbox と in-cmux supervisor の workspace UUID を queue DB（`session_workspaces`）に記録し、その UUID が `cmux workspace list` に居るかで reuse を判定する（title を rename しても判定は変わらない。閉じられていれば作り直す）。`DAGQ_ROLE` / `DAGQ_QUEUE` は workspace の `--env` にあり（`cmux workspace env <id> --json` で読める）、その workspace で `claude` を打ち直しても引き継がれる。queue の workspace は `[dagq]` の workspace group（external ID は queue hash）にまとまる
- task 100 の入ったバイナリに入れ替えるときは、supervisor を旧バイナリで `down --wait` → バイナリ入れ替え → inbox か planner の session から `up --in-cmux` の順にする。新バイナリの `up` は退役した常駐 session の workspace を開かず、`session_workspaces` のその行を消すだけなので、残った旧 workspace は人が閉じる。新バイナリは queue を開いただけで schema を上げることがあるので、旧 supervisor が動いているうちに新バイナリで本番 queue を開かない（`target/` のバイナリで開かないのも同じ理由）
- workspace 名は [ADR-0028](docs/adr/0028-workspace-titles-are-repo-and-role.md) で `[<repo>]supervisor` / `[<repo>]inbox` / `[<repo>]worker#<task-id> - <task title>`、planner は ADR-0044 の決定 6 で `[<repo>]planner#<id>`。識別は UUID なので、旧名の workspace は改名しなくても reuse・`down` の対象のまま
- バイナリは「作業中」のとおり固定した `~/.local/bin/dagq` だけを使う（`~/.local/bin` が PATH にあるので supervisor の起動でも同じものが動く）。キューは cwd から解決されるので、コマンドは repository の中（どの worktree でもよい）で実行する。CLI の外で状態を持たず、DB は手で直さない（例外は無い。repository を移動したときの束縛の付け替えも `rebind` で行う。[ADR-0020](docs/adr/0020-rebind-queue-to-a-moved-repository.md)）

### 着地と人の判断

- 着地は supervisor が行う。receipt を受理した run は session を開いたまま headless の review job にかけ、pass なら着地（integrate が push まで行う）、revise なら生きている session に差し戻し、concern なら `approve_landing` の ask（`land` / `send_back` / `cancel`）にしてその answer を適用する（[ADR-0027](docs/adr/0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)）。failed / interrupted の run は triage job が retry / resume / ask を決める。3 回 resume しても解消しない `needs_session` と、worker が止まったダイアログ（`prompt_waiting`）は inbox 宛ての ask になる
- 人が手で行うのは、headless の review が失敗した run（`review by hand`）の review と `integrate`、`push_failed`（next: push main）の原因の修正と `git push origin main`、triage が失敗した run の判断、plan review が失敗した proposal（`plan review by hand`）の判断（`ready --bypass-review` か、planner からの `submit --proposal ID` での出し直しか、cancel）、supervisor が居ないときの `recover`、`stuck_exit` / `answer_prompt` の answer に従う run の workspace へのキー送信。どれも inbox が人に知らせ、人の指示で `dagq-recover` skill に従って行う。runtime（`src/`）を変えた run を手で着地させるときは、`integrate` の前に receipt の `e2e` の evidence と run_dir の log を確認する（e2e は自分では再実行しない）
- 着手と着地は人に報告する。人の判断が要るとき（受け入れ条件の変更、固定バイナリの更新、DB に触らずに解消できない詰まり）は ask にして待つ

### worker

- runtime の prompt に従う。割り当てられた worktree（branch `dagq/<run-id>`）の中だけで作業し、main、queue DB、`runs/` 配下の runtime ファイル、他の run の worktree は触らない。merge も push も workspace の close もしない
- 変更後は「変更後に必ず通す」の 3 本（fmt / test / clippy）と、タスクの verify コマンドのうち `cargo llvm-cov` 以外を worktree で実行する。`cargo llvm-cov` は `integrate` が rebase 後に 1 回だけ流すので手元では流さない（`integrate` の検証が落ちて resume された run では、落ちたコマンドを手元で流して再現してよい）。e2e と subagent review は該当するときに実行し、しないときは理由を receipt に書く。runtime（`src/`）を変えた run では e2e（`cargo test --locked --test e2e -- --ignored`）は必須で、結果を receipt の `e2e` に evidence として書く
- コミットしてから receipt を書く。receipt の commit は run branch の clean head で、base commit の上に乗っている
- 判断が要るときは terminal に質問を書いて待つのではなく、`dagq ask --run <run-id> --kind worker_question --question '...'` を打ち、短く報告して止まる。回答は supervisor が `answer to ask <id>: ...` として同じ terminal に送る（ADR-0022 決定 2）
- receipt を書く前に、自分が起動した background の処理（`run_in_background` の shell、待ちループ、watch など）をすべて止める。残っていると supervisor の `/exit` が Claude Code の「Background work is running」の確認画面で止まり、`exit_request_timed_out` になる
- receipt を書いたら結果を短く報告して止まる。`/exit` は自分で打たない。supervisor が idle を見て送る

### inbox と planner

- inbox は `up` が開き、初期 prompt（`inbox_prompt`）で起動する唯一の常駐 session。planner は人が `dagq plan` で開くもの（`planner_prompt`）と、supervisor が立てるもの（plan review が差し戻した proposal の planner が閉じていたとき、plan review が submitted に戻した ready の task を直させるとき、runtime や job が作った draft ごと）がある。どれも workspace の `--env` に `DAGQ_ROLE=inbox` / `planner` と `DAGQ_QUEUE` を持ち、compaction と `/clear` の後は plugin の SessionStart hook が `status --role <role>` を出す
- inbox は `dagq-inbox` skill に従い、`status --role inbox` から始め、`watch --role inbox` を background で回し、`ask_opened` の question と options を人に見せ、人の答えを `answer` で書く。attention はすべて inbox 宛てで、回答済みの ask、止まった supervisor、失敗した review / triage / plan review（`plan_review_failed`）、応答しない planner（`planner_unresponsive`）、runtime の planner が決めきれなかった draft、push の失敗も人に知らせ、人の指示があるときだけ `dagq-recover` skill の手順を実行する。plan review の concern（`approve_plan`：`ready` / `send_back` / `cancel`）と runtime の planner の `planner_question` の answer は supervisor が適用・配送する。自分では判断しない
- planner は `dagq-planner` skill に従い、人の課題を聞き、dagq skill で goal と draft の task を書き、`lint` を通して `submit` し、plan review の revise を受けたら直して `submit --proposal ID` で出し直す。自分では `ready` にしない。計画の意図が変わる修正は、人が開いた planner ならその workspace で人に聞き、runtime が立てた planner なら `planner_question` の ask にする。交通整理（draft の棚卸し、重複・実装済みの検出、ADR 番号の衝突、依存の付け替え、他の task の退避）は plan review job に任せて抱えない。goal の全 task の完了を見たら receipt と acceptance を照合して `goal close` する。人に頼まれれば `up` / `down` も打つ
- 人に届くものはすべて inbox 宛てで、planner に返るのはその planner 自身の proposal への revise と、その planner が作った ask の answer だけ

### observer

- supervisor が `--observe-interval`（既定 3600 秒、0 で無効）ごとと 1 日 1 回（`--observe-daily`、既定 on）、`dagq observe` を子プロセスで起動する。cmux workspace は持たず、`claude -p` を `DAGQ_ROLE=observer` で動かす。supervisor が居ないときは動かない。手で走らせるなら `dagq observe`（`--dry-run` で prompt だけ見る）
- 入力は `stats --since <cursor>`、直近 20 件の note、open な ask、graph の candidates と critical。書けるのは note、`kind: blocked` の ask、draft の goal だけで、run / task / goal の状態を変えるコマンドは CLI が拒否する。個々の詰まりは解消しない
- 経過は `observe_started` / `observe_finished`（書いた件数、cursor）と `<queue dir>/observer/<started_at>/`（prompt、入力、出力）に残る。note と draft goal は planner（`dagq` skill の `reference/observer.md`）が人と見て、採るなら planner が draft goal を `submit --goal ID` で plan review に出し、採らないなら `goal close --verdict abandoned` にする。`blocked` の ask は inbox が人に見せる
- 詳細は [supervisor-lifecycle の Observer](docs/design/supervisor-lifecycle/observer.md)
