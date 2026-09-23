# AGENTS.md

dagq は cmux と Git worktree で依存関係付きの開発タスクを実行する Rust runtime。文書の分類は [docs/README.md](docs/README.md)。この repository 自身の開発タスクも dagq で流す（ドッグフーディング）。

CLI の使い方（登録・起動・監視・レビューと着地・復旧）は plugin の skill が持つ。この文書はこの repository でだけ必要な注意を書く。

## セッション開始時に読む

1. `dagq list` と、担当タスクの `dagq show ID`。タスクの一覧・状態・依存・run 履歴はキューだけが持つ
2. [docs/plans/current.md](docs/plans/current.md) の現在のステップと完了条件
3. 触る範囲の `docs/design/*.md`

## 作業中

- タスクは `dagq add` で登録し、`ready` にしてから supervisor に流す。経過と次の一手はキュー（`show ID` の run 履歴と receipt）が持つ
- 本番 queue（この repository の queue DB）の登録・参照・操作は、supervisor・maintainer・計画中の session のどれでも必ず固定バイナリ `~/.local/bin/dagq` で行う。`target/debug` や `target/release` のバイナリは queue を開いただけで schema を黙って migrate し、古い schema のまま走っている固定バイナリの supervisor と実行中の run を `unsupported queue schema version` で壊すので、本番 queue には使わない。session 開始時に `which dagq` が `~/.local/bin/dagq` に解決することを確認する
- 新しいビルドの動作確認と、Git worktree・cmux workspace のスモークは使い捨て repository の queue で行う。この repository の queue DB や実行中の runtime バイナリ（`~/.local/bin/dagq`）を作業成果で勝手に置き換えない
- 固定バイナリの更新は `~/.local/bin/dagq` を入れ替えてから `up` を叩けばよい。runtime が version の違う supervisor を drain して（走行中の run の完了を待って）入れ替える（[ADR-0014](docs/adr/0014-up-replaces-a-supervisor-of-another-binary-version.md)）。入替そのものはユーザーに報告してから行う。ただし version は `CARGO_PKG_VERSION` なので、`Cargo.toml` の version を上げずに build し直したバイナリは同じ version を名乗り、`up` は入れ替えずに reuse する。リリースをまたがない差し替えでは version を上げるか `down --wait` で明示的に止めてから入れ替える

## 変更後に必ず通す

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

## テストの制約

- unit test: 行カバレッジの合計を 80% 以上に保つ（`cargo-llvm-cov`、行基準、全体）。下回る変更は着地しない。門番は task の `verification_commands`（supervisor の validating が worker の receipt を信用せず再実行する）と CI で、worker が手元で `cargo llvm-cov` を回す必要はない。runtime（`src/`）を触る task を `dagq add` するときは verification に `cargo llvm-cov --locked --fail-under-lines 80` を含める
- e2e test: ハッピーパスを `tests/e2e.rs` に置く。実バイナリ・実 Git・実 cmux を使い、Claude の代わりに受け入れ条件どおり commit と receipt を書く stub スクリプトを provider にする。cmux が必要なので `#[ignore]` とし、runtime（`src/`）を変えた run では worker が worktree で `cargo test --locked --test e2e -- --ignored` を実行し、receipt の `e2e` に evidence を書く。maintainer は自分では再実行しない
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

- run session は自分の run branch `dagq/<run-id>` にコミットする。main への着地は `dagq integrate` だけが行い（1 タスク 1 squash commit）、push は maintainer だけが行う
- メッセージは `feat:` / `fix:` / `docs:` / `test:` の接頭辞、本文は何をなぜ変えたか。着地時の commit メッセージはタスクの title と receipt の summary から runtime が作る

## 役割: supervisor と maintainer と worker

runtime の `supervise` プロセスが **supervisor**、登録・監視・レビュー・着地を行う常駐の Claude Code session が **maintainer**、run ごとに worktree で作業する Claude session が **worker**（[ADR-0010](docs/adr/0010-maintainer-and-resident-supervisor.md)、[docs/design/overview.md](docs/design/overview.md) の用語）。

同じ commit に対する verification は supervisor の validating の結果が正で、`integrate` は rebase が head を動かしたときだけ `verification_commands` を再実行する（rebase が no-op なら再検証しない）。

### maintainer

cold start は repository の中で1行。当面は in-cmux mode で運用する（cmux の socket password を設定していないので launchd mode は preflight で止まる。[ADR-0011](docs/adr/0011-cmux-socket-password-and-in-cmux-fallback.md)）。

```sh
dagq up --in-cmux --claude ~/.local/bin/claude --plugin-dir <この repository>/plugins/claude-dagq
```

- `--claude` を明示するのは、cmux の terminal の PATH では session ごとの shim（`$TMPDIR/cmux-cli-shims/<surface id>/claude`）が先に解決され、`up` がそれを supervisor の `--claude` に固定してしまうため。`up` は path を実体（`~/.local/share/claude/versions/<version>`）に解決して固定するので、Claude Code を更新したら `down --wait` → 同じ `up` で解決し直す
- in-cmux mode に自動再起動はない。supervisor が止まったら `dagq dagq supervisor` workspace の画面を読んで閉じ、同じ `up` を打ち直す（`down --wait` は drain の後に workspace を閉じるところまで行う）
- maintainer workspace は `dagq dagq maintainer`。maintainer session の中から `up` を打つと maintainer は `skipped`、生きている supervisor は `reused` になる
- 操作は plugin の `dagq` / `dagq-maintain` / `dagq-recover` skill に従う。CLI の外で状態を持たず、DB は手で直さない。`cmux read-screen` は当面の一次情報として認める
- バイナリは「作業中」のとおり固定した `~/.local/bin/dagq` だけを使う（`~/.local/bin` が PATH にあるので supervisor の起動でも同じものが動く）。キューは cwd から解決されるので、コマンドは repository の中（どの worktree でもよい）で実行する
- runtime（`src/`）を変えた run は、`integrate` の前に receipt の `e2e` の evidence と run_dir の log を確認する。e2e は自分では再実行せず、evidence が無い・不十分なときだけ worker の session に差し戻す
- push は maintainer だけが行う。着手と着地はユーザーに報告するが承認は待たない。ユーザーの判断が要るとき（受け入れ条件の変更、固定バイナリの更新、DB に触らずに解消できない詰まり）だけ報告して待つ
- 権限確認は worktree 内の編集・cargo・git など安全なものは maintainer が応答し、それ以外とユーザーの判断が要るものはユーザーに確認する

### worker

- runtime の prompt に従う。割り当てられた worktree（branch `dagq/<run-id>`）の中だけで作業し、main、queue DB、`runs/` 配下の runtime ファイル、他の run の worktree は触らない。merge も push も workspace の close もしない
- 変更後は「変更後に必ず通す」の 3 本（fmt / test / clippy）とタスクの verify コマンドを worktree で実行する。e2e と subagent review は該当するときに実行し、しないときは理由を receipt に書く。runtime（`src/`）を変えた run では e2e（`cargo test --locked --test e2e -- --ignored`）は必須で、結果を receipt の `e2e` に evidence として書く
- コミットしてから receipt を書く。receipt の commit は run branch の clean head で、base commit の上に乗っている
- 判断が要るときは terminal に質問を書いて待つ。maintainer が `read-screen` で拾い、同じ terminal に返答する
- receipt を書いたら結果を短く報告して止まる。`/exit` は自分で打たない。supervisor が idle を見て送る
