# AGENTS.md

cmux-taskq は cmux と Git worktree で依存関係付きの開発タスクを実行する Rust runtime。文書の分類は [docs/README.md](docs/README.md)。

## セッション開始時に読む

1. [docs/journal/README.md](docs/journal/README.md) の Open と、担当タスクのジャーナル。前のセッションの状態と次の一手はここにある
2. [docs/plans/current.md](docs/plans/current.md) の現在のステップと完了条件
3. 触る範囲の `docs/design/*.md`

## 作業中

- タスクを始めるときは `docs/journal/000-template.md` から次の連番でジャーナルを作り、README の Open に追加する
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

- 指示があったときだけコミット・push する
- メッセージは `feat:` / `fix:` / `docs:` / `test:` の接頭辞、本文は何をなぜ変えたか
