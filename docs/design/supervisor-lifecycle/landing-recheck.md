---
id: design-supervisor-lifecycle-landing-recheck
type: design
title: "Landing recheck"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0068
  - adr-0027
  - adr-0047
  - adr-0049
---

# Landing recheck

supervisorが着地させたrunが`run_integrated`で終わるたびに、着地待ちのrunをその着地の後のmainに対して先回りして確かめる（[ADR-0068](../../adr/0068-recheck-waiting-runs-after-each-landing.md)）。着地しなくなったrunは、askの答えや`integrate`の順番を待たずにresumeへ回る。実装は`src/application/supervise/recheck.rs`（supervisor側）と`src/domain/recheck.rs`（記録の形）。

## 対象と起動

- 対象: `awaiting_integration`で`result_commit`を持つrunのうち、leaseの無いもの（`approve_landing`のaskの答えやrecoverを待つもの）と、このsupervisorがslotに着地待ちとして持つもの（`AwaitingSlot`、`AfterExit::Land`の`/exit`を待つもの。`stuck_exit`のaskを含む）。reviewとreviseの途中のrunはpassの時のmerge-treeの判定（ADR-0027の決定4）に任せ、他のprocessがleaseを持つrunは見ない。
- 起動: slotが`Step::Done`で`integrated`のrunを返したとき（`note_landed`）、そのrunをrecheckの予定にする。loopの各回の`recheck_pass`が、走っているrecheckが終わっていれば結果を記録し、走っていなければ予定のものを始める。同時に走るのは1つで、走っている間の着地は最新の1件にまとまる。drain・handoffの回は新しく始めない。handoffは走っているrecheckが終わるのを待ってからexecする（commandが次のprocessのscratch worktreeで走り続けないように。予定だけのrecheckは捨てる）。handoffやadoptで作り直したrunが、recheckがresumeさせて`approve_landing`の答えを待つrun（最新のparkが`landing_recheck_failed`で、閉じていない`approve_landing`のaskがある）なら、reviewをやり直さずに答えを待つ（`adopt_review`）。対象が無ければ何も記録しない。
- recheckが走っている間と、結果を記録した回は、slotが空でもloopは終わらない（observerやplan reviewと同じ扱い。記録した回の次の回が、parkしたrunをresumeする）。

## 確かめ方

recheckのthreadが対象を1件ずつ確かめる（`recheck_runs`）。

1. `git merge-tree --write-tree --name-only --no-messages -z <main> <head>`（`Repository::merged_tree`）。衝突すれば`rebase_conflict`で、衝突したpathを持つ。
2. 衝突が無く、main checkoutの`dagq.toml`の`[recheck] command`（[Run environment](run-environment.md)）があれば、mergeした木を`commit-tree`でmainの上の1 commitにし（refは作らない）、queue dirの`recheck/worktree`に`Repository::checkout_scratch`で出す（worktreeでなければ`git worktree prune`の後に`git worktree add --detach --force`、worktreeなら`checkout --detach --force`と`clean -ffdxq`）。そこで`/bin/sh`でcommandを実行する。envはそのrunの`[run.env]`（`${DAGQ_RUN_DIR}`はそのrunのrun dir）に、`CARGO_TARGET_DIR=<queue dir>/recheck/target`を上書きしたもの。出力はrun dirの`recheck-<mainの先頭12桁>.log`。非0の終了は`verification_failed`で、`command`・`exit_code`・`log_path`・`output_tail`（末尾2000文字）を持つ。
3. `[recheck]`が無いか、`[run.env]`のprogramが見つからない間（ADR-0049の決定9）は1だけを見る。Gitやcommandが実行できなかったrunは`errors`に数え、runには何も記録しない。

この repositoryの`dagq.toml`には`[recheck] command = "cargo check --locked --all-targets"`を置く予定で、`[recheck]`を知らない旧バイナリは`dagq.toml`を読めなくなるので、固定バイナリを入れ替えた後の別taskで足す（それまではmerge-treeだけ）。target（`recheck/target`）は1つで、recheckは直列なので、同時にそれを使うのは1本だけ。worktreeとtargetはqueue dirの`recheck/`に残り、次のrecheckが使い回す。

## 見つかったとき

記録はloopのthreadで行う（`apply_recheck`）。runのheadかstatusがrecheckの後に変わっていれば何もしない。

- **leaseの無いrun**: `park_rechecked`が1 transactionでrunを`awaiting_integration` → `needs_session`にし（`last_error`は`reason`）、`landing_recheck_failed`（`code`、`main`、`head`、`landed_run_id`、`landed_task_id`、`conflicts`か`command`・`exit_code`・`log_path`・`output_tail`、`reason`、`action: resumed`、`status: needs_session`）を記録する。次のpassの`resume_parked_runs`（[`needs_session`](needs-session.md)）が、空いたslotからふつうのresumeとして拾う。依頼文は`ResumeKind::Recheck`（「waiting to land … the supervisor's landing recheck found that it no longer lands」）で、手順は着地の延期と同じrebaseと、commandの失敗ならrebase後にそのcommandを手元で流して直すこと。
- **このsupervisorがslotに持つrun**: `landing_recheck_failed`を`action: held`（`reason`付き）で記録するだけにする。そのrunが`AwaitingSlot`で着地slotを取る直前に`park_held_by_recheck`が、最新の`landing_recheck_failed`が`held`で`main`と`head`が今のmainと`result_commit`に一致するかを見て、一致すれば`park_rechecked`（このsupervisorのtoken）でleaseを手放して`needs_session`にし、同じ内容を`action: resumed`、`repeat: true`でもう1度記録する（`lease_released`の`reason`は`landing_recheck_failed`）。mainがさらに動いていれば着地を試みる。
- **askへの注記**: どちらの場合も、runの閉じていないask（答えの有無を問わない）のquestionの末尾に段落`Landing recheck: <reason>. …`を足し、`ask_updated`（`ask_id`、`kind`、`why: landing_recheck_failed`）を記録する（`AskStore::note_on_asks`）。askは閉じない。
- **resumeの後**: resumeが解決して`validating`を通ったrunに閉じていない`approve_landing`のaskがあれば、reviewをやり直さず、sessionを`/exit`してworkspaceを閉じ、leaseを手放して`awaiting_integration`で答えを待つ（`AfterExit::Rest { close: true }`。`stuck_exit`のaskの文面は「waits for the answer to its approve_landing ask」）。答えは`apply_landing_answers`がこれまでどおり適用する。`integration_approved`のあるrunはreviewを経ずに着地へ進む。
- **resumeの数え方**（ADR-0068の決定5、`domain::resume`）: `action: resumed`の`landing_recheck_failed`はparkのイベントで、code `rebase_conflict`ならreviewのverdictを問わず衝突だけのresume（`MAX_RESUME_ATTEMPTS`に数えず`CONFLICT_ONLY_RESUME_LIMIT`で止める）、`verification_failed`なら数えるresume。使い切ったときの引き継ぐretryは、reviewがpassしたかapproveされたrunだけ。

## 記録

- recheckが終わるたびに、mainを動かした着地のrunに`landing_recheck_finished`（`main`、`landed_run_id`、`landed_task_id`、`command`、`checked`、`clean`、`conflicts`、`check_failed`、`errors`、`resumed`、`held`、`failed_runs`（`run_id`・`code`・`action`）、`duration_secs`、`supervisor`）を記録する。
- [`status`](status.md)は最新の`landing_recheck_finished`のpayloadを`landing_recheck`（`at`付き。まだ無ければnull）として出す。
- [`stats`](stats.md)の`landing_rechecks`は、`backend_failures`と同じ窓の`rechecks`、`runs_checked`、`conflicts`、`check_failures`、`resumed`と、findingごとの`runs`（`task_id`、`run_id`、`code`、`action`、`landed_task_id`）。`repeat`は新しいfindingに数えず、`resumed`には数える。
- `landing_recheck_failed`はcode（ADR-0034）を持つので、`needs_session`のrunの`last_error_code`にも出る。[Conflict thresholds](conflict-thresholds.md)の`conflict_hotspots`はこのイベントを数えない（着地のrebaseとpassの時のprecheckの衝突だけ）。
