---
id: journal-014
type: journal
title: Dogfooding: failure, recovery, and procedures
status: done
created: 2026-09-22
updated: 2026-09-22
plan_step: 9
queue_task: 6
depends_on_journal: [13]
related:
  - plan-rust-runtime-mvp
---

# 014: Dogfooding: failure, recovery, and procedures

## Goal

[plans/current.md](../plans/current.md) ステップ9。実タスクで失敗または中断を1件起こし、リソース保持、状態確認、明示復旧、再試行を確認する。実taskはSVが[012のProcedure](012-dogfood-independent-task.md)に従って選び、登録内容と失敗のさせ方をこのジャーナルのLogに書く。セットアップ、実行、成果の取り込み、復旧の手順を文書化する。

完了条件: 復旧と再試行がDBの手修正なしで通り、次の小さな開発taskを同じ手順で流せる文書がある。plans/current.mdのステップ9とM1を完了にできる。

## Log

### 2026-09-22 13:49 claude (SV)

- T6（`--depends-on` B, C）: tests/cli.rs に `cmux-taskq --version` がクレートのバージョンを出し、queueなしで動くテストを追加する。verify: fmt / test / clippy
- 失敗のさせ方: runが `running` になったらagentプロセスを `kill -KILL` して `failed` にし、`doctor` → `recover RUN` → `ready ID` で再試行する。再試行のrunはそのまま完走させて着地する

### 2026-09-22 16:16 claude (SV)

- T6 は T4 の着地直後にclaim（run `4d58d1e8`、base `dfaac7b`、workspace:137 / 38B0F66D-…）。`running` を確認して agent（pid 92277、claude）に `kill -KILL`
- 6秒後: wrapperが終了コード128を報告 → run `failed`（`supervision_finished`）。worktree・run dir・workspaceは保持。他のrunなし、supervisorは継続
- `last_error` は null。固定バイナリ（`18800cd`）がT3の修正（`session exited with code N`）より前のため。バイナリ更新はユーザーに報告してから行う（AGENTS.md）
- `doctor` は failed run を出さない（未完了runのみ）。`recover 4d58d1e8` は「failed; only unfinished runs can be recovered」で拒否 — wrapperが終了を報告した失敗はorphanではないので `recover` 不要、READMEどおり `ready ID` で再試行
- AGENTS.md「失敗と中断」の手順どおり `cmux workspace close 38B0F66D-…`（OK workspace:137）で失敗runのworkspaceを閉じた
- `ready 6` → 次のpollで新run `9a897435`（base `82b8a6c` = 最新main）。失敗runは `failed` のまま履歴に残る
- 中断（supervisor kill → doctor → recover）の経路は010で実機確認済みなので、ここでは「失敗 → 再試行」のみ

### 2026-09-22 16:40 claude (SV)

- 再試行run `9a897435` は人の介入なしに receipt → 検証3件（fmt / test / clippy）→ close → `awaiting_integration`。差分は tests/cli.rs のみ。`integrate 6` → `fbebec4`、push。task 6 は `completed`、失敗run `4d58d1e8` は `failed` のまま履歴に残る
- 手順の文書: AGENTS.md（019）にSVの準備・登録・起動・監視・レビューと着地・`needs_session`・失敗と中断の各操作をCLIコマンド単位で記載済み。READMEに supervise / recover / integrate / plugin の節がある

## Result

実taskで失敗（agentの `kill -KILL` → `failed`）を起こし、リソース保持（worktree・run dir・workspace）、状態確認（`show`、`doctor`、`recover` の正しい拒否）、失敗runのworkspace close、`ready` による再試行、再試行runの着地まで、DBの手修正なしで通った。中断（supervisor kill → `recover`）の経路は010で実機確認済み。セットアップ・実行・着地・復旧の手順はAGENTS.mdとREADMEにあり、次の開発taskを同じ手順で流せる。

ドッグフーディング全体（012〜014、019）: 6 task、7 run（失敗1、着地6）、登録から最後の着地まで約3時間。mainは1 task = 1 squash commit（`88012c4`、`fedff3d`、`7e184d8`、`e862843`、`dfaac7b`、`fbebec4`）。

残課題（後続taskの候補）: 固定バイナリが `18800cd` のままでT3の `last_error` 修正を含まない（更新はユーザー判断）。READMEとsupervisor-lifecycle.mdの「`doctor` が失敗runのworkspace IDを出す」は誤り（T5のreceiptより）。active runのない常駐supervisorが `status` に見えない（010 Found 2）。

## Promoted
