# AGENTS.md

cmux-taskq は cmux と Git worktree で依存関係付きの開発タスクを実行する Rust runtime。文書の分類は [docs/README.md](docs/README.md)。

## セッション開始時に読む

1. [docs/journal/README.md](docs/journal/README.md) の Open と、担当タスクのジャーナル。前のセッションの状態と次の一手はここにある
2. [docs/plans/current.md](docs/plans/current.md) の現在のステップと完了条件
3. 触る範囲の `docs/design/*.md`

## 作業中

- タスクを始めるときは README の Open にある `planned` のジャーナルを `open` にする。なければ `docs/journal/000-template.md` から次の連番で作り、Open に追加する
- ジャーナルの Log に追記する。試して駄目だったこと、一時的な path・workspace 番号・制限の復活時刻、次にやろうとしていたことを書く。中断されても次のセッションが Log だけで再開できる状態を保つ
- Git worktree と cmux workspace のスモークは使い捨て repository で行い、この repository の queue DB や実行中の runtime バイナリを作業成果で置き換えない

## 変更後に必ず通す

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

## 文書のルール

- 決定は `docs/adr/` に追加する。既存 ADR は書き換えない
- 実装を変えたら `docs/design/` の該当文書と `updated` / `last_verified` を更新する
- ステップの状態が変わったら `docs/plans/current.md` を更新する
- frontmatter は [docs/frontmatter.md](docs/frontmatter.md) に従う

## タスクを閉じるとき

- ジャーナルに Result と Promoted を書き、`status: done` にして Open から外す
- Log にある事実のうち普遍的なものを design / ADR へ昇格させる

## コミット

- worker は自分の branch にコミットする。main への直接コミットと push は SV だけが行う
- メッセージは `feat:` / `fix:` / `docs:` / `test:` の接頭辞、本文は何をなぜ変えたか

## 役割: SV と worker

タスクは cmux workspace ごとに起動した対話モードの Claude Code session（worker）が実行し、1つの Claude Code session（SV）が起動・監視・merge を行う。

### SV

- ジャーナルの Open 一覧から依存が満たされたタスクを選び、同時に最大4件まで起動する
- タスクごとに main から `git worktree add .worktrees/<NNN>-<slug> -b journal/<NNN>-<slug>` で worktree を作る。`.worktrees/` は gitignore 済み
- `cmux workspace create --name "<NNN> <slug>" --cwd <worktree> --command "claude --model opus"` で workspace を作り、worker は常に Opus で起動する。`cmux send` / `cmux send-key` で指示を送る。指示にはジャーナルのパス、完了マーカー、SV への質問方法を含める
- 数分おきに `cmux read-screen --workspace <ws> --lines N` で画面を読む。権限確認は worktree 内の編集・cargo・git など安全なものは SV が応答し、それ以外とユーザーの判断が要るものはユーザーに確認する
- worker の完了マーカーを確認したら、worktree で fmt / test / clippy を通し、差分をレビューして main へ merge（fast-forward 優先）し、push する。ジャーナルの README Open 一覧は SV が main で更新する
- merge 後に workspace を閉じ、worktree と branch を削除する。失敗・中断時は両方を残す
- Claude 利用制限などで worker が止まったら、ジャーナルの Log を確認して別 session で引き継ぐ

### worker

- 起動時の指示にあるジャーナルを `open` にし、Log に追記しながら進める。Result と Promoted を書いて `done` にしてから完了を報告する
- worktree の外、main、`docs/journal/README.md` の Open 一覧、他タスクのジャーナルは触らない。push しない
- 判断が要るときは terminal に質問を書いて待つ。SV が同じ terminal に返答する
- 完了時は最終メッセージを指示された完了マーカーで終える
