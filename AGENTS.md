# AGENTS.md

cmux-taskq は cmux と Git worktree で依存関係付きの開発タスクを実行する Rust runtime。文書の分類は [docs/README.md](docs/README.md)。この repository 自身の開発タスクも cmux-taskq で流す（ドッグフーディング）。

CLI の使い方（登録・起動・監視・レビューと着地・復旧）は plugin の skill が持つ。この文書はこの repository でだけ必要な注意を書く。

## セッション開始時に読む

1. `cmux-taskq list` と、担当タスクの `cmux-taskq show ID`。タスクの一覧・状態・依存・run 履歴はキューだけが持つ
2. タスクにジャーナル（`docs/journal/`、frontmatter の `queue_task` が一致するもの）があればその Log。前のセッションの状態と次の一手はここにある
3. [docs/plans/current.md](docs/plans/current.md) の現在のステップと完了条件
4. 触る範囲の `docs/design/*.md`

## 作業中

- タスクは `cmux-taskq add` で登録し、`ready` にしてから supervisor に流す。ジャーナルは人が関わったセッションの経過と判断を残す場所で、エージェントが完走するタスクには作らなくてよい。作るときは `docs/journal/000-template.md` から次の連番で作り、登録後に返った ID を `queue_task` に書く
- ジャーナルのある作業では Log に追記する。試して駄目だったこと、一時的な path・workspace 番号・run ID・制限の復活時刻、次にやろうとしていたことを書く。中断されても次のセッションが Log だけで再開できる状態を保つ
- Git worktree と cmux workspace のスモークは使い捨て repository で行い、この repository の queue DB や実行中の runtime バイナリ（`~/.local/bin/cmux-taskq`）を作業成果で置き換えない。バイナリの更新は maintainer がユーザーに報告してから行う

## 変更後に必ず通す

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo llvm-cov --locked --fail-under-lines 80
```

## テストの制約

- unit test: 行カバレッジの合計を 80% 以上に保つ（`cargo-llvm-cov`、行基準、全体）。下回る変更は着地しない
- e2e test: ハッピーパスを `tests/e2e.rs` に置く。実バイナリ・実 Git・実 cmux を使い、Claude の代わりに受け入れ条件どおり commit と receipt を書く stub スクリプトを provider にする。cmux が必要なので `#[ignore]` とし、runtime を変えた run では maintainer が `integrate` の前に run の worktree で `cargo test --locked --test e2e -- --ignored` を実行する
- 実 Claude を含む経路は自動化せず、手動スモーク（journal 010, 012）で確認する

## 文書のルール

- 決定は `docs/adr/` に追加する。既存 ADR は書き換えない
- 実装を変えたら `docs/design/` の該当文書と `updated` / `last_verified` を更新する
- ステップの状態が変わったら `docs/plans/current.md` を更新する
- frontmatter は [docs/frontmatter.md](docs/frontmatter.md) に従う

## タスクを閉じるとき

- タスクの完了はキューが持つ。`integrate` が run を `integrated`、タスクを `completed` にする
- ジャーナルがあれば Result と Promoted を書き、`status: done` にする
- Log にある事実のうち普遍的なものを design / ADR へ昇格させる

## コミット

- run session は自分の run branch `taskq/<run-id>` にコミットする。main への着地は `cmux-taskq integrate` だけが行い（1 タスク 1 squash commit）、push は maintainer だけが行う
- メッセージは `feat:` / `fix:` / `docs:` / `test:` の接頭辞、本文は何をなぜ変えたか。着地時の commit メッセージはタスクの title と receipt の summary から runtime が作る

## 役割: supervisor と maintainer と worker

runtime の `supervise` プロセスが **supervisor**、登録・監視・レビュー・着地を行う常駐の Claude Code session が **maintainer**、run ごとに worktree で作業する Claude session が **worker**（[ADR-0010](docs/adr/0010-maintainer-and-resident-supervisor.md)、[docs/design/overview.md](docs/design/overview.md) の用語）。

### maintainer

cold start は repository の中で1行。

```sh
cmux-taskq up --plugin-dir <この repository>/plugins/claude-taskq
```

- 操作は plugin の `taskq` / `taskq-maintain` / `taskq-recover` skill に従う。CLI の外で状態を持たず、DB は手で直さない。`cmux read-screen` は当面の一次情報として認める
- バイナリは `~/.local/bin/cmux-taskq` に固定したものを使う（`~/.local/bin` が PATH にあるので supervisor の起動でも同じものが動く）。キューは cwd から解決されるので、コマンドは repository の中（どの worktree でもよい）で実行する
- runtime（`src/`）を変えた run は、`integrate` の前に run の worktree で `cargo test --locked --test e2e -- --ignored` を通す
- push は maintainer だけが行う。着手と着地はユーザーに報告するが承認は待たない。ユーザーの判断が要るとき（受け入れ条件の変更、固定バイナリの更新、DB に触らずに解消できない詰まり）だけ報告して待つ
- 権限確認は worktree 内の編集・cargo・git など安全なものは maintainer が応答し、それ以外とユーザーの判断が要るものはユーザーに確認する

### worker

- runtime の prompt に従う。割り当てられた worktree（branch `taskq/<run-id>`）の中だけで作業し、main、queue DB、`runs/` 配下の runtime ファイル、他の run の worktree は触らない。merge も push も workspace の close もしない
- 変更後は「変更後に必ず通す」のコマンドとタスクの verify コマンドを worktree で実行する。e2e と subagent review は該当するときに実行し、しないときは理由を receipt に書く
- コミットしてから receipt を書く。receipt の commit は run branch の clean head で、base commit の上に乗っている
- 判断が要るときは terminal に質問を書いて待つ。maintainer が `read-screen` で拾い、同じ terminal に返答する
- タスクにジャーナルがある（prompt にパスがある）ときだけそのジャーナルを更新する。Log に追記し、閉じるなら Result と Promoted を書いて `done` にする。他のタスクのジャーナルは触らない
- receipt を書いたら結果を短く報告して止まる。`/exit` は自分で打たない。supervisor が idle を見て送る
