---
id: design-provider-lifecycle
type: design
title: Agent provider lifecycle
status: current
created: 2026-09-21
updated: 2026-09-22
last_verified: 2026-09-22
scope: provider
related:
  - adr-0004
  - design-supervisor-lifecycle
---

# Agent provider lifecycle

application層はagent providerの共通契約を使い、CLI引数や出力形式を直接扱わない。

```text
AgentProvider
  preflight()              -- 実装済み: 実行可能性の確認
  command(run, prompt)     -- 実装済み: wrapperが起動するコマンド
  inspect / interrupt / collect_result  -- 後続
```

Claude Code adapter（`src/infrastructure/adapters.rs`）はworktreeをcwdにし、`--session-id`にrun IDを渡し、`--debug-file`をrun管理領域に置き、`--add-dir`でrun管理領域への書き込みを許可し、promptを位置引数で渡す。stdin/stdout/stderrはwrapperのTTYを継承する。permission modeは上書きしない。

加えて`command()`は`<run-dir>/claude-settings.json`を書いて`--settings`で渡す。内容は`Stop` hook 1件で、hookのstdin（イベントJSON）を`<run-dir>/idle.json`（`TaskRun::idle_marker_path`）へ一時ファイル + renameで書く。supervisorはこのmarkerをidle判定に使う（[supervisor-lifecycle](supervisor-lifecycle.md)）。`SessionEnd` hookは使わず、セッション終了はwrapperの終了コードで確認する。他のproviderは同じmarkerを自分の仕組みで書けばよく、書かなければ手動終了待ちになる。

Claude providerはcmux内の通常セッションを起動し、実装、unit test、E2E、subagent review、完了レポートを実行させる。Codex providerはCodexの対応するセッション方式を使う。provider capabilityとしてinteractive、subagents、stream events、structured resultを表現する。

requested providerとactual providerをTaskRunに保存する。Claudeが起動不能の場合はCodexへfallbackできるが、実装途中の一般的な失敗は自動fallbackしない。

## Trust prompt

Claude Code の folder trust dialog（`Quick safety check: Is this a project you created or one you trust?` / `Yes, I trust this folder`、既定の選択は `No, exit`）が run worktree で出るかどうかは、**worktree の親 repository（`git rev-parse --git-common-dir` の親）の root が `~/.claude.json` の `projects` に `hasTrustDialogAccepted: true` で記録されているか**で決まる（通常の対話起動の場合。下の判定 1 と 3 の例外は run worktree には当てはまらない）。worktree の置き場所（scratch でも XDG data dir でも）、adapter が渡す `--session-id` / `--debug-file` / `--add-dir` / `--settings`、`--dangerously-skip-permissions` は無関係。journal 010 で全 session が止まったのは使い捨て repository を root で一度も開かずに supervise を始めたから、012〜014 で出なかったのは `~/ghq/github.com/hisamekms/cmux-taskq` が既に信頼済みだったからで、runtime の挙動は同じだった。

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

1. maintainer 手順として文書化する（runtime 変更なし、推奨）: ある repository で初めて `supervise` を流す前に、その repository の root で `claude` を一度起動して dialog を承認する（または `~/.claude.json` の `projects[<root>].hasTrustDialogAccepted` が真であることを確認する）。使い捨て repository のスモーク（journal 010 の手順）も root を先に信頼する。task 16 で `plugins/claude-taskq/skills/taskq-maintain/SKILL.md`（旧 `taskq-run`）の「every run's worktree is a directory Claude Code has never seen」という誤った本文をこの条件に書き換えた
2. adapter の `preflight()` で `~/.claude.json` を読み、repository root が未信頼なら warning（event か stderr）を出す。dialog を抑止する CLI flag は 2.1.278 にはないので adapter flag では解決できず、runtime が `hasTrustDialogAccepted` を書き込むのはユーザーの判断を代行することになるので採らない
3. 未信頼の repository で `supervise` を始めてしまった場合の扱いは、task 16 で plugin の skill `taskq-maintain`（[ADR-0010](../adr/0010-maintainer-and-resident-supervisor.md) の T3）に入れた: 最初の承認より前に起動した session（最大 `--parallel` 件）はすべて dialog で止まるので、それぞれに `send-key down` + `enter` で応答する。承認後に起動した session には出ない
