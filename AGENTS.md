# AGENTS.md

dagq は cmux と Git worktree で依存関係付きの開発タスクを実行する Rust runtime。文書の分類は [docs/README.md](docs/README.md)。この repository 自身の開発タスクも dagq で流す（ドッグフーディング）。

CLI の使い方（登録・起動・監視・レビューと着地・復旧）は plugin の skill が持つ。この文書はこの repository でだけ必要な注意を書く。

## セッション開始時に読む

1. `dagq list` と、担当タスクの `dagq show ID`。タスクの一覧・状態・依存・run 履歴はキューだけが持つ
2. [docs/plans/current.md](docs/plans/current.md) の現在のステップと完了条件
3. 触る範囲の `docs/design/*.md`

## 作業中

- タスクは `dagq add` で登録し、`ready` にしてから supervisor に流す。経過と次の一手はキュー（`show ID` の run 履歴と receipt）が持つ
- 本番 queue（この repository の queue DB）の登録・参照・操作は、supervisor・inbox・planner のどの session でも必ず固定バイナリ `~/.local/bin/dagq` で行う。`target/debug` や `target/release` のバイナリは queue を開いただけで schema を黙って migrate し、古い schema のまま走っている固定バイナリの supervisor と実行中の run を `unsupported queue schema version` で壊すので、本番 queue には使わない。session 開始時に `which dagq` が `~/.local/bin/dagq` に解決することを確認する
- 新しいビルドの動作確認と、Git worktree・cmux workspace のスモークは使い捨て repository の queue で行う。この repository の queue DB や実行中の runtime バイナリ（`~/.local/bin/dagq`）を作業成果で勝手に置き換えない
- 固定バイナリの更新は `~/.local/bin/dagq` を入れ替えてから `up` を叩けばよい。runtime が version の違う supervisor を drain して（走行中の run の完了を待って）入れ替える（[ADR-0014](docs/adr/0014-up-replaces-a-supervisor-of-another-binary-version.md)）。入替そのものはユーザーに報告してから行う。ただし version は `CARGO_PKG_VERSION` なので、`Cargo.toml` の version を上げずに build し直したバイナリは同じ version を名乗り、`up` は入れ替えずに reuse する。リリースをまたがない差し替えでは version を上げるか `down --wait` で明示的に止めてから入れ替える
- repository root の `dagq.toml` の `[run.env]` が run の worker workspace と検証コマンドに env として渡る（[ADR-0023](docs/adr/0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md) 決定 3、書式は [supervisor-lifecycle](docs/design/supervisor-lifecycle.md) の Run environment）。この repository では target を共有せず、`dagq.toml` も置かない（2026-09-23 のユーザー決定）。理由: (a) cargo の lock は build だけを直列化し、その後の test 実行は分離されないので、`CARGO_BIN_EXE_dagq` を exec する test（`tests/cli.rs`・`runtime.rs`・`location.rs`・`plugin.rs`・`e2e.rs`）が、並行する別の run の build が上書きした `target/debug/dagq` を実行しうる。(b) 同時の `cargo llvm-cov` が共有の `llvm-cov-target` の profraw を消し合い・混ぜ合い、coverage の関門が誤る。build の共有は sccache など安全な方法を別途検討する。runtime が読むのは run の worktree ではなく main checkout の作業ファイルの `dagq.toml` なので、worktree で変えた `dagq.toml` は着地して main checkout に反映されてから効く

## 変更後に必ず通す

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

## テストの制約

- unit test: 行カバレッジの合計を 80% 以上に保つ（`cargo-llvm-cov`、行基準、全体）。下回る変更は着地しない。門番は task の `verification_commands`（`integrate` が rebase 後に worker の receipt を信用せず 1 回だけ実行する。validating では実行しない）と CI で、worker が手元で `cargo llvm-cov` を回す必要はない。runtime（`src/`）を触る task を `dagq add` するときは verification に `cargo llvm-cov --locked --fail-under-lines 80` を含め、`--evidence e2e` も付ける（receipt の `e2e` が evidence 付きの `passed` でない run は validating で `needs_session`（`evidence_missing`）になり、supervisor の resume が不足分を補わせる。ADR-0019 決定 5）。llvm-cov を verification に含める task では `cargo test --locked` を verification に重ねない。`cargo llvm-cov` は `cargo test` と同じ test binary 群（`src/lib.rs` の unit test と `tests/*.rs`。この crate に doctest は無い）を全部実行し、1 件でも落ちれば失敗するので、両方を並べても integrate の直列の検証で同じ test が 2 回走る（約 100 秒）だけで検出力は増えない。llvm-cov を含めない task（docs・plugin の文書など）は、必要なら `cargo test --locked` を verification に残す。worker が手元で回す「変更後に必ず通す」の 3 本はこれと別で、変えない
- 変更の種類で verification を軽くしてよい。条件は `--paths` で変えてよいパスを宣言すること（[ADR-0029](docs/adr/0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)）。宣言外のパスを変えた run は validating で `needs_session`（`scope_violation`）になり、`integrate` も rebase 後の差分を同じく検査して着地させないので、軽い検証のまま `src/` の変更が入ることはない。`--paths` を付けない task は今までどおり制限されない。推奨の組み合わせ（glob は repository root 起点で、`*` は 1 階層、`**` は任意の深さ。詳細は dagq skill の `reference/scope.md`）:
  - docs だけ: `--paths 'docs/**' --paths '*.md' --verify 'cargo fmt --all --check'`（fmt も要らなければ検証なし）
  - plugin の文書・skill: `--paths 'plugins/**' --paths 'docs/**' --paths '*.md' --verify 'cargo test --locked --test plugin'`（`tests/plugin.rs` が skill の大きさと参照を検査するので test を残す。plugin の文書を読む test はこれだけ）
  - runtime（`src/`・`tests/`・`migrations/`）: `--paths` なしで fmt / clippy / `cargo llvm-cov --locked --fail-under-lines 80` と `--evidence e2e`（上の llvm-cov の規則どおり `cargo test --locked` は重ねない）
  - 種類が混ざる task は重い方の検証にする。task が宣言外のパスを本当に必要とするなら、worker は `failed` の receipt に必要なパスを書き、planner が `--paths` を広げて登録し直す（draft / ready のうちは `set-paths TASK --paths ...` / `--none` で変えられる）
- e2e test: ハッピーパスを `tests/e2e.rs` に置く。実バイナリ・実 Git・実 cmux を使い、Claude の代わりに受け入れ条件どおり commit と receipt を書く stub スクリプトを provider にする。cmux が必要なので `#[ignore]` とし、runtime（`src/`）を変えた run では worker が worktree で `cargo test --locked --test e2e -- --ignored` を実行し、receipt の `e2e` に evidence を書く。inbox も planner も自分では再実行しない
- 実 Claude を含む経路は自動化せず、手動スモーク（journal 010, 012）で確認する

## 文書のルール

- 人の判断は ADR・Goal の記述・`Task.context`・receipt の `summary` に残す（`docs/journal/` は凍結済みで、新しいジャーナルは作らない）
- 決定は `docs/adr/` に追加する。既存 ADR は書き換えない
- 実装を変えたら `docs/design/` の該当文書と `updated` / `last_verified` を更新する
- ステップの状態が変わったら `docs/plans/current.md` を更新する
- frontmatter は [docs/frontmatter.md](docs/frontmatter.md) に従う

## タスクを閉じるとき

- タスクの完了はキューが持つ。`integrate` が run を `integrated`、タスクを `completed` にする

## コミット

- run session は自分の run branch `dagq/<run-id>` にコミットする。main への着地は `dagq integrate` だけが行い（1 タスク 1 squash commit）、push は integrate が行う（`push_failed` の attention が inbox に出たら、人の指示で原因を直して `git push origin main`）
- メッセージは `feat:` / `fix:` / `docs:` / `test:` の接頭辞、本文は何をなぜ変えたか。着地時の commit メッセージはタスクの title と receipt の summary から runtime が作る

## 役割: supervisor と worker と planner と inbox と observer

役割はこの 5 つ（ADR-0024 の決定 1、[docs/design/overview.md](docs/design/overview.md) の用語集）。runtime の `supervise` プロセスが **supervisor**（claim・worker の起動・validating・run ごとの headless の review / triage の job・resume・着地・後始末）、run ごとに worktree で作業する Claude session が **worker**、人と対話して goal / task を登録する常駐 session が **planner**、人に届くもの（ask と attention）の窓口になる常駐 session が **inbox**、supervisor が timer で起動する headless の job が **observer**。以前の常駐 session（ADR-0010〜0023 に出てくる英字の役割名）は ADR-0024 で退役し、既存 ADR のその記述は overview の用語集で読み替える。

同じ commit に対する verification は `integrate` の 1 回が正で、validating は receipt・commit・clean・要求 evidence だけを見て `verification_commands` を実行しない。`integrate` は rebase の有無に関わらず rebase 後に必ず `verification_commands` を実行し（試行ごとの `integrate-<attempt>-verify-N.log`）、失敗すれば run は `needs_session` になって supervisor が resume する（[ADR-0023](docs/adr/0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md) 決定 1）。

### 起動と停止（`up` / `down`）

cold start は repository の中で1行。`up` は supervisor・inbox・planner を開く（ADR-0024 の決定 6）。当面は in-cmux mode で運用する（cmux の socket password を設定していないので launchd mode は preflight で止まる。[ADR-0011](docs/adr/0011-cmux-socket-password-and-in-cmux-fallback.md)）。`up` / `down` / 固定バイナリの更新は、人が inbox か planner の session から打つ（手順は plugin の `dagq-recover` skill の section 5）。

```sh
dagq up --in-cmux --claude ~/.local/bin/claude --plugin-dir <この repository>/plugins/claude-dagq
```

- `--claude` を明示するのは、cmux の terminal の PATH では session ごとの shim（`$TMPDIR/cmux-cli-shims/<surface id>/claude`）が先に解決され、`up` がそれを supervisor の `--claude` に固定してしまうため。`up` は path を実体（`~/.local/share/claude/versions/<version>`）に解決して固定するので、Claude Code を更新したら `down --wait` → 同じ `up` で解決し直す
- in-cmux mode に自動再起動はない。supervisor が止まると inbox の `watch` が `restart supervisor`（`supervisor_stopped`）として人に知らせる。`[dagq]supervisor` workspace の画面を読んで閉じ、同じ `up` を打ち直す（`down --wait` は drain の後に workspace を閉じるところまで行う）
- `up` が開くのは `[dagq]inbox` と `[dagq]planner`（と in-cmux mode の `[dagq]supervisor`）。inbox / planner の session の中から `up` を打てばそれ自身が `skipped` で、もう片方は開くか `reused`、生きている supervisor は `reused` になる。`down` は inbox と planner を閉じない
- workspace の title は表示専用で、runtime は title で workspace を探さない（[ADR-0026](docs/adr/0026-identify-workspaces-by-uuid-env-and-queue-group.md)）。`up` は inbox・planner と in-cmux supervisor の workspace UUID を queue DB（`session_workspaces`）に記録し、その UUID が `cmux workspace list` に居るかで reuse を判定する（title を rename しても判定は変わらない。閉じられていれば作り直す）。`DAGQ_ROLE` / `DAGQ_QUEUE` は workspace の `--env` にあり（`cmux workspace env <id> --json` で読める）、その workspace で `claude` を打ち直しても引き継がれる。queue の workspace は `[dagq]` の workspace group（external ID は queue hash）にまとまる
- task 100 の入ったバイナリに入れ替えるときは、supervisor を旧バイナリで `down --wait` → バイナリ入れ替え → inbox か planner の session から `up --in-cmux` の順にする。新バイナリの `up` は退役した常駐 session の workspace を開かず、`session_workspaces` のその行を消すだけなので、残った旧 workspace は人が閉じる。新バイナリは queue を開いただけで schema を上げることがあるので、旧 supervisor が動いているうちに新バイナリで本番 queue を開かない（`target/` のバイナリで開かないのも同じ理由）
- workspace 名は [ADR-0028](docs/adr/0028-workspace-titles-are-repo-and-role.md) で `[<repo>]supervisor` / `[<repo>]inbox` / `[<repo>]planner` / `[<repo>]worker#<task-id> - <task title>`。識別は UUID なので、旧名の workspace は改名しなくても reuse・`down` の対象のまま
- バイナリは「作業中」のとおり固定した `~/.local/bin/dagq` だけを使う（`~/.local/bin` が PATH にあるので supervisor の起動でも同じものが動く）。キューは cwd から解決されるので、コマンドは repository の中（どの worktree でもよい）で実行する。CLI の外で状態を持たず、DB は手で直さない（例外は無い。repository を移動したときの束縛の付け替えも `rebind` で行う。[ADR-0020](docs/adr/0020-rebind-queue-to-a-moved-repository.md)）

### 着地と人の判断

- 着地は supervisor が行う。receipt を受理した run は session を開いたまま headless の review job にかけ、pass なら着地（integrate が push まで行う）、revise なら生きている session に差し戻し、concern なら `approve_landing` の ask（`land` / `send_back` / `cancel`）にしてその answer を適用する（[ADR-0027](docs/adr/0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)）。failed / interrupted の run は triage job が retry / resume / ask を決める。3 回 resume しても解消しない `needs_session` と、worker が止まったダイアログ（`prompt_waiting`）は inbox 宛ての ask になる
- 人が手で行うのは、headless の review が失敗した run（`review by hand`）の review と `integrate`、`push_failed`（next: push main）の原因の修正と `git push origin main`、triage が失敗した run の判断、supervisor が居ないときの `recover`、`stuck_exit` / `answer_prompt` の answer に従う run の workspace へのキー送信。どれも inbox が人に知らせ、人の指示で `dagq-recover` skill に従って行う。runtime（`src/`）を変えた run を手で着地させるときは、`integrate` の前に receipt の `e2e` の evidence と run_dir の log を確認する（e2e は自分では再実行しない）
- 着手と着地は人に報告する。人の判断が要るとき（受け入れ条件の変更、固定バイナリの更新、DB に触らずに解消できない詰まり）は ask にして待つ

### worker

- runtime の prompt に従う。割り当てられた worktree（branch `dagq/<run-id>`）の中だけで作業し、main、queue DB、`runs/` 配下の runtime ファイル、他の run の worktree は触らない。merge も push も workspace の close もしない
- 変更後は「変更後に必ず通す」の 3 本（fmt / test / clippy）とタスクの verify コマンドを worktree で実行する。e2e と subagent review は該当するときに実行し、しないときは理由を receipt に書く。runtime（`src/`）を変えた run では e2e（`cargo test --locked --test e2e -- --ignored`）は必須で、結果を receipt の `e2e` に evidence として書く
- コミットしてから receipt を書く。receipt の commit は run branch の clean head で、base commit の上に乗っている
- 判断が要るときは terminal に質問を書いて待つのではなく、`dagq ask --run <run-id> --kind worker_question --question '...'` を打ち、短く報告して止まる。回答は supervisor が `answer to ask <id>: ...` として同じ terminal に送る（ADR-0022 決定 2）
- receipt を書く前に、自分が起動した background の処理（`run_in_background` の shell、待ちループ、watch など）をすべて止める。残っていると supervisor の `/exit` が Claude Code の「Background work is running」の確認画面で止まり、`exit_request_timed_out` になる
- receipt を書いたら結果を短く報告して止まる。`/exit` は自分で打たない。supervisor が idle を見て送る

### inbox と planner

- `up` が開き、初期 prompt（`inbox_prompt` / `planner_prompt`）で起動する。workspace の `--env` に `DAGQ_ROLE=inbox` / `planner` と `DAGQ_QUEUE` を持ち、compaction と `/clear` の後は plugin の SessionStart hook が `status --role <role>` を出す
- inbox は `dagq-inbox` skill に従い、`status --role inbox` から始め、`watch --role inbox` を background で回し、`ask_opened` の question と options を人に見せ、人の答えを `answer` で書く。attention はすべて inbox 宛てで、回答済みの ask、止まった supervisor、失敗した review / triage、push の失敗も人に知らせ、人の指示があるときだけ `dagq-recover` skill の手順を実行する。自分では判断しない
- planner は `dagq-planner` skill に従い、人の課題を聞き、dagq skill で goal と task を登録して ready にし、follow_ups の draft task を人と決め、goal の全 task の完了を見たら receipt と acceptance を照合して `goal close` する。人に頼まれれば `up` / `down` も打つ

### observer

- supervisor が `--observe-interval`（既定 3600 秒、0 で無効）ごとと 1 日 1 回（`--observe-daily`、既定 on）、`dagq observe` を子プロセスで起動する。cmux workspace は持たず、`claude -p` を `DAGQ_ROLE=observer` で動かす。supervisor が居ないときは動かない。手で走らせるなら `dagq observe`（`--dry-run` で prompt だけ見る）
- 入力は `stats --since <cursor>`、直近 20 件の note、open な ask、graph の candidates と critical。書けるのは note、`kind: blocked` の ask、draft の goal だけで、run / task / goal の状態を変えるコマンドは CLI が拒否する。個々の詰まりは解消しない
- 経過は `observe_started` / `observe_finished`（書いた件数、cursor）と `<queue dir>/observer/<started_at>/`（prompt、入力、出力）に残る。note と draft goal は planner（`dagq` skill の `reference/observer.md`）が人と見て、draft goal を `goal ready` するか `goal close --verdict abandoned` にする。`blocked` の ask は inbox が人に見せる
- 詳細は [supervisor-lifecycle](docs/design/supervisor-lifecycle.md) の Observer
