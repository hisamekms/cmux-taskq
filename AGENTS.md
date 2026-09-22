# AGENTS.md

cmux-taskq は cmux と Git worktree で依存関係付きの開発タスクを実行する Rust runtime。文書の分類は [docs/README.md](docs/README.md)。この repository 自身の開発タスクも cmux-taskq で流す（ドッグフーディング）。

## セッション開始時に読む

1. `cmux-taskq list` と、担当タスクの `cmux-taskq show ID`。タスクの一覧・状態・依存・run 履歴はキューだけが持つ
2. タスクにジャーナル（`docs/journal/`、frontmatter の `queue_task` が一致するもの）があればその Log。前のセッションの状態と次の一手はここにある
3. [docs/plans/current.md](docs/plans/current.md) の現在のステップと完了条件
4. 触る範囲の `docs/design/*.md`

## 作業中

- タスクは `cmux-taskq add` で登録し、`ready` にしてから supervise に流す。ジャーナルは人が関わったセッションの経過と判断を残す場所で、エージェントが完走するタスクには作らなくてよい。作るときは `docs/journal/000-template.md` から次の連番で作り、登録後に返った ID を `queue_task` に書く
- ジャーナルのある作業では Log に追記する。試して駄目だったこと、一時的な path・workspace 番号・run ID・制限の復活時刻、次にやろうとしていたことを書く。中断されても次のセッションが Log だけで再開できる状態を保つ
- Git worktree と cmux workspace のスモークは使い捨て repository で行い、この repository の queue DB や実行中の runtime バイナリ（`~/.local/bin/cmux-taskq`）を作業成果で置き換えない。バイナリの更新は SV がユーザーに報告してから行う

## 変更後に必ず通す

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo llvm-cov --locked --fail-under-lines 80
```

## テストの制約

- unit test: 行カバレッジの合計を 80% 以上に保つ（`cargo-llvm-cov`、行基準、全体）。下回る変更は着地しない
- e2e test: ハッピーパスを `tests/e2e.rs` に置く。実バイナリ・実 Git・実 cmux を使い、Claude の代わりに受け入れ条件どおり commit と receipt を書く stub スクリプトを provider にする。cmux が必要なので `#[ignore]` とし、runtime を変えた run では SV が `integrate` の前に run の worktree で `cargo test --locked --test e2e -- --ignored` を実行する
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

- run session は自分の run branch `taskq/<run-id>` にコミットする。main への着地は `cmux-taskq integrate` だけが行い（1 タスク 1 squash commit）、push は SV だけが行う
- メッセージは `feat:` / `fix:` / `docs:` / `test:` の接頭辞、本文は何をなぜ変えたか。着地時の commit メッセージはタスクの title と receipt の summary から runtime が作る

## 役割: SV と run session

タスクは `cmux-taskq supervise` が run ごとに cmux workspace と Git worktree を作って起動した対話モードの Claude Code session（run session）が実行し、常駐の Claude Code session（SV）が登録・監視・レビュー・着地を行う。SV の操作は 1 操作 1 CLI コマンドで書く。後で runtime へ移す（[docs/plans/current.md](docs/plans/current.md) の After first dogfooding）ためで、SV は CLI の外で状態を持たない。`cmux read-screen` は当面の一次情報として認める。

### SV

準備。バイナリは `~/.local/bin/cmux-taskq` に固定したものを使う（`~/.local/bin` が PATH にあるので、supervise の workspace でも同じものが動く）。コマンドは repository の中（どの worktree でもよい）で実行し、キューは cwd から解決される。

| 操作 | コマンド |
| --- | --- |
| キューを作る（初回だけ） | `cmux-taskq init` |
| キューと runs ディレクトリの場所を見る | `cmux-taskq locate` |

登録。タスクの title、description、受け入れ条件、検証コマンド、依存を CLI に渡す。ジャーナルがあれば Goal を `--description`、完了条件を `--acceptance`、frontmatter の `verify` を `--verify` に写し、返った ID を `queue_task` に書く。

| 操作 | コマンド |
| --- | --- |
| 登録する | `cmux-taskq add "<title>" --description "<text>" --acceptance "<text>" --verify "<command>" --depends-on <ID>` |
| 実行可能にする | `cmux-taskq ready <ID>` |
| 依存を直す | `cmux-taskq dependency add <ID> <PREDECESSOR>` / `cmux-taskq dependency remove <ID> <PREDECESSOR>` |
| 取り下げる | `cmux-taskq cancel <ID>` |

起動。supervise は専用の cmux workspace で 1 つだけ常駐させる。同時実行は 4。

| 操作 | コマンド |
| --- | --- |
| supervise を起動する | `cmux workspace create --name TASKQ-SUPERVISOR --cwd <repository root> --command "cmux-taskq supervise --parallel 4"` |
| supervise が生きているか見る | `cmux-taskq status` |

監視。数分おきにキューと run の画面を見る。run の workspace 名は `taskq <task-id> <run-id>`、workspace ID は `show` に出る。権限確認は worktree 内の編集・cargo・git など安全なものは SV が応答し、それ以外とユーザーの判断が要るものはユーザーに確認する。run session が terminal に書いた質問にも同じ経路で答える。

| 操作 | コマンド |
| --- | --- |
| 一覧と状態を見る | `cmux-taskq list` |
| run の状態・workspace ID・receipt と検証ログのパス・イベントを見る | `cmux-taskq show <ID>` |
| run の画面を読む | `cmux read-screen --workspace <workspace_id> --lines <N>` |
| run に返答する | `cmux send --workspace <workspace_id> "<text>"` のあと `cmux send-key --workspace <workspace_id> enter` |

レビューと着地。run が `awaiting_integration` になったら差分と証跡を見て、問題なければ着地させる。着地は runtime が rebase・再検証・squash まで行うので、SV は merge も cherry-pick もしない。

| 操作 | コマンド |
| --- | --- |
| receipt と verify ログを見る | `cmux-taskq show <ID>`（`<runs_dir>/<run-id>/receipt.json`、`verify-N.log`） |
| 差分を見る | `git log main..taskq/<run-id>` と `git diff main...taskq/<run-id>` |
| runtime を変えた run の e2e を通す | `cargo test --locked --test e2e -- --ignored`（`<runs_dir>/<run-id>/worktree` で） |
| 着地する | `cmux-taskq integrate <ID>` |
| push する | `git push origin main` |

`needs_session`。`integrate` が rebase の衝突や再検証の失敗で run を止めたら、run の worktree で session を開き直して解消させる。着地は worktree を消すので、`integrate` の前にその session を必ず終える。

| 操作 | コマンド |
| --- | --- |
| session を開き直す | `cmux workspace create --name "taskq resume <run-id>" --cwd <runs_dir>/<run-id>/worktree --command "claude --resume <run-id>"` |
| 解消を指示する | `cmux send --workspace <workspace_id> "integrate が needs_session で止めた。last_error の main commit へ git rebase し、衝突を解消し、タスクの verify コマンドと AGENTS.md の変更後に必ず通すコマンドを再実行し、新しい head commit で receipt.json を書き直せ。変更が不要になったなら result を failed にして summary に理由を書け。"` のあと `cmux send-key --workspace <workspace_id> enter` |
| 完了を確認する | `cmux read-screen --workspace <workspace_id> --lines <N>` と `cmux-taskq show <ID>` |
| session を終える | `cmux send --workspace <workspace_id> "/exit"` のあと `cmux send-key --workspace <workspace_id> enter` |
| 着地し直す | `cmux-taskq integrate <ID>` |

失敗と中断。DB は手で直さない。失敗・中断した run の workspace は runtime が閉じないので、確認が済んだら SV が閉じる。

| 操作 | コマンド |
| --- | --- |
| 止まった run と復旧の障害を見る | `cmux-taskq doctor` |
| 死んだ run を interrupted にする | `cmux-taskq recover <run-id>` |
| 再試行する | `cmux-taskq ready <ID>`（直してから流すなら先に `cmux-taskq draft <ID>`） |
| 残った workspace を閉じる | `cmux workspace close <workspace_id>` |

報告。着手と着地はユーザーに報告するが承認は待たない。ユーザーの判断が要るとき（受け入れ条件の変更、バイナリの更新、DB に触らずに解消できない詰まり）だけ報告して待つ。

### run session

- runtime の prompt に従う。割り当てられた worktree（branch `taskq/<run-id>`）の中だけで作業し、main、queue DB、`runs/` 配下の runtime ファイル、他の run の worktree は触らない。merge も push も workspace の close もしない
- 変更後は「変更後に必ず通す」のコマンドとタスクの verify コマンドを worktree で実行する。e2e と subagent review は該当するときに実行し、しないときは理由を receipt に書く
- コミットしてから receipt を書く。receipt の commit は run branch の clean head で、base commit の上に乗っている
- 判断が要るときは terminal に質問を書いて待つ。SV が `read-screen` で拾い、同じ terminal に返答する
- タスクにジャーナルがある（prompt にパスがある）ときだけそのジャーナルを更新する。Log に追記し、閉じるなら Result と Promoted を書いて `done` にする。他のタスクのジャーナルは触らない
- receipt を書いたら結果を短く報告して止まる。`/exit` は自分で打たない。supervise が idle を見て送る
