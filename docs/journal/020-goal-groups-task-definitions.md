---
id: journal-020
type: journal
title: Goal groups (ADR-0009): task definitions for the four serial tasks
status: draft
created: 2026-09-22
updated: 2026-09-22
plan_step: null
queue_task: null
depends_on_journal: [14, 19]
verify:
  - cargo fmt --all --check
  - cargo test --locked
  - cargo clippy --locked --all-targets -- -D warnings
  - cargo llvm-cov --locked --fail-under-lines 80
related:
  - plan-rust-runtime-mvp
  - adr-0009
  - design-domain-model
  - design-persistence
  - design-supervisor-lifecycle
  - design-plugin-integration
---

# 020: Goal groups (ADR-0009): task definitions for the four serial tasks

## Goal

[plans/current.md](../plans/current.md) の After first dogfooding 2項目目。[ADR-0009](../adr/0009-goal-groups-tasks.md) を4 taskに分けて `cmux-taskq add` に渡せる形で定義する。4件は直列（T2 は T1 に、T3 は T2 に、T4 は T3 に `--depends-on`）。この4件が最初のgoalの実例になるが、goalエンティティは T2 で入るので、4件とも goal なしで登録し、T2 の着地とバイナリ更新のあとに SV が goal を作って、まだ走っていない T3・T4 を `set-goal` で紐付ける（手順は「SV への依頼」）。

登録・実行・着地は SV が行う。登録後に返った T1〜T4 の ID をこの Log に書く。この journal 自体は task に紐付けず（`queue_task: null`）、4 task の定義と経過の記録として使う。

ADR との差分（定義時点の判断）:

- schema は現在 v6（`migrations/0006_merge_queue.sql`、`tests/queue.rs` が `schema_version() == 6` を確認）。ADR の「schema v6」は v7 と読み替え、migration は `0007_goals.sql` にする。ADR は書き換えず、design/persistence.md に v7 として書く。
- ADR の「journal テンプレートの `## Goal` 節を `## Scope` に変える」は 019 で行われなかった。T2 に含める（テンプレートと journal/README.md の1行だけ。既存 journal は触らない）。
- 検証コマンドは AGENTS.md の「変更後に必ず通す」4行を全 task 共通の `--verify` にする。T4 は plugin の skill 文書だけを触るので `cargo test --locked` は `tests/plugin.rs` の loader test を通すために残す。

### T1: 依存元 task の receipt summary と result commit を prompt に載せる（段階1）

```sh
cmux-taskq add "prompt: include predecessor tasks' receipt summary and result commit" \
  --description "ADR-0009 段階1。runtime が run の prompt.txt を書くとき（src/runtime.rs の provision → prompt()）、その task の直接の依存元（task_dependencies の predecessor）ごとに task ID、title、integrated run の receipt summary、result commit（main に積んだ squash commit）を 'Predecessor tasks' 節として載せる。依存元がなければ 'Predecessor tasks: none' と書き、節の有無で prompt の形を変えない。summary は integrated run の receipt.json（run_dir/receipt.json）から読み、読めない・存在しないときは '(receipt unavailable)' にして起動を止めない。同じ節に、claim 時点で in_progress の他の task（title と ID のみ。自分は除く）を 'Tasks in progress' として載せる。prompt() の signature は task と run に加えて predecessor の情報と in_progress task の一覧を受け取る形に変え、取得は queue（application.rs の trait）に read-only の操作を足して行う。docs/design/supervisor-lifecycle.md の prompt の記述を更新し updated / last_verified を今日にする。" \
  --acceptance "1. 依存元 A が integrated である task B の run の prompt.txt に、A の ID・title・receipt summary・result commit（A の integrated run の result_commit と一致）が含まれることを tests/runtime.rs の unit test で確認できる。2. 依存元のない task の prompt.txt に 'Predecessor tasks: none' があることを unit test で確認できる。3. claim 時点で他に in_progress の task があるとき、その ID と title が 'Tasks in progress' に載り、自分の task は載らないことを unit test で確認できる。4. A の receipt.json が削除されていても B の run は starting → running に進み、prompt に '(receipt unavailable)' が入ることを unit test で確認できる。5. schema と CLI は変えない。6. docs/design/supervisor-lifecycle.md に prompt の Predecessor tasks / Tasks in progress の記述がある。" \
  --verify "cargo fmt --all --check" \
  --verify "cargo test --locked" \
  --verify "cargo clippy --locked --all-targets -- -D warnings" \
  --verify "cargo llvm-cov --locked --fail-under-lines 80"
```

### T2: Goal エンティティ（schema v7、CLI、`Task.context`、receipt `follow_ups`）

```sh
cmux-taskq add "queue: add Goal entity, Task.goal_id and Task.context (schema v7)" \
  --description "ADR-0009 の Goal エンティティ。migrations/0007_goals.sql で goals テーブル（id INTEGER PK、title NOT NULL、description、acceptance、constraints、doc（任意）、closed_at、verdict（'achieved' | 'abandoned' | NULL）、created_at、updated_at）、tasks.goal_id（goals(id) への FK、NULL 可、index）、tasks.context（TEXT、既定 ''）を足し、user_version を 7 にする。domain.rs に Goal 構造体と、Task に goal_id: Option<i64> と context: String を足す。application.rs の trait と sqlite.rs に goal add / goal list / goal show / goal edit / goal close / set-goal を実装し、main.rs に次の CLI を足す: 'goal add TITLE --description --acceptance --constraints --doc'、'goal list'（id、title、closed、verdict、task 数と status 別の件数）、'goal show ID'（goal と所属 task の id/title/status）、'goal edit ID --title/--description/--acceptance/--constraints/--doc'、'goal close ID --verdict achieved|abandoned'、'set-goal TASK GOAL' と 'set-goal TASK --none'。'add' に '--goal ID' と '--context TEXT' を足す。規則: achieved は未終端（completed / canceled 以外）の task があれば拒否、abandoned は in_progress の task があれば拒否、閉じた goal への task 追加と付け替えは拒否、set-goal は task が draft / ready のときだけ許す（依存の追加・削除と同じ）。イベントは goal_created、goal_updated（新旧を payload に）、goal_closed、task_goal_changed で、run_id は null。goal に状態機械と verification_commands は持たせない。'show ID' の task に goal_id と context が出る。Receipt に任意の配列 follow_ups（要素は title と description の文字列）を足し、Receipt::check は形（配列であること）だけ確認して検証に使わない。docs/journal/000-template.md と docs/journal/README.md の journal の '## Goal' 節名を '## Scope' に変える（既存の journal は触らない）。docs/design/domain-model.md と persistence.md を更新し、updated / last_verified を今日にする。README の CLI 一覧に goal と set-goal を足す。ADR-0009 は書き換えない（schema 番号は v7 と読み替え、design に書く）。" \
  --acceptance "1. v6 の DB を開くと v7 に migrate され、既存 task の goal_id が NULL、context が '' で、task と run が保持されることを tests/queue.rs の migration test で確認できる。2. goal add → add --goal → goal show で所属 task が見え、goal list に status 別の件数が出ることを tests/cli.rs で確認できる。3. 未終端の task を持つ goal の close --verdict achieved が拒否され、in_progress の task を持つ goal の close --verdict abandoned が拒否され、全 task が completed または canceled なら achieved が通ることを unit test で確認できる。4. 閉じた goal への add --goal と set-goal が拒否され、in_progress の task への set-goal が拒否され、draft / ready への set-goal と --none が通ることを unit test で確認できる。5. goal_created / goal_updated（old と new を含む）/ goal_closed / task_goal_changed のイベントが記録され run_id が null であることを unit test で確認できる。6. follow_ups を含む receipt と含まない receipt の両方が Receipt::check を通り、follow_ups が配列でない receipt は拒否されることを unit test で確認できる。7. claim と candidates の挙動は変わらない（既存 test がそのまま通る）。8. docs/journal/000-template.md の節名が '## Scope' になり、docs/design/domain-model.md に Goal の項目と規則、docs/design/persistence.md に v7 の記述がある。" \
  --verify "cargo fmt --all --check" \
  --verify "cargo test --locked" \
  --verify "cargo clippy --locked --all-targets -- -D warnings" \
  --verify "cargo llvm-cov --locked --fail-under-lines 80" \
  --depends-on <T1>
```

### T3: prompt 拡張（goal の記述と制約、`Task.context`、同じ goal の兄弟）

```sh
cmux-taskq add "prompt: include goal, task context and in-progress siblings" \
  --description "ADR-0009 の prompt 拡張。T1 の prompt() を拡張し、task に goal があれば 'Goal' 節として goal の ID、title、description、acceptance、constraints、doc（パスをそのまま。内容は読まない）を載せ、なければ 'Goal: none, this task stands alone' と書く。task の context が空でなければ 'Context' 節として載せ、空なら 'Context: none' と書く。T1 の 'Tasks in progress' は同じ goal の in_progress task に限定し（goal がないときは全 in_progress task のまま）、'Sibling tasks in progress' と名前を変える。'Your assignment is this task only. Do not change what a sibling task owns; if you find work outside this task, record it in the receipt as follow_ups instead of doing it.' の趣旨の一文と、receipt の follow_ups の形（title と description の配列、任意）を prompt の receipt JSON 例に足す。goal の有無で prompt の節構成を変えない。prompt.txt は claim 時点のスナップショットで、goal edit は次の claim から反映されることを docs/design/supervisor-lifecycle.md に書き、updated / last_verified を今日にする。" \
  --acceptance "1. goal 付き task の run の prompt.txt に goal の title・description・acceptance・constraints・doc が含まれることを tests/runtime.rs の unit test で確認できる。2. goal なし task の prompt.txt に 'Goal: none, this task stands alone' と 'Context: none' があり、goal 付きと同じ節の並びであることを unit test で確認できる。3. 同じ goal に in_progress の兄弟がいるときその ID と title が 'Sibling tasks in progress' に載り、別 goal の in_progress task は載らないことを unit test で確認できる。4. context を持つ task の prompt.txt に context の本文が載ることを unit test で確認できる。5. goal edit のあとに claim した run の prompt.txt は新しい記述で、edit 前に claim した run の prompt.txt は変わらないことを unit test で確認できる。6. prompt の receipt JSON 例に follow_ups があり、tests/e2e.rs の stub provider が書く receipt（follow_ups なし）が引き続き検証を通る。7. docs/design/supervisor-lifecycle.md の prompt の記述に Goal / Context / Sibling tasks in progress とスナップショットの規則がある。" \
  --verify "cargo fmt --all --check" \
  --verify "cargo test --locked" \
  --verify "cargo clippy --locked --all-targets -- -D warnings" \
  --verify "cargo llvm-cov --locked --fail-under-lines 80" \
  --depends-on <T2>
```

### T4: plugin skill（課題を聞く → goal を登録 → task に分解）

```sh
cmux-taskq add "plugin: taskq skill registers a goal and decomposes it into tasks" \
  --description "ADR-0009 の plugin skill。plugins/claude-taskq/skills/taskq/SKILL.md の '2. Register a task' を「課題を聞く → goal add で登録 → task に分解して add --goal で登録 → ready」を標準手順に書き換える。一発 task（typo 修正、clippy 警告の解消など、1 task で終わり判断を揃える相手がいないもの）だけ goal なしを許し、その判断基準を skill に書く。goal add の各項目（title、description、acceptance、constraints（命名・境界・やらないこと）、doc（repository 内の参照文書のパス））と add の --goal / --context の集め方、goal list / goal show / set-goal / goal edit / goal close の使い方、'3. Inspect' の表に goal list と goal show を足す。'4. Report results' に、goal の全 task が completed になったら show の receipt の follow_ups を見て、goal の acceptance に照らして未達なら後続 task を同じ goal に add --goal してから goal close --verdict achieved を呼ぶ、という SV の手順を書く。taskq-run skill には、run の receipt に follow_ups があれば integrate の前後で SV がそれを report する一文を足す。docs/design/plugin-integration.md の skill の契約を更新し、updated / last_verified を今日にする。AGENTS.md の SV の「登録」表に goal add / add --goal / set-goal / goal close の行を足す。バイナリ（src/）は変えない。" \
  --acceptance "1. plugins/claude-taskq/skills/taskq/SKILL.md に goal add → add --goal → ready の手順、goal なしを許す基準、goal list / goal show / set-goal / goal edit / goal close の使い方、follow_ups を見て goal を close する手順がある。2. skill に書いた goal 系のコマンド列（goal add、add --goal、goal show、goal close）が、固定バイナリではなく T2・T3 を含む main を cargo build --locked したバイナリと使い捨て queue（--db）で実行して通ることを、receipt の evidence に実行ログとして残す。3. tests/plugin.rs の loader test が通り、skill の frontmatter（name、description）が有効なままである。4. docs/design/plugin-integration.md と AGENTS.md の登録表に goal 系の操作がある。5. src/ の変更がない（git diff main...HEAD --stat に src/ が含まれない）。" \
  --verify "cargo fmt --all --check" \
  --verify "cargo test --locked" \
  --verify "cargo clippy --locked --all-targets -- -D warnings" \
  --verify "cargo llvm-cov --locked --fail-under-lines 80" \
  --depends-on <T3>
```

### SV への依頼

- T1 → T4 の順に `add` し、返った ID を後続の `--depends-on` と、この Log に書く。4件とも `ready` にしてよい（直列なので同時に走るのは1件）。
- T3・T4 を最初の goal の実例にするため、T3・T4 は T2 が着地するまで `draft` のままにする（`ready` にしても依存で走らないが、`set-goal` は draft / ready のどちらでも通るので、どちらでもよい）。
- T2 の着地後にユーザーへ報告し、承認を得てから supervise を止め、T2 を含む main で `~/.local/bin/cmux-taskq` を更新して supervise を再起動する。新バイナリの最初の open で DB は v7 に migrate される。v6 の固定バイナリは v7 の DB を拒否するので、更新前に supervise と integrate を止め、走行中の run がない状態で行う。
- 更新後に `goal add "Goal groups (ADR-0009)" --description ... --acceptance ... --doc docs/adr/0009-goal-groups-tasks.md` で goal を作り、`set-goal <T3> <GOAL>` と `set-goal <T4> <GOAL>` で紐付けてから T3・T4 を `ready` にする。T1・T2 は `completed` なので `set-goal` は拒否される（ADR の規則どおり）。goal に紐付くのは T3・T4 の2件になる。
- T3 の run の prompt は T2 時点のバイナリが書くので goal 節は入らない。T4 の prompt に goal 節を入れたければ T3 の着地後にもう一度バイナリを更新する（任意。しなければ次の goal で確認する）。
- T4 の着地後、`goal show <GOAL>` で T3・T4 が completed であることを見て、goal の acceptance に照らして未達がなければ `goal close <GOAL> --verdict achieved` を流す。goal の ID と close の結果をこの Log に書き、Result を書いて `done` にする。

## Log

### 2026-09-22 claude (PLAN session)

- ADR-0009、plans/current.md、src/runtime.rs の `prompt()`（1110 行付近）、`provision()`（391 行付近で `prompt.txt` を書く）、domain.rs の `Task` / `Receipt`、application.rs の `Queue` trait、migrations/0006、plugins/claude-taskq/skills/taskq/SKILL.md、docs/design の見出しを読んで上の4定義を書いた
- 現在の schema は v6。ADR の「schema v6」は 018 の merge queue が先に v6 を使ったための誤りで、goal の migration は 0007 / v7 にする。ADR は書き換えない
- 019 は journal テンプレートの `## Goal` 節を変えなかったので、`## Scope` への改名を T2 に含めた
- T1 の「Tasks in progress」（全 in_progress task）は T3 で「Sibling tasks in progress」（同じ goal に限定）に置き換わる。T1 単独で並列の衝突回避に効かせるため、段階1で全 task を載せる
- T4 の受け入れ条件 2 は plugin の skill を実 CLI に対して通す手動確認で、run session が worktree でビルドしたバイナリと `--db` の使い捨て queue で行う。固定バイナリは v6 なので使えない
- 登録した task ID: T1 = 未登録、T2 = 未登録、T3 = 未登録、T4 = 未登録（SV が記入）

## Result

閉じるときに書く。

## Promoted

-
