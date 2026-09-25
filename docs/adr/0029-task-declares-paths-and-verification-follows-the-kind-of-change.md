---
id: adr-0029
type: adr
title: taskが変更してよいパス（add --paths）を宣言し、validatingとintegrateが宣言外の変更を拒否し、verification_commandsを変更の種類で軽くする
status: accepted
created: 2026-09-24
updated: 2026-09-24
accepted_on: 2026-09-24
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - operations
  - performance
related:
  - adr-0019
  - adr-0023
  - design-domain-model
  - design-supervisor-lifecycle
  - design-persistence
---

# ADR-0029: taskが変更してよいパス（add --paths）を宣言し、validatingとintegrateが宣言外の変更を拒否し、verification_commandsを変更の種類で軽くする

## Context

[ADR-0023](0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)の決定1で、同じcommitへの`verification_commands`は`integrate`のrebase後の1回だけになった。それでも2026-09-23のmaintainerの計測ではvalidateの中央値が222秒で、docsだけのtaskにもfmt / test / clippy / llvm-covの4本を登録していた。`integrate`は単一slotで直列なので、1本の検証が長いと後ろの着地がすべて待つ。

検証を変更の種類で軽くすれば着地は速くなるが、「docsだけのはず」で登録したtaskのworkerが実際には`src/`を変えていると、その変更はtestもclippyも通らずにmainへ入る。taskの宣言と実際の差分の食い違いをruntimeが止めることを条件に、検証を軽くしてよいとユーザーが判断した。

## Decision

1. **宣言**: `dagq add`に`--paths GLOB`（繰り返し可）を足し、`tasks.paths`（JSON配列、migration 0018、既定`[]`）に保存する。`show`と`list --full`に出し、workerのpromptに`Paths you may change ...`の行で渡す。省略時（空）は制限しない（今までどおり）。globはrepository rootからの相対パス全体に合わせ、`*`と`?`は1つのsegment（`/`を越えない）の中、segment全体が`**`なら0個以上のsegmentに合う。それ以外は字義どおり。`*.md`はrootのMarkdownだけ、`docs/**`は`docs/`の下すべて、`**/*.md`は任意の深さのMarkdownに合う。空、`/`始まり、`.` / `..` / 空のsegmentを持つglobは登録で拒否する。
2. **変更**: draft / readyのtaskの`--paths`は`set-paths TASK --paths GLOB...`で置き換え、`set-paths TASK --none`で制限を外す（`set-goal`と同じ流儀。変化があったときだけ`task_paths_changed`（`from`、`to`）を記録する）。claimの後は変えられず、runは開始時の宣言で検査される。
3. **validating**: receipt・commit・cleanの検査を通ったrunについて、run branchが今のmainから分かれた点（mainとreceiptのcommitのmerge-base。rebaseしていなければbase commit）からreceiptのcommitまでに変わったパス（resumeしたsessionがrebaseしても、他のtaskが着地したパスは数えない）（`git diff --name-only --no-renames`、renameは両側）がすべてどれかのglobに合うかを調べる。合わないパスがあれば`failed`ではなく`needs_session`にし、`validation_finished`に`scope_violation`（外れたパス）と`allowed_paths`、新しいイベント`scope_violation`（`paths`、`allowed`、`reason`）を記録する。`last_error`は`changed paths outside the task's --paths: <paths>`。要求evidenceの検査（ADR-0019の決定5）より先に行う。
4. **integrate**: rebaseの後、squashしてmainに載せる範囲（`main..rebased head`）について同じ検査をし、合わなければmainを動かさず、`verification_commands`も実行せずに`integration_deferred`（payloadに`scope_violation`と`allowed`）で`needs_session`にする。validatingの後にsessionが足したcommitやrebaseで変わった差分もここで止まる。
5. **resume**: supervisorは`scope_violation`イベント、または`scope_violation`を持つ`integration_deferred`で止まったrunを、宣言外のパスを`git merge-base HEAD <main>`の状態に戻すよう依頼する定型文でresumeする（rebaseやevidenceの依頼ではない）。どうしても必要なら`failed`のreceiptで必要なパスを書かせ、人がtaskの`--paths`を広げて再登録するか判断する。
6. **検証の軽さの運用**: 変更の種類ごとに`--verify`と`--paths`を組み合わせて登録する（AGENTS.mdの「テストの制約」とdagq skillのRegister the tasks）。
   - docsだけ: `--paths 'docs/**' --paths '*.md' --verify 'cargo fmt --all --check'`（fmtも不要なら検証なし）
   - pluginの文書: `--paths 'plugins/**' --paths 'docs/**' --paths '*.md' --verify 'cargo test --locked --test plugin'`（`tests/plugin.rs`がskillの大きさや参照を検査するので、testを残す）
   - `src/`や`tests/`を触る: `--paths`なし（または広い宣言）で4本（fmt / test / clippy / `cargo llvm-cov --locked --fail-under-lines 80`。llvm-covを含めるならtestは重ねない）と`--evidence e2e`

## Alternatives

- **変更の種類をruntimeが推定して検証を選ぶ**: 差分から検証を自動で決めると、taskの登録者の意図（何を検証したいか）が消え、推定の誤りが黙って検証を飛ばす。宣言と検証を人が一緒に決め、runtimeは食い違いだけを止める方が失敗が見える。
- **宣言外の変更を`failed`にする**: 作業の大半は正しいことが多く、捨てるとtask 64のような丸ごとの再実行になる。ADR-0019の決定5と同じく`needs_session`にしてresumeで直させる。
- **validatingだけで検査する**: validatingの後にresumeしたsessionが足したcommitや、rebaseの解消で入った変更を見逃す。着地する差分そのものを`integrate`で検査するのが最後の関門になる。
- **gitignore形式のglob（slashの無いpatternは任意の深さ）**: `*.md`がplugin配下のskillまで含み、docsのつもりの宣言でplugin文書（testが要る）を許してしまう。root起点の単純な規則の方が宣言の範囲を読み違えにくい。外部crateも増やさない。

## Consequences

- docsだけのtaskの着地はfmtの数秒になり、`integrate`のslotを占める時間が短くなる。宣言外の変更は着地せず、resumeで取り除かれる。
- `--paths`を付けないtaskの振る舞いは変わらない。宣言は任意なので、軽い検証で登録するtaskに`--paths`を付け忘れると守りが効かない。skillとAGENTS.mdの推奨の組み合わせで補う。
- schemaはv18になる。run_eventsのkindは`scope_violation`と`task_paths_changed`を足すだけ。
- taskが本当に宣言外のパスを必要とする場合は、そのrunを`failed`で終えて`--paths`を広げたtaskを登録し直すか、draft / readyのうちに`set-paths`で広げる。走行中のrunの宣言は変えられない。
