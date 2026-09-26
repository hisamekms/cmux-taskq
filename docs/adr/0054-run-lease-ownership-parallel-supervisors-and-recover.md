---
id: adr-0054
type: adr
title: supervisorがrun単位のleaseでrunのlifecycleを所有して並列に実行し、死んだsupervisorのrunを引き継ぎ、手放したrunをrecoverに回す
status: accepted
created: 2026-09-26
updated: 2026-09-26
accepted_on: 2026-09-26
supersedes:
  - adr-0003
  - adr-0007
  - adr-0025
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - persistence
  - lifecycle
  - operations
related:
  - adr-0003
  - adr-0007
  - adr-0025
  - adr-0039
  - adr-0042
  - adr-0045
  - adr-0047
  - adr-0049
  - adr-0062
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-supervise
  - design-supervisor-lifecycle-abandon
  - design-persistence
---

# ADR-0054: supervisorがrun単位のleaseでrunのlifecycleを所有して並列に実行し、死んだsupervisorのrunを引き継ぎ、手放したrunをrecoverに回す

## Context

runを誰が動かし、誰が片付け、止まったrunを誰が拾うかは3本のADRに分かれていた。

- [ADR-0003](0003-supervisor-owns-lifecycle.md)（2026-09-21）: workerのagent（ClaudeまたはCodex）は実装・テスト・subagentのreview・receiptの作成を担う。workspaceの削除までagentに任せると、agentが異常終了したときの後始末が不確実になり、失敗の調査と終了の競合も起きる。main sessionに全部を管理させると、main sessionが終わった時点で監視が失われる。そこでRustのsupervisorがworkspaceの作成・監視・完了の検証・削除を所有し、agentはworkspaceを消さずに結果を知らせると決めた。当時は「キューごとに1つのsupervisor」だった。
- [ADR-0007](0007-run-level-leases-parallel-execution.md)（2026-09-22）: 当時のsupervisorはqueue全体に1つのleaseを持ち、1件を処理して終わっていた。queue単位のleaseのままでは、常駐supervisorが生きている限りどのrunも`recover`できず、1つのsupervisorが止まると全runが孤児になり、`doctor` / `recover`がrunごとに判定できない。そこでleaseをrun単位にし（migration 0005、schema v5で`supervisor_leases`と`one_executing_run_per_queue`を廃止して`run_leases`へ移した）、依存が解けたtaskを上限付きで並列に実行する常駐ループにした。
- [ADR-0025](0025-leaseless-unfinished-run-is-a-recover-run-attention.md)（2026-09-23）: supervisorがabandonしたrunは、leaseが消えてstatusが未完了のまま残るが、どのattentionにも出ず、`status`でも`watch`でも見えなかった（2026-09-23のtask 49のrunがleaseの無い`running`で止まった）。そこでleaseの無い未完了runを`recover run`のattentionにした。

その後、ADR-0007の決定の一部は後のADRで変わった。「`awaiting_integration`になったrunのleaseを解放する」は、reviewをsupervisorの工程にしてleaseとslotをreviewの後まで持つ形に変わり（[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定2）、`/exit`の要求の時間切れではleaseを手放さなくなり（[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定25）、wrapperの死んだleaseの無いrunはsupervisorが自動で`recover`するようになった（ADR-0047の決定3）。ADR-0003の「キューごとに1つ」はADR-0007で複数を許す形に変わり、ADR-0025の「attentionを`watch`で受けて`recover`する担い手」は、今の役割に無く、attentionをinboxが受けて人に知らせる形になった（ADR-0047の決定17）。3本とも単独で開くと今も有効に見える。

[ADR-0042](0042-adr-is-superseded-whole-and-deprecation-date-is-deprecated-on.md)の規則に従い、このADRはADR 0003・0007・0025を丸ごと置き換える（[ADRの棚卸し](../plans/adr-inventory.md)の組C）。生きている決定を今の名前で書き直して引き継ぎ、上書きされた決定は今の形で書く。書き直しでは今の実装（[supervisor-lifecycle](../design/supervisor-lifecycle.md)とその[`supervise`](../design/supervisor-lifecycle/supervise.md)・[abandon](../design/supervisor-lifecycle/abandon.md)・[`recover`](../design/supervisor-lifecycle/recover.md)・[`doctor`](../design/supervisor-lifecycle/doctor.md)・[`status`](../design/supervisor-lifecycle/status.md)、`src/application/supervise/`、`src/infrastructure/runtime_store.rs`、`src/domain/mod.rs`の`run_attention` / `event_attention`）に合わせた。

死んだsupervisorのrunの引き継ぎ（adopt）と、自分のtokenのままstaleになったleaseの更新は[ADR-0039](0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)が持つ。ADR-0039は[ADR-0042](0042-adr-is-superseded-whole-and-deprecation-date-is-deprecated-on.md)の規則で書かれた`accepted`の統合ADRで、上書きされた決定を含まないので、このADRは束ねずに参照する。

## Decision

### lifecycleの所有

1. **supervisorがrunのlifecycleを所有する。** runtimeの`supervise`プロセス（supervisor）が、runのclaim、worktreeとcmux workspaceの作成（provision）、sessionの監視、receiptの検証（validating）、review、着地、workspaceのclose（後始末）を行う。workspaceとworktreeの寿命は分け、worktreeとbranchは着地まで残す（workspaceのcloseの後も残る）。workspaceを閉じる時点はreviewのverdictの後で（決定5）、所有者は変わらない。
2. **workerのagentはworkspaceを消さず、receiptで知らせる。** workerは割り当てられたworktreeで作業してcommitし、結果をreceiptに書いて止まる。merge・push・workspaceのclose・`/exit`はしない（`/exit`はsupervisorがidleを見て送る）。wrapper（`session`）はsessionのprocessを登録し、heartbeatと終了コードを記録するだけで、runの状態を進めない。
3. **同じqueueに複数のsupervisorが居てよい。** queue全体の排他は無い。supervisorは起動時に`supervisors`表に自分を登録し（token、PID、`--parallel`など）、runを1つも持たない常駐supervisorも`status` / `doctor`に並ぶ。claimは決定5のトランザクションで直列化されるので、同じtaskに2つのrunはできない。

### run単位のlease

4. **leaseはrun単位にする。** `run_leases(run_id → task_runs, token, pid, heartbeat_at)`の行が「supervisorがいまそのrunを見ている」ことを表す。tokenはsupervisorのプロセスに1つで、そのsupervisorの全leaseが同じtokenを持つ。supervisorは別threadで2秒ごとに、そのtokenの登録と全leaseのheartbeatを1トランザクションで更新する。leaseは、PIDが死んでいるかheartbeatが`HEARTBEAT_TIMEOUT_SECS`（30秒）より古ければstaleとみなす（`doctor` / `recover` / 引き継ぎで同じ規則）。`task_runs.supervisor_token`は、いまそのrunを動かしているsupervisorのtokenを指す（引き継ぎで付け替える規則はADR-0039の決定3）。
5. **claimとleaseの作成を1つのトランザクションで行う。** `claim_for_supervisor_in_order`がrun・`supervisor_token`・lease行を同じトランザクションで作るので、所有者の無いclaimed runは生じない。実行枠の制約は、taskごとの未完了run 1件（`one_unfinished_run_per_task`）と、supervisorごとの`--parallel`だけである。claimの順はADR-0049の決定4に従う。
6. **leaseとslotはreviewの後まで持つ。** supervisorはclaimしたrunのleaseとslotを、provision・監視・validating・headlessのreview・着地まで持ち続ける。`awaiting_integration`になった時点ではleaseを返さない（reviewの工程はADR-0049の決定2）。leaseを返すのは、runがsupervisorの手を離れて次の担い手（着地の完了、resume、復旧job、人の答え）を待つ時点で、主なものは次のとおり。abandon（決定8）以外は、leaseの削除と`lease_released`の記録を同じトランザクションで行う。
   - 着地が終わったとき（`integrated`。`lease_released`の`reason: integrated`）。
   - runを`needs_session`にしたとき（着地の衝突や検証の失敗、validatingの`evidence_missing` / `scope_violation`、resumeの終わりなど。resumeは`needs_session`のrunにleaseを取り直して行う。ADR-0047の決定24）。
   - runが`failed`になったとき（sessionの非0終了、validatingの失敗）と、`failed` / `interrupted`のrunの復旧job（triage）が終わったか失敗したとき（triageはrunにleaseを取り直して行う。ADR-0047の決定3）。
   - reviewが`concern`を返した、またはreviewが失敗して、`approve_landing`のaskを開いたとき（runは`awaiting_integration`のままleaseの無い状態で答えを待つ）。
   - drainや引き継ぎのexec（ADR-0045の決定10）で、supervisorが持ち続けられないrunを人に渡すとき。
   - runtime errorでrunを手放したとき（決定8）。abandonは`lease_released`を別に書かず、`runtime_error`のpayloadの`lease_released: true`で記録する。

   人の答えを待つrunをslotから外すかどうかは[ADR-0062](0062-runs-waiting-for-a-person-leave-the-slot.md)が持ち、そこでもleaseは返さない。

### 常駐ループ

7. **`supervise --parallel N`（既定4）は常駐ループにする。** 1秒ごとのループで、空きslotがあれば引き継ぎ（ADR-0039）の後に候補をclaimし、claimするときは`refs/heads/main`を読み直してbase commitにする（依存が解けたtaskは、先行taskを含むmainから始まる）。各runのwrapperの登録とheartbeat、receipt、idle marker、wrapperの終了、review・着地・復旧jobの子プロセスを1つのループで監視し、receiptの検証と着地はrunごとのthread（専用のSQLite接続）で行う。候補もactive runも無い間は2秒ごとに候補を見る。`--once`は「active runが無く、claimできるtaskも無ければ終了する」。SIGINT / SIGTERMは1回目でclaimを止めてactive runの終了を待ち（drain）、2回目で即終了する。ループを抜けるときは自分の登録を消す（heartbeatの失敗で終わるときだけは消さず、全runに`runtime_error`を記録してleaseと登録を残す。プロセスが終わればstaleになり、次のsupervisorが引き継ぐかrecoverする）。

### 手放す（abandon）

8. **1 runのruntime errorは、そのrunだけを手放す。** wrapperの登録が期限（45秒）までに来ない、wrapperのheartbeatが切れてそのプロセスも死んでいる、監視中のstepの処理（Gitの呼び出し、DB、workspaceのcloseの記録など）のエラー、のどれかでは、`last_error`と`runtime_error`（payload: `message`、`lease_released: true`）を書き、そのrunのlease行を削除し、status・`run_processes`・workspace・worktreeは変えない。supervisorは他のrunを続け、結果の`errors`にそのrunを載せる。leaseを消すのは、常駐supervisorが生きている間も`recover`がそのrunのprocessだけで判定できるようにするため。taskは未完了のrunで占有されたままなので二重実行にはならず、未登録のwrapperはleaseが無ければ登録できずClaudeを起動しない。leaseが無いので、他のsupervisorは引き継がない。
   - `/exit`の要求の時間切れ（`exit_request_timed_out`）と、`/exit`を送るcmuxの呼び出しの時間切れは、ここに含めない。leaseを手放さずに待つ（ADR-0047の決定25）。
   - wrapperのプロセスが生きたままheartbeatだけが黙ったsessionも手放さない（[wrapperが黙ったsession](../design/supervisor-lifecycle/silent-wrapper.md)）。
   - leaseを失った（lease行が無いか、他のtokenに変わった）ときは手放すのではなく退き、何も書かない（ADR-0039の決定5・7）。
9. **provisioningの失敗では、claimを止めてdrainし、非0で終わる。** worktree・run dir・workspaceの作成の失敗は環境の要因（cmuxやGitが落ちている）とみなし、そのrunを決定8のとおり手放した上で以後のclaimを止め、active runをdrainしてから非0で終了する。全候補を順に潰さない。

### runごとの`doctor` / `recover`

10. **`doctor` / `recover`はrunごとに判定する。** `doctor`は未完了のrunごとに、そのrunのlease（PID、生存、heartbeatの経過秒数、stale）と登録済みのwrapper / agentのprocessを報告し、`recover`を拒む理由（`blockers`）はそのrunのprocessとleaseだけから出す。`recover`は、未終了のprocessのPIDが生きている、leaseのheartbeatが30秒以内、leaseのPIDが生きている、のどれかなら拒否し、そうでなければ1トランザクションでそのrunだけを`interrupted`（`integrating`なら`awaiting_integration`、leaseの残った`awaiting_integration`はleaseだけを外す）にし、そのrunのleaseだけを削除して`run_recovered`を記録する。他のrun、そのleaseとprocess、worktree、branch、workspaceには触らない。`recover`はtaskを`ready`に戻さず、再実行するかは復旧jobのverdictで決まる（ADR-0047の決定3）。`status`は`supervisors`の登録（と、登録の無いtokenのlease保持者）ごとにsupervisorを並べ、leaseをtokenで登録に結び付けてそのsupervisorの`run_ids`に出す。
11. **見ているsupervisorの居ないrunのうち、processの止まったものはsupervisorが自分でrecoverする。** fill passごとに、`claimed` / `starting` / `running` / `validating`のrunで、lease行が無いか、leaseのPIDが死んでいて引き継ぎ（ADR-0039の決定1）の対象にならないもののうち、`blockers`が空のものを`recover`と同じ手順で`interrupted`にし（`run_recovered`に`by: "supervisor"`）、復旧jobに回す。これと引き継ぎの規則はADR-0047の決定3とADR-0039の決定1が持つ。人が`recover`を打つのは、supervisorが居ないとき、`integrating`のrun（着地中の`integrate`が死んだもの）、leaseの残った`awaiting_integration`をsupervisorが引き継がないとき、processが生きたままleaseの無いrunを人が止めたときである。

### 手放したrunのattention

12. **leaseの無い未完了runを`recover run`のattentionにする。** `in_progress`のtaskの最新runが`claimed` / `starting` / `running` / `validating` / `integrating`で、lease行が無ければ、`next`を`recover run`とするattentionにする（`domain::AttentionNext::RecoverRun`、判定は`domain::run_attention`）。`kind`はそのrunの直近の`runtime_error`。`integrating`を含めるのは、`integrate`がleaseの取得と`integrating`への遷移、leaseの解放と`integrating`からの遷移をそれぞれ1トランザクションで行い、正常な経路でleaseの無い`integrating`が生じないため。leaseがあってstaleなrunはこのattentionにしない（supervisorが死んだならその`supervisor_stale` / `supervisor_stopped`、wrapperが生きていれば引き継ぎの側）。leaseが無いことは、`/exit`の待ち（`stuck_exit`）より優先する。leaseの無い`awaiting_integration`は`recover run`ではなく、人の着地を待つrun（`review and integrate`、失敗したreviewなら`review by hand`。開いた`approve_landing`のaskがあればそのask）として出す。
13. **attentionになる`runtime_error`は`lease_released: true`のものだけにする。** `runtime_error`は`ATTENTION_KINDS`に含め、`event_attention`はpayloadの`lease_released`が`true`のものだけを`recover run`にする。leaseを手放さない`runtime_error`（heartbeatの失敗の記録など）は記録にとどめる。
14. **attentionはinboxが受け、人の指示で`dagq-recover`の手順を行う。** `recover run`は他のattentionと同じくinbox宛てで、inboxの`watch`が受けて人に知らせる（ADR-0047の決定17）。このattentionのための通知は足さない。processが止まっていれば決定11のとおりsupervisorが自分で`recover`するので、attentionが残るのは、processが生きている間と、supervisorが居ない間である。

### 旧ADRの決定からの対応

ADR-0003とADR-0007は決定に番号が無いので、Decisionの箇条を上から数えた番号（[ADRの棚卸し](../plans/adr-inventory.md)の「箇条N」）で示す。後のADR（0036以降）で、この3本の決定を番号で参照するものは無い（ADR-0003・0007・0025をADR全体として参照するものだけ。2026-09-26にgrepで確認）。

| 旧ADRの決定 | このADR |
| --- | --- |
| ADR-0003 箇条1（キューごとに1つのsupervisor） | 決定3（複数のsupervisorが居てよい） |
| ADR-0003 箇条1（supervisorがworkspace作成・監視・完了検証・削除を行う） | 決定1 |
| ADR-0003 箇条2（agentはworkspaceを削除せず、receiptで知らせる） | 決定2 |
| ADR-0007 箇条1（`run_leases`、supervisorごとのtoken、1文のheartbeat、旧表の廃止） | 決定4（廃止とmigrationはContextの記録） |
| ADR-0007 箇条2（claimとleaseを同じトランザクションで、実行枠はtaskごとの未完了run、複数supervisorでも直列） | 決定5、決定3 |
| ADR-0007 箇条3（`supervise --parallel N`の常駐ループ、`--once`、signal、`awaiting_integration` / `failed`でleaseを解放） | 決定7、決定6（leaseとslotはreviewの後まで） |
| ADR-0007 箇条4（1 runのruntime errorはそのrunだけを手放す） | 決定8（`exit_request_timed_out`は除く） |
| ADR-0007 箇条5（provisioningの失敗でclaimを止めてdrain） | 決定9 |
| ADR-0007 箇条6（`doctor` / `recover`はrunごと、`status`はsupervisorごと） | 決定10、決定11 |
| ADR-0025 決定1（leaseの無い未完了runを`recover run`にする） | 決定12 |
| ADR-0025 決定2（`lease_released: true`の`runtime_error`だけ） | 決定13 |
| ADR-0025 決定3（通知は足さない） | 決定14（inboxが受ける。ADR-0047の決定17） |
| ADR-0025 決定4（`doctor`を見て`recover`する） | 決定14、決定11（processの止まったrunはsupervisorが自分で`recover`する） |

## Alternatives

- **agent自身がworkspaceを削除する**（ADR-0003が退けた案）: agentが異常終了したり途中で終わったりしたときの後始末が不確実になる。
- **main sessionが全部を管理する**（ADR-0003が退けた案）: main sessionが終わると監視が失われる。
- **wrapperがrunの状態機械を持つ**: supervisorの死に影響されなくなるが、検証・review・着地をrunごとのプロセスに複製することになる。死んだsupervisorのrunはADR-0039の引き継ぎで拾う。
- **`task_runs`にsupervisorのpidとheartbeatの列を足す**（ADR-0007が退けた案）: 「解放済み」をnullで表すことになり、実行の記録と揮発する所有権が同じ行に混ざる。lease行の有無で所有を表す方が`recover`と`doctor`の判定が単純になる。
- **runごとにthreadを立てて直列の処理を並べる**（ADR-0007が退けた案）: workspace backendの共有と、1つのループで監視する方針に反する。検証と着地だけをthreadに出した。
- **abandonでもleaseを残す**（ADR-0007が退けた案）: 常駐supervisorではleaseが新鮮なまま残り、`recover`が「supervisorのpidが生きている」で永久に拒否する。taskは未完了のrunで占有されたままなので、leaseを消しても二重実行にはならない。
- **abandonでrunを`failed`や`interrupted`にする**（ADR-0025が退けた案）: abandonは「sessionが生きているかもしれない曖昧な失敗」で、processが止まったことを確かめずにstatusを進めると`recover`の安全確認を飛ばす。statusはそのままにし、processが止まっていれば決定11の自動`recover`で進める。
- **`runtime_error`をすべてattentionにする**（ADR-0025が退けた案）: leaseを持ったまま監視を続けるrunまで人を起こす。`lease_released`で区別できる。
- **queue全体の排他（supervisor 1つ）を残す**（ADR-0007が退けた案）: run単位のleaseがあれば要らず、testで`--db`を分けるのと同じ理由で複数のsupervisorを禁じる根拠が無い。
- **provisioningの失敗後もclaimを続ける**（ADR-0007が退けた案）: cmuxやGitが落ちていると全候補が`starting`で止まり、それぞれに`recover`が要る。
- **`awaiting_integration`になった時点でleaseを返す**（ADR-0007の元の形）: reviewの途中でsupervisorが死ぬと、leaseの無い`awaiting_integration`は人の着地待ちと区別できず、引き継ぎもできない。reviewのverdictを生きているsessionに返すにもleaseが要る。
- **ADR 0003・0007・0025を置き換えずに残す**: 「キューごとに1つのsupervisor」「`awaiting_integration`でleaseを解放する」「退役した役割が`recover`する」が`accepted`のまま残り、単独で開いた読み手が今も有効と読む（ADR-0042）。

## Consequences

- 依存の無いtaskは同時に走り、依存のあるtaskは先行taskの着地を待って、その後のmainから始まる。
- supervisorの入れ替え・kill・再起動で走っているrunは失われず、引き継ぎ（ADR-0039）かexecの引き継ぎ（[ADR-0045](0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)の決定10）で続く。1 runの復旧のためにsupervisorを止めなくてよい。
- reviewと着地の間もrunがslotを占めるので、同時に進むrunの数は`--parallel`で抑えられる（人の答えを待つrunの扱いはADR-0062）。
- 手放されたrunは`status` / `watch`に`recover run`として出る。processが止まれば次のfill passでsupervisorが`interrupted`にして復旧jobに回し、attentionは消える。`recover`の後、runは`interrupted`（`integrating`だったものは`awaiting_integration`）になる。
- workspaceのcloseの記録の失敗など、runが未完了でない時点の`runtime_error`（`lease_released: true`）は、`watch` / `events`では`recover run`になるが、`status`はrunのstatusどおりのattentionを出し、`recover`も拒否する。`status`の判定を正とする。
- 常駐supervisorはClaudeのworkspaceを`--parallel`まで同時に開く。
- ADR 0003・0007・0025は`superseded`になり、`superseded_by: adr-0054`を持つ。
