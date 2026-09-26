---
id: design-supervisor-lifecycle-run-worktrees
type: design
title: "Run worktrees"
status: current
created: 2026-09-26
updated: 2026-09-27
last_verified: 2026-09-27
scope: runtime
related:
  - design-supervisor-lifecycle
---

# Run worktrees

終わったrunのworktreeが占めるディスクを、supervisorが自動で空ける（task 376。goal 34のconstraintsの人の決定（2026-09-25）。2026-09-25に、消されずに残ったrunの`target/`（1 runあたり0.6〜3.4GB、合計20GB超）で空きが尽き、integrateの検証が`No space left on device`で落ちた）。対象は、leaseが無くslotに居ないrunのうち、`integrated`・`succeeded`・`failed`・`interrupted`で終わったものと、taskが`completed` / `canceled`になったもの（runの状態を問わない）（`ended_run_worktrees`）。worktreeのpathはrun_dirの下の`<runs>/<run-id>/worktree`で、`--repo`のcheckoutとその祖先には触れない。worktreeがもう無ければ何もしないので、何度見てもよい。

- **taskが`completed` / `canceled`**: worktreeとbranchを消す（`git worktree remove --force`、branchがあれば`git branch -D`）。`worktree_removed`（`path`、`branch`、`bytes`、`by: supervisor`、`reason`: `task_completed` / `task_canceled`）を記録する。前のrunのworktreeも、着地した（`integrate`がworktreeを消せなかった）runのものも同じ。run_dirとその記録、着地したcommitの`refs/dagq/runs/<run-id>`は残す
- **それ以外（taskがまだ`in_progress` / `ready`など）**: 次のrunやresumeが引き継ぐかもしれないのでworktreeとbranchは残し、worktree直下のビルド成果物（`target/`と`llvm-cov-target/`。`cargo llvm-cov`は既定で`target/llvm-cov-target`に作る）だけを消す。Gitがその下のfileをtrackしていれば消さず、linkはたどらない。`build_outputs_removed`（`paths`、`bytes`、`by: supervisor`）を記録する。ソース、commit、run_dirは残る。triageを待つrunも対象で、resumeされたら作り直す

`bytes`は消したものがディスクで占めていた量（blocks × 512、hard linkは1回だけ数える）。どちらのeventも人の手の代わりにruntimeが直したもので、goal 34の自動修正の件数に数える。

空き容量がclaimか着地の検証に足りないときも、supervisorは同じ掃除と`git worktree prune`を走らせ、何か消えれば`auto_repaired`（`repair: disk_cleanup`）を記録する（[空き容量を確かめる](disk-space.md)、task 377）。その閾値は直近の`build_outputs_removed`の`bytes`の最大値から決める。

消す契機は、supervisorのslotが終わったとき（`integrated`・`failed`・`interrupted`）、triageが終わったとき、triageの`decide` askとlandingの`approve_landing` askのanswer（`cancel`を含む）を適用したとき（そのtaskのrunだけ）と、上の掃除と同じ回（`sweep_ended_runs`、workspaceを閉じた後、全runを見直す）。手での`integrate`は着地したrunのworktreeを自分で消し、手での`recover`・人の`ready` / `cancel`・supervisorの外で終わったrunは次の掃除が拾う。失敗は`cleanup_failed`（`path`、`message`、`by: supervisor`）にして残りを続け、次の掃除で再び試す（supervisorのプロセスごとにworktreeあたり1回だけ記録する）。

closeの成否は`task_runs.workspace_closed_at`で表す。nullは「閉じたことを確認していない」で、closeの失敗だけでなく、cmuxが閉じた後にDBへ書けなかった場合も含む。closeの失敗は`cleanup_failed`イベントと`last_error`に残るが、run状態は変えない。閉じていないworkspaceをcleaned扱いにせず、再試行は`doctor`/`recover`で扱う。
