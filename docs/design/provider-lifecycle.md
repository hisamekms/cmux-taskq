---
id: design-provider-lifecycle
type: design
title: Agent provider lifecycle
status: current
created: 2026-09-21
updated: 2026-09-24
last_verified: 2026-09-24
scope: provider
related:
  - adr-0004
  - adr-0023
  - adr-0027
  - design-supervisor-lifecycle
---

# Agent provider lifecycle

application層はagent providerの共通契約を使い、CLI引数や出力形式を直接扱わない。

```text
AgentProvider
  preflight()              -- 実装済み: 実行可能性の確認
  command(run, prompt)     -- 実装済み: wrapperが起動するコマンド
  resume_command(run)      -- 実装済み: needs_sessionのrunを同じ会話で開き直すコマンド
  headless_command(cwd, prompt, allowed_tools) -- 実装済み: observerのheadless job（runを持たない）
  review_command(run, prompt) -- 実装済み: supervisorのheadless review（stdoutがverdict JSON）
  review_timeout()         -- 実装済み: headless reviewの上限（既定600秒）
  inspect / interrupt / collect_result  -- 後続

AgentSignals
  detect_prompt(screen)    -- 実装済み: 画面の末尾のダイアログのkind（trust / choice / confirm）
  screen_excerpt(screen)   -- 実装済み: askと`prompt_waiting`に載せる画面の末尾
  idle_hook(content)       -- 実装済み: idle markerの内容（background_running、evidenceに記録するhookのフィールド）
```

`AgentSignals`はsupervisorが生きているsessionのagentについて読むもの（画面とidle marker）で、形式がagent固有なのでproviderのadapterが実装する（Claude Codeは`src/infrastructure/claude.rs`）。applicationはkindの名前・画面の抜粋・background workの有無だけを受け取り、それがrunにとって何を意味するか（askにする、`/exit`を待つ）を決める。

コマンドを返すメソッドは`std::process::Command`ではなくapplicationの`CommandSpec`（program、引数、環境変数の設定と削除、cwdだけを持つ値。`Command`と同じ名前のbuilderを持つ）を返す。起動はapplicationの`Spawner` portが行い、標準入出力の行き先（wrapperの端末を継承、null、`<run-dir>`のファイル）は呼び出す側が`Streams`で決める。実装は`infrastructure::process::LocalSpawner`で、`CommandSpec`を`Command`に変えて子プロセスとして起動する（`infrastructure::process::command`。observerの`observe`もこれで`Command`にする）。こうしてsupervisorとsession wrapperのユースケース（`application::supervise` / `application::session`）はプロセスを直接扱わない（[supervisor-lifecycle](supervisor-lifecycle.md#supervise)）。

Claude Code adapter（`src/infrastructure/adapters.rs`）はworktreeをcwdにし、`--session-id`にrun IDを渡し、`--debug-file`をrun管理領域に置き、`--add-dir`でrun管理領域への書き込みを許可し、promptを位置引数で渡す。stdin/stdout/stderrはwrapperのTTYを継承する。permission modeは上書きしない。

加えて`command()`は`<run-dir>/claude-settings.json`を書いて`--settings`で渡す。内容は`Stop` hook 1件で、hookのstdin（イベントJSON）を`<run-dir>/idle.json`（`TaskRun::idle_marker_path`）へ一時ファイル + renameで書く。supervisorはこのmarkerをidle判定に使う（[supervisor-lifecycle](supervisor-lifecycle.md)）。`SessionEnd` hookは使わず、セッション終了はwrapperの終了コードで確認する。他のproviderは同じmarkerを自分の仕組みで書けばよく、書かなければ手動終了待ちになる。同じ設定に`autoMode.environment: ["$defaults"]`も入れ、auto modeの初回案内（Teach auto mode）を抑止する（[起動時のダイアログ](#起動時のダイアログ)）。

`review_command()`（[ADR-0023](../adr/0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)の決定2、[ADR-0027](../adr/0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)）はsupervisorが受理したrunをreviewさせる非対話のコマンドを返す。Claude Code adapterは`claude -p --debug-file <run-dir>/claude-review.log --add-dir <run-dir> --settings <run-dir>/claude-review-settings.json --allowedTools Read,Grep,Glob --disallowedTools Bash,Edit,Write,NotebookEdit -- <prompt>`をworktreeで起動する（worktreeは生きているworkerのsessionのものなので、reviewは読むだけ）。`claude-review-settings.json`は`autoMode.environment`だけで`Stop` hookを持たない: reviewの間もworkerのsessionは開いたままなので、reviewがidle markerを書くとsupervisorのidle判定（reviseの往復）を誤らせる。cmux workspaceは作らず、stdin / stdout / stderrはruntimeが繋ぐ（stdinはnull、stdoutとstderrは`<run-dir>/review-<attempt>.out` / `.err`）。runtimeは`review_timeout()`を過ぎたらkillし、stdoutの`{"verdict": "pass" | "revise" | "concern", "reasons": [..], "summary": ".."}`を読む（[supervisor-lifecycle](supervisor-lifecycle.md#review-supervisor)）。`headless_command()`と1つのportにしないのは、reviewがrunに属し、そのrun directoryの設定・debug file・`--add-dir`と禁止するtoolを要るのに対し、observerのjobにはrunが無いため。reviewの子プロセスにはruntimeが`DAGQ_ROLE=reviewer`と`DAGQ_QUEUE`を渡し、CLIはreviewerに読むコマンドだけを許す。print mode（`-p`）はfolder trustの判定を飛ばす（[binary から読める判定](#binary-から読める判定)の1）ので、reviewはtrust dialogで止まらない。

Claude providerはcmux内の通常セッションを起動し、実装、unit test、E2E、subagent review、完了レポートを実行させる。Codex providerはCodexの対応するセッション方式を使う。provider capabilityとしてinteractive、subagents、stream events、structured resultを表現する。

requested providerとactual providerをTaskRunに保存する。Claudeが起動不能の場合はCodexへfallbackできるが、実装途中の一般的な失敗は自動fallbackしない。

## Trust prompt

Claude Code の folder trust dialog（`Quick safety check: Is this a project you created or one you trust?` / `Yes, I trust this folder`、既定の選択は `No, exit`）が run worktree で出るかどうかは、**worktree の親 repository（`git rev-parse --git-common-dir` の親）の root が `~/.claude.json` の `projects` に `hasTrustDialogAccepted: true` で記録されているか**で決まる（通常の対話起動の場合。下の判定 1 と 3 の例外は run worktree には当てはまらない）。worktree の置き場所（scratch でも XDG data dir でも）、adapter が渡す `--session-id` / `--debug-file` / `--add-dir` / `--settings`、`--dangerously-skip-permissions` は無関係。journal 010 で全 session が止まったのは使い捨て repository を root で一度も開かずに supervise を始めたから、012〜014 で出なかったのは `~/ghq/github.com/hisamekms/dagq` が既に信頼済みだったからで、runtime の挙動は同じだった。

### 実験（2026-09-22、Claude Code 2.1.278、macOS）

起動は Python の `pty.fork` で `~/.local/bin/claude`（`versions/2.1.278` への symlink）を cwd を変えて起動し、`TERM=xterm-256color`、120x40、親（この run session）から継承する `CLAUDE*` 環境変数は削除した（supervise が cmux workspace の login shell から起動するのと同じ条件）。画面に dialog か通常の入力 UI（`Try "..."` / `auto mode on`）が出るまで最大 30 秒待ち、dialog が出たら Esc で終了（decline）か Down + Enter で承認して `/exit`（accept）した。dialog は model を呼ぶ前に出るので token は消費しない。承認の前後で `~/.claude.json` の `projects` のうち `hasTrustDialogAccepted` が真の key を比較した。

準備:

```sh
git init -q repo-B && git -C repo-B commit -q --allow-empty -m seed          # repo-D、repo-E も同じ
git -C repo-B worktree add -q ../wt-B1 -b wt-b1                              # scratch 配下
git -C repo-B worktree add -q ~/.local/share/taskq-trust-probe/runs/c1/worktree -b wt-c1   # XDG data dir 配下
git -C repo-D worktree add -q ~/.local/share/taskq-trust-probe/runs/d1/worktree -b wt-d1   # d2、repo-E の e1 も同じ
# 「adapter flags」= adapter と同じ引数。claude-settings.json は Stop hook で <run-dir>/idle.json を書く 1 件
claude --session-id <uuid> --debug-file <run-dir>/claude.debug.log --add-dir <run-dir> --settings <run-dir>/claude-settings.json
```

| # | cwd | 引数 | dialog | 備考 |
| --- | --- | --- | --- | --- |
| A | git でない新規 directory（scratch） | なし | 出る | |
| B1 | `repo-B` root（未信頼） | なし | 出る | |
| B2 | `wt-B1`（未信頼 `repo-B` の worktree、scratch） | なし | 出る | |
| B3 | `repo-B` root で承認 | なし | 出る → 承認 | `projects[<repo-B root>].hasTrustDialogAccepted: true` だけが追加される |
| B4 | `wt-B1`（B3 の後） | なし | 出ない | |
| C1 | `~/.local/share/taskq-trust-probe/runs/c1/worktree`（信頼済み `repo-B` の worktree） | なし | 出ない | |
| C2 | 同上 | adapter flags | 出ない | |
| D1 | `.../runs/d1/worktree`（未信頼 `repo-D` の worktree、XDG data dir） | なし | 出る | |
| D2 | 同上 | adapter flags | 出る | `--add-dir` / `--settings` は抑止しない |
| D3 | 同上で承認 | なし | 出る → 承認 | 追加される key は worktree の path ではなく `<repo-D root>` |
| D4 | `.../runs/d2/worktree`（`repo-D` の別 worktree、D3 の後） | なし | 出ない | |
| D5 | `repo-D` root（D3 の後） | なし | 出ない | |
| E | `.../runs/e1/worktree`（未信頼 `repo-E` の worktree） | `--dangerously-skip-permissions` | 出る | permission mode は trust dialog を飛ばさない |

B3 / D3 で `~/.claude.json` に残った `projects` の key は repository root の path だけで、worktree の path は一度も書かれない。D3 → D4 / D5 は journal 010 の観測と整合する: 並列に起動した 2 つの run session（task 8 / 9）は両方 dialog で止まり、片方で承認すると repository root が信頼済みになるので、その後に起動した session は止まらない。

### binary から読める判定

`strings ~/.local/share/claude/versions/2.1.278` の minified JS で、trust 判定は次の順（関数名は minified のもの）。

1. `CLAUDE_CODE_SANDBOXED` が設定されている、session 内で既に承認済み（`sessionTrustAccepted`）、または print mode（`-p`）なら trusted（実験では未確認）
2. cwd の「project key」= git root。linked worktree は `.git` file から common dir を辿った **canonical root**（親 repository の root）に解決される。`projects[key].hasTrustDialogAccepted` が真なら trusted
3. そうでなければ cwd から親 directory を 1 段ずつ上がり、途中の directory が `projects` で信頼済みなら trusted。上がるのは git root（linked worktree ではその worktree の root）まで、git repository の外では `/` まで
4. 承認時に書く key も同じ canonical root（`oV(Xw(cwd))`）

つまり worktree が repository の外にあっても親 repository の信頼が効き、逆に worktree を repository の中（`.worktrees/` など）に置いても親 repository が未信頼なら出る。

### 推奨する後続

1. 運用手順として文書化する（runtime 変更なし、推奨）: ある repository で初めて `supervise` を流す前に、その repository の root で `claude` を一度起動して dialog を承認する（または `~/.claude.json` の `projects[<root>].hasTrustDialogAccepted` が真であることを確認する）。使い捨て repository のスモーク（journal 010 の手順）も root を先に信頼する。task 16 で当時の plugin skill（名前は `taskq-run`、task 100 で退役した常駐 session 用の skill の前身）の「every run's worktree is a directory Claude Code has never seen」という誤った本文をこの条件に書き換えた
2. ~~adapter の `preflight()` で `~/.claude.json` を読み、repository root が未信頼なら warning（event か stderr）を出す~~ → task 92 で `up` の preflight にした（warning ではなく error で止める。[起動時のダイアログ](#起動時のダイアログ)）。dialog を抑止する CLI flag は 2.1.278 / 2.1.280 にはないので adapter flag では解決できず、runtime が `hasTrustDialogAccepted` を書き込むのはユーザーの判断を代行することになるので採らない
3. 未信頼の repository で `supervise` を始めてしまった場合の扱いは、task 16 で plugin の skill に入れた（task 65 で run の session 用の skill に分割、task 100 で `dagq-recover` の `reference/session.md` に移した）: 最初の承認より前に起動した session（最大 `--parallel` 件）はすべて dialog で止まる。supervisor はそれぞれを `answer_prompt` の ask として inbox に上げ、人の指示で `send-key down` + `enter` を送る。承認後に起動した session には出ない

## 起動時のダイアログ

2026-09-23 に worker が起動直後に止まったダイアログは trust（5 件）、LSP plugin の推奨、auto mode の初回案内（Teach auto mode）の 3 種類。Claude Code 2.1.280 の binary（`strings ~/.local/share/claude/versions/2.1.280` の minified JS。関数名は minified のもの）から、settings.json と CLI flag で抑止できるかを調べた。結果と runtime の扱い:

| ダイアログ | 表示の条件（2.1.280） | settings / flag で抑止 | runtime の扱い |
| --- | --- | --- | --- |
| folder trust | [Trust prompt](#trust-prompt) のとおり repository root の `projects[<root>].hasTrustDialogAccepted` | できない（flag なし。`--dangerously-skip-permissions` も効かない。`CLAUDE_CODE_SANDBOXED` は sandbox を偽ることになるので使わない） | `up` の preflight で検査して、未信頼なら案内付きの error で止まる |
| Teach auto mode（`Teach auto mode about your environment?`） | `xP()`: auto mode の gate が有効、**settings の `autoMode.environment` が空**、`numStartups >= 5`、`autoModeEnvSetup.denials >= 5`、`dismissed` でなく `dismissedAt` から 7 日（`dnt=604800000`）経過 | できる: `autoMode.environment` が 1 件以上あれば出ない。読むのは `userSettings` / `flagSettings` / `policySettings`（`qwe`）で、`--settings` で渡す run の設定は `flagSettings` | run の `claude-settings.json` に `"autoMode": {"environment": ["$defaults"]}` を書く。`$defaults` は組み込みの environment をその位置に継承するので classifier の挙動は変わらない（`claude --settings <この設定> auto-mode config` の実効 `environment` は設定なしと同じ 21 件で、`$defaults` は展開される。2026-09-23 に確認） |
| LSP plugin の推奨（`LSP plugin recommendation` / `Would you like to install this LSP plugin?`） | `rno()`: global config（`~/.claude.json`）の `lspRecommendationDisabled` が真か `lspRecommendationIgnoredCount >= 5`（`tno=5`）なら出ない。それ以外は session で開いたファイルの拡張子に合う LSP plugin が marketplace にあれば session に 1 回 | できない: 判定は global config だけを見て、settings.json の key も CLI flag も無い（`--bare` は LSP を切るが settings の hook も切るので Stop hook が動かない） | 何もしない。ダイアログは 30 秒（`s$e=30000`）応答が無ければ `timeout` で閉じて `lspRecommendationIgnoredCount` を 1 増やすので、止まるのは最長 30 秒で、5 回無視されると以後出ない（この machine は 2026-09-23 時点で既に 5）。runtime が `~/.claude.json` を書くのはユーザーの設定を代行するので採らない。止めたいユーザーは推奨の `Disable all LSP recommendations` を選ぶ |

- trust の判定は `$CLAUDE_CONFIG_DIR/.claude.json`（未設定なら `~/.claude.json`）の `projects` を、main checkout の root（`git rev-parse --git-common-dir` の親。`up` を linked worktree から打っても同じ key になる。common dir が `.git` でない配置では `--show-toplevel`）の path で引く（`claude_trusts_repository`）。親 directory の信頼は見ない（[binary から読める判定](#binary-から読める判定)の 3 のとおり、Claude Code も git root より上は辿らない）。config が無い、HOME も `CLAUDE_CONFIG_DIR` も無い、key が無い、`hasTrustDialogAccepted` が `true` でない、のどれも未信頼として `up` を止める。parse できない config は別の error。`supervise` 自体は検査しない（`up` を経ない起動は従来どおり dialog で止まり、`prompt_waiting` になる）
- 抑止したダイアログは task 101 の `prompt_waiting` の検知とは重ならない: 検知は画面の兆候を見るだけで、出なくなったダイアログは検知されないだけ。goal 11 の受け入れ条件「`prompt_waiting` が trust 以外で出ない」は、LSP の推奨が 30 秒で閉じる（検知は 90 秒後から）ことと Teach auto mode の抑止で満たす

