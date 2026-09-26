---
id: adr-0068
type: adr
title: 着地のたびに着地待ちのrunをmerge-treeと軽い検査で先回りして確かめ、着地しなくなったrunは人の回答や着地の順番を待たずにresumeする
status: accepted
created: 2026-09-26
updated: 2026-09-26
accepted_on: 2026-09-26
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - operations
  - performance
related:
  - adr-0027
  - adr-0034
  - adr-0047
  - adr-0049
  - adr-0062
  - design-supervisor-lifecycle
---

# ADR-0068: 着地のたびに着地待ちのrunをmerge-treeと軽い検査で先回りして確かめ、着地しなくなったrunは人の回答や着地の順番を待たずにresumeする

## Context

[ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)の決定4は、reviewがpassしたrunの`/exit`の前に`git merge-tree`でmainとの衝突を判定する。判定はその一瞬のmainに対してだけで、その後に待つrunは次の`integrate`まで衝突にも build の失敗にも気づかれない。待つrunは次のとおり。

- `awaiting_integration`でleaseの無いrun: `approve_landing`のask（concern、失敗したreview）の答えを待つもの、runtime_errorで`recover`を待つもの。
- supervisorがslotに持つrun: 着地slotの空きを待つもの（`AwaitingSlot`）、`/exit`が効かず`stuck_exit`のaskを待つもの。

2026-09-26 08:52 JSTにtask 360が着地した直後、292・334・309が同時に崩れた。292はbuildの失敗で、360の`recovery.rs`の`NewAsk`に292が足した`finding_id`が無い、gitでは衝突しない意味の衝突だった。309はresumeを使い切ってretryになった。goal 39の記録では、2026-09-25にinboxが答えた後の着地5件（292・334・338・360・324）がすべて衝突し、待ちは2時間40分〜約8時間だった。人が答えるときにはmainは動いていて、答えの後にresumeの空きを待ち、冷えた会話で衝突を解くことになる。

[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定24（task 358）は、reviewがpassしていて衝突だけで止まったresumeを`MAX_RESUME_ATTEMPTS`に数えず、別の上限（`CONFLICT_ONLY_RESUME_LIMIT`）で止める。この repository は`CARGO_TARGET_DIR`をrun間で共有しない（AGENTS.mdの`[run.env]`の項、[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)）。

## Decision

1. **着地のたびに、着地待ちのrunを確かめる。**
   - supervisorが着地させたrun（`run_integrated`になったslot）が終わるたびに、着地待ちのrunをそのmainに対して確かめる（landing recheck）。対象は、`awaiting_integration`で`result_commit`（reviewを受けたhead）を持つrunのうち、leaseが無いもの（askの答えやrecoverを待つもの）と、このsupervisorがslotに着地待ちとして持つもの（`AwaitingSlot`、着地へ向かう`/exit`を待つもの。`stuck_exit`を含む）。review・reviseの途中のrunは決定4のpassの判定に任せ、他のprocessがleaseを持つrunは触らない。
   - recheckはloopの外のthreadで、同時に1つだけ走る。recheckの間に次の着地があれば、終わった後に最新のmainに対してもう1回だけ走る（途中の着地はまとめる）。drain・handoffに入ったsupervisorは新しいrecheckを始めない。
   - `integrate`を人が直接打った着地では走らない。次にsupervisorが着地させたときに確かめられる。
2. **確かめ方は`git merge-tree`と、`dagq.toml`の`[recheck] command`。**
   - まず`git merge-tree --write-tree <main> <head>`で衝突を見る（worktreeは触らない）。
   - 衝突が無く、repository rootの`dagq.toml`に`[recheck]`の`command`があるときは、mergeした木をmainの上の1 commit（`commit-tree`、refは作らない）にし、queue dirの`recheck/worktree`（専用のworktreeを1つだけ持ち、使うたびにそのcommitへ`checkout --detach --force`して`clean -ffdx`する）で、`/bin/sh`でcommandを実行する。envはそのrunの`[run.env]`（`${DAGQ_RUN_DIR}`はそのrunのrun dir）に、`CARGO_TARGET_DIR`をqueue dirの`recheck/target`に上書きしたもの。出力はrun dirの`recheck-<mainの先頭12桁>.log`。
   - commandはtaskのverificationから選ばない。verificationは`cargo llvm-cov`のような重いcoverageの関門を含み、recheckは着地のたびに待つrunの数だけ走るので、軽い検査をrepositoryが1つ決める。この repositoryでは`cargo check --locked --all-targets`とする。`[recheck]`が無ければmerge-treeだけを見る。
   - `[recheck]`を知らないバイナリは`dagq.toml`の未知の表を拒み、provisioningも`integrate`も止まる。この repositoryの`dagq.toml`に`[recheck]`を足すのは、本ADRを実装した固定バイナリ（`~/.local/bin/dagq`）への入れ替えの後の別taskにする。それまでは本番queueのrecheckはmerge-treeだけを見る。
   - recheckのtargetは専用の1つで、recheckは直列なので、同時にそのtargetを使うのは1本だけ。run間で`CARGO_TARGET_DIR`を共有しない理由（AGENTS.mdの(a)(b): testが別runのbuildした`target/debug/dagq`を実行しうること、同時の`cargo llvm-cov`がprofrawを消し合うこと）は、testもcoverageも実行しないrecheckの専用targetには当たらない。
   - `[run.env]`のprogramが見つからない間（ADR-0049の決定9）はcommandを実行せず、merge-treeだけを見る。
   - Gitやcommandがそもそも実行できなかったrunには何も記録しない（着地がいつもどおり確かめる）。
3. **着地しなくなったrunは、その場でresumeへ回す。**
   - 衝突はcode `rebase_conflict`（`conflicts`に衝突したpath）、commandの失敗はcode `verification_failed`（`command`・`exit_code`・`log_path`・`output_tail`）として、runに`landing_recheck_failed`を記録する。payloadには`main`、`head`、mainを動かした着地の`landed_run_id` / `landed_task_id`、`reason`、`action`を持つ。
   - leaseの無いrunは、同じtransactionで`awaiting_integration` → `needs_session`にし（`last_error`は`reason`）、`action: resumed`、`status: needs_session`を記録する。以後はふつうのresume（`resume_parked_runs`）が、askの答えや`integrate`の順番を待たずに、次のpassで空いたslotから拾う。resumeの依頼文は新しい種類（`ResumeKind::Recheck`）で、recheckが見つけたことと、commandが失敗したときはrebase後にそのcommandを手元で流して直すことを書く。
   - このsupervisorがslotに持つrunは、`action: held`で記録するだけにする（sessionが生きている間はresumeを開けない）。そのrunが着地しようとしたとき（`AwaitingSlot`で着地slotを取る直前）、最新のrecheckの失敗が`held`で、そのmainとheadが今のmainと`result_commit`に一致すれば、着地を始めずにleaseを手放して`needs_session`にする（`action: resumed`、`repeat: true`で同じ事実をもう1度記録する）。mainがさらに動いていれば、ふつうに着地を試み、着地のrebaseか検証が決める。
   - recheckの間にrunが動いた（headやstatusが変わった、他のprocessがleaseを取った）ときは何も記録しない。
4. **開いているaskにこの事実を足す。**
   - 記録したrunのaskのうち閉じていないもの（`approve_landing`・`stuck_exit`など、答えの有無を問わない）のquestionの末尾に、`Landing recheck: <reason>.`と、resumeしたこと（`held`なら、sessionが終わったら着地せずにresumeへ回すこと）を段落で足し、runに`ask_updated`（`ask_id`・`kind`・`why: landing_recheck_failed`）を記録する。
   - askは閉じない。`approve_landing`の答えは人の判断で、recheckは変えない。resumeが解決したrunは`validating`を通った後、閉じていない`approve_landing`のaskがあればreviewをやり直さず、sessionを`/exit`してworkspaceを閉じ、leaseを手放して`awaiting_integration`で答えを待つ。答えはこれまでどおり適用される（`land`はrebase済みのrunを着地させ、`send_back`・`cancel`も同じ）。approveされていたrun（`integration_approved`）は従来どおりreviewを経ずに着地に進む。
5. **resumeの数え方はtask 358に合わせる。**
   - recheckが衝突で止めたrun（`landing_recheck_failed`で`action: resumed`、code `rebase_conflict`）のresumeは、reviewのverdictを問わず`MAX_RESUME_ATTEMPTS`に数えず、`CONFLICT_ONLY_RESUME_LIMIT`で止める。衝突を作ったのは待ちで、run自身ではない。reviewがconcernのrunもここに入れるのは、人の答えを待つ間に着地が続くと、数えるresumeを人の答えより先に使い切ってしまうから。
   - commandの失敗で止めたrunのresumeは、着地の検証の失敗（`verification_failed`）と同じく数える。
   - resumeを使い切ったときに前のbranchを引き継いでretryするのは、task 358のとおりreviewがpassしたかapproveされたrunだけにする。
6. **statusとstatsに結果を出す。**
   - recheckが終わるたびに、mainを動かした着地のrunに`landing_recheck_finished`（`main`、`landed_run_id` / `landed_task_id`、`command`、`checked`・`clean`・`conflicts`・`check_failed`・`errors`・`resumed`・`held`の数、`failed_runs`、`duration_secs`、`supervisor`）を記録する。queue自体のevent（task・goal・runの無いevent）はschemaのCHECKが種類を限るので使わない。
   - `status`は最新の`landing_recheck_finished`のpayloadを`landing_recheck`（`at`付き）として出す。
   - `stats`は`backend_failures`と同じ窓で`landing_rechecks`（`rechecks`、`runs_checked`、`conflicts`、`check_failures`、`resumed`、各findingの`runs`）を出す。`held`の後の`repeat`は新しいfindingに数えず、`resumed`には数える。

## Alternatives

- **待っている間ずっとsessionを開けておく**: 人の答えまでslotとsessionを握り続け、並列度が落ちる（ADR-0027のAlternativesと同じ理由）。
- **mainが動くたびに待つrunをrebaseしてしまう**: 解決が要らない場合でもworktreeとheadが動き、reviewしたcommitとreceiptが食い違う。衝突すればどのみちsessionが要る。
- **検査をtaskのverificationから選ぶ**: 名前や形でcommandを選ぶ規則が脆く、`cargo llvm-cov`のような関門を待つrunの数だけ流すことになる。repositoryが1つ決める方が軽く、repositoryごとに変えられる。
- **runごとのtargetでcommandを流す**: 待つrunのworktreeは着地まで残るので使えるが、同時に走る検査とrunのbuildが同じtargetを取り合い、sessionが生きているrun（`stuck_exit`）のworktreeを外から書き換えることになる。専用のworktreeとtargetを1つ持って直列に回す。
- **衝突を見つけてもaskに書くだけにする**: 人が気づけるが、答えの後にresumeの空きを待ち、冷えた会話で解く状況は変わらない（goal 39の記録）。

## Consequences

- 着地待ちのrunが、次の`integrate`より前に、意味の衝突を含めて崩れたことを知る。人がaskに答える時点で、runはすでにmainに載せ直されているか、載せ直しの途中にある。
- 着地のたびに、待つrunの数だけ`git merge-tree`と（`[recheck]`があれば）commandが走る。この repositoryでは（`[recheck]`を足した後は）`cargo check --locked --all-targets`が専用のtargetで1本ずつ走り、host（8コア / 16GB）の負荷が少し増える。専用targetとscratch worktreeはqueue dirの`recheck/`に残り続ける。
- recheckが衝突を見つけたrunは、人の答えを待たずにresumeのslotを使う。その分、新しいclaimの順番が後ろになる。
- `approve_landing`の答えを待つrunは、resumeの後にreviewをやり直さない。reviewが見たのはrebase前の差分なので、rebaseで解いた衝突の中身は人の答え（`land`）と着地の検証（`integrate`のverification）が見る。
- `landing_recheck_failed`は新しいparkの種類で、`needs_session`の理由と`last_error_code`（ADR-0034の`rebase_conflict` / `verification_failed`）に出る。
- 実装はtask 462が本ADRと同時に行う。
