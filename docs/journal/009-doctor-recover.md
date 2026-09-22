---
id: journal-009
type: journal
title: doctor and recover
status: done
created: 2026-09-22
updated: 2026-09-22
plan_step: 4
queue_task: null
depends_on_journal: [5]
verify:
  - cargo fmt --all --check
  - cargo test --locked
  - cargo clippy --locked --all-targets -- -D warnings
related:
  - design-supervisor-lifecycle
  - design-persistence
---

# 009: doctor and recover

## Goal

[plans/current.md](../plans/current.md) ステップ4。停止したsupervisorや孤児runを人が確認して明示的に復旧できるようにする。

- `doctor`: stale lease、heartbeat切れのrun、終了していないwrapper/agentプロセス、存在しないworktree/workspaceを一覧する。状態は変えない。
- `recover RUN`: 旧プロセスの停止を確認してからrunを`interrupted`（または`failed`）にし、leaseを解放する。リソースは削除しない。
- 再試行は`ready`に戻して新しいTaskRunを作る。孤児runは自動再実行しない。

完了条件: supervisor強制終了後に`doctor`が状態を報告し、`recover`後に同じtaskを新しいrunで再実行でき、生きているプロセスがある間は`recover`が拒否されることをテストで確認できる。

## Log

### 2026-09-22 claude

- worker として開始。branch `journal/009-doctor-recover`、worktree `.worktrees/009-doctor-recover`。006-008 が並行しているので変更は `doctor` / `recover` に必要な範囲に絞る
- 確認: `interrupted` は `RunStatus` と `task_runs.status` の CHECK 制約に最初から含まれている。migration は不要（schema version 2 のまま）
- 決定: `recover` は run を `interrupted` にして lease を消すだけにし、task は `in_progress` のまま残す。再試行は別操作 `ready ID`（または編集したいときは `draft ID`）で行う。理由: (1) `failed` run の後も task は `in_progress` で止まっており、再試行の入口を `interrupted` だけに用意すると `failed` の再試行手段が無いまま残る。「未完了 run が無い `in_progress` task は手動操作を受け付ける」という1つの規則で両方を扱える。(2) 復旧（旧プロセスの停止確認）と再実行の判断は別で、recover が勝手に ready に戻すと「孤児 run を自動再実行しない」との境界が曖昧になる。(3) 依存や検証コマンドを直してから再試行したい場合は draft を経由する必要がある
- 決定: pid の生存確認は `libc::kill(pid, 0)`。`EPERM` は「存在するが他人のプロセス」なので生存扱い、`ESRCH` だけを死亡扱いにする。`libc` は rusqlite 経由で既に Cargo.lock にあるので直接依存に足すだけ
- 決定: `recover` は run の未終了プロセス（`exited_at IS NULL`）の pid が生きている間、lease の heartbeat が 30 秒以内、または lease の pid が生きている間は拒否する。lease pid の確認は指示の範囲を超えるが、heartbeat が止まったまま生きている supervisor（DB 障害・hang）から lease を奪うと `release_supervisor` が失敗するだけでなく所有権が二重になるので、doctor で見せて人に止めてもらう。`exited_at` 済みのプロセスは pid 再利用の誤検知を避けるため確認しない

### 2026-09-22 claude (実装)

- 実装: `adapters::process_alive`（`libc::kill(pid, 0)`）、`SqliteQueue::active_runs` / `recover_run`（`BEGIN IMMEDIATE` 内で lease の鮮度と `run_processes` 行数を再検査）、`runtime::doctor` / `recover`（`LeaseHealth` / `ProcessHealth` / `RunHealth` / `DoctorReport` を JSON 化）、CLI `doctor` / `recover RUN_ID`
- `TaskStatus::transition(action, unfinished_run)` に引数を追加し、未完了 run（claimed/starting/running/validating/awaiting_integration）が無い `in_progress` から ready/draft/canceled へ戻せるようにした。`sqlite::transition` が同じトランザクション内で `has_unfinished_run` を引く。`one_unfinished_run_per_task` と READY_QUERY の集合と同じ定義
- `TaskRun` に `supervisor_token` を出す案は `show` の JSON と 006-008 の差分に影響するのでやめた。doctor は lease と run の突き合わせを出さない
- `recover` は `run_processes` の `exited_at` を書かない。wrapper が終了を報告していない事実をそのまま残し、確認した pid の状態は `run_recovered` の payload に置く。`last_error` も上書きしない
- テスト: `orphan_run` ヘルパーで supervise と同じ手順（acquire → claim → plan_run → worktree 作成 → workspace_created → register_wrapper/agent）を supervisor ループなしで組み、wrapper/agent に実際の `sleep 60` 子プロセスを登録。(1) 全部生存 → doctor が報告し recover 拒否、ready も拒否 (2) lease を stale + 死んだ pid に更新しても agent 生存で拒否 (3) 子プロセスを kill → recover 成功、`interrupted`、lease 削除、`run_recovered`、worktree 残存、task は in_progress (4) `ready` → `supervise` で2本目の run が `awaiting_integration`、1本目は path を保持。別テストで `exited_at` 済みプロセス（pid は生きているテストプロセス自身）は無視されること、lease 無しでも復旧できること、`draft` に戻せることを確認。`failed` run からの `cancel` と CLI の `doctor` / `recover` も追加。runtime 17件、全体 32件
- fmt / clippy / `cargo llvm-cov --fail-under-lines 80` 通過。行カバレッジ 87.43%（着手前 86.03%）
- migration なし、Cargo.lock は `libc` を直接依存に足した1行のみ
- 実機（supervisor を kill → doctor → recover）は未検証。010 のスモークで確認する

## Result

`doctor` が lease（PID 生存・heartbeat 経過・stale）と未完了 run（workspace ID、worktree / run dir / receipt の存在、wrapper/agent の PID 生存と heartbeat、`blockers`、`recoverable`）を状態を変えずに JSON で報告する。`recover RUN_ID` は登録プロセスの生存、lease の heartbeat、lease PID の生存のいずれかがあれば拒否し、なければ run を `interrupted` にして lease を削除し `run_recovered` を記録する。worktree / branch / workspace / `run_processes` / task の `in_progress` は触らない。再試行は `ready ID`（または `draft ID`）で行い、未完了 run が無い `in_progress` task だけがこの遷移を受け付ける。`failed` run の後の再試行・cancel も同じ規則で可能になった。

完了条件（supervisor 停止後の doctor 報告、recover 後に同じ task を新しい run で再実行、生存プロセスがある間の recover 拒否）をテストで確認。migration 不要。

未検証: 実機での supervisor kill → doctor → recover（010）。

## Promoted

- [design/supervisor-lifecycle.md](../design/supervisor-lifecycle.md): `doctor` / `recover` の手順と拒否条件、復旧後の再試行経路
- [design/domain-model.md](../design/domain-model.md): 未完了 run が無い `in_progress` からの手動遷移、`recover` の位置付け
- [design/persistence.md](../design/persistence.md): token なしで run を変える唯一の操作としての `recover`、migration 不要の記録
- [README.md](../../README.md): コマンド表、"Recover an interrupted run" 節
- ADR は追加しない。「孤児 run を自動再実行しない」「再試行は新しい TaskRun」は plans/current.md ステップ4と ADR-0003 の帰結で、recover が ready に戻さない選択は design に記載した
