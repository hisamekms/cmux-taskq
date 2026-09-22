---
id: journal-014
type: journal
title: Dogfooding: failure, recovery, and procedures
status: open
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

## Result

## Promoted
