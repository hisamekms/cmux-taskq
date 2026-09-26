---
id: design-supervisor-lifecycle-triage
type: design
title: "Triage (supervisor)"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0039
---

# Triage (supervisor)

ADR-0044の決定3（task 98）。`failed` / `interrupted`で止まったrunの次の一手を、supervisorが起動するheadlessのtriage jobが決め、そのverdictでruntimeが動く。これまで退役した常駐sessionが手で行っていた`recover` → `ready`（またはcancel）とworkspaceのcloseを置き換える。

1. **自動recover**: fill passごとに、`active_runs`のうち`integrating`以外でlease行の無いrunについて`doctor`と同じ判定（`run_health`）を行い、`blockers`が空（登録されたwrapper / agentのprocessが無いか、`exited_at`が記録済みか、PIDが死んでいる）なら`recover_run`で`interrupted`にする。`run_recovered`のpayloadは`recover`と同じで、`by: "supervisor"`が付く。taskは`ready`にしない（再実行はtriageのverdictに委ねる。ADR-0003・ADR-0007の「孤児runは自動再実行しない」は維持し、ADR-0012の「wrapperが死んだrunの`recover`は手動」を`recover`の実行だけ自動に改める）。staleなleaseを持つrunは、leaseのpidが死んでいて、adoptの対象（wrapperが生きているか終了を記録した`running` / `validating`、`resume_skipped`の後のrun、`awaiting_integration`）でなければ対象にする（task 236。supervisorとwrapperが両方死んだrun）。`run_recovered`の`lease_deleted`はtrueになる。leaseのpidが生きている（heartbeatが止まったまま生きているsupervisor）runは対象外。
2. **対象**: `in_progress`のtaskの最新runで`failed` / `interrupted`のもののうち、最後の`resume_started`より後に`triage_finished`も`triage_failed`も無いもの（`domain::triage_state`が`Pending`）。resumeの後にもう一度`failed`になったrunはもう一度triageされる。新鮮なleaseのあるrun（verdictの後の`/exit`を待っているsessionなど）は待つ。active runが上限未満のときだけ始め、triage中はrun slotを1つ使う。
3. **開始**: `begin_triage`が`BEGIN IMMEDIATE`の中で上の条件（runがtaskの最新runであることを含む）を再検査し（staleなleaseは置き換える）、runにleaseを取って`lease_acquired`（`reason: triage`）と`triage_started`（`attempt`、`status`）を記録する。同じrunを2つのsupervisorが取ろうとしても1つしか通らない。runのdirectory（無ければ`runs/<run-id>/`）に`triage-prompt-N.txt`を書き、`AgentProvider::headless_command`（Claudeでは`claude -p --allowedTools Read Grep Glob -- <prompt>`）をそのdirectoryで、`DAGQ_ROLE=reviewer`（queueの読み取りだけを許すreviewの役割を共用する）と`DAGQ_QUEUE`付きで起動し、stdout / stderrを`triage-N.out` / `triage-N.err`に書く。timeoutはreviewと同じ`AgentProvider::review_timeout`（既定600秒）。
4. **prompt**（`runtime::triage_prompt`）: taskのtitle・description・acceptance、runの`last_error`、receipt（末尾3000 byte）、run directoryの最新の`integrate`試行の`integrate-<attempt>-verify-N.log`（N順）とvalidatingが以前書いた`verify-N.log`（あわせて8つまで、各末尾3000 byte。それより前の試行のlogはpathだけ並べる。試行番号の無い旧名`integrate-verify-N.log`は最初の番号付きの試行より前の試行として読む）、`terminal-final.txt`の末尾、そのrunのevent（`events`と同じ圧縮形の直近40件）、同じtaskの他のrun（status、`last_error`、triageの`action`）。verdictのschema `{"verdict": "retry" | "resume" | "ask", "reason": string, "instruction": string}`（`resume`ならsessionへの解消依頼、`ask`なら人への質問を`instruction`に書く。`retry`では空でよい）と規則（taskの`failed` / `interrupted`のrunがこのrunを含めて`TRIAGE_RETRY_FAILURES`（2）以上ならretryを選ばない、resumeの試行が上限（`MAX_RESUME_ATTEMPTS`、3）に達していればresumeを選ばない）を渡す。
5. **verdictの適用**（`act_on_triage`）: stdoutから`TriageVerdict`を読む（全体か、最外の`{...}`）。runtimeも同じ規則を持ち、`retry`でtaskの`failed` / `interrupted`のrunが2以上、`resume`で試行が上限に達しているかworktreeが無い、のどれかなら`ask`に置き換える（理由は`triage_finished`の`overridden`）。
   - `retry`: taskを`in_progress`から`ready`に戻す（`task_status_changed`）。次のclaimが新しいrunを作る。
   - `resume`: runを`failed` / `interrupted`から`needs_session`にし、`last_error`に`instruction`（空なら`reason`）を書く。[`needs_session`](needs-session.md#needs_session)の自動resumeが拾い、resolution requestは「triageが差し戻した」文面（`ResumeKind::Triage`: `Do what the reason asks in this worktree and commit; if main moved, git rebase <main> first.`）で`Reason:`に`instruction`を載せる。試行の上限はrunごとの通算で数える。
   - `ask`: `kind: decide`のaskをrunに作る（`asked_by: supervisor`、options `retry` / `resume` / `cancel`、questionに`instruction`（無ければ上書きの理由）、`reason`、`last_error`、`triage-prompt-N.txt`のpath）。`ask`を通すのでinboxに`cmux notify`が届く。runはそのまま。
   - verdictを適用する前にleaseをまだ持っているかを確かめ、失っていれば（止まっていた間に別のsupervisorが引き継いだ）何も書かずにそのrunを手放す。`finish_triage`が遷移と`triage_finished`（`attempt`、`verdict`、`reason`、`instruction`、`overridden`、`failures`、`duration_secs`、`action`、`ask_id`、遷移後の`status`）を1トランザクションで記録する。
6. **workspaceのclose**: `triage_finished`の後、runが開いたworkspace（[Run workspaces](run-workspaces.md#run-workspaces)）のうち閉じた記録の無いもので、`cmux workspace list`に居るもの（`WorkspaceBackend::exists`）を閉じ、`workspace_closed`（`workspace_id`、`by: "triage"`、resumeのものなら`resume_attempt`。runのworkspaceなら`workspace_closed_at`も）を記録する。cmuxの失敗は`cleanup_failed`（`workspace_id`、`message`、`by`）に記録して続ける（`last_error`は変えない）。1つでも閉じたら、そのrunの閉じていない`stuck_exit`のaskを閉じる。最後にleaseを解放する。`triage_finished`の後のこれらの失敗（DBのerrorなど）はsupervisor logに書くだけで、`triage_failed`にはしない（verdictは適用済み）。
7. **headlessの失敗**: 起動できない、非0終了、timeout、stdoutにverdictが無い、verdictの適用中のerrorは`triage_failed`（`attempt`、`error`、`duration_secs`、`status`）を記録してleaseを解放し、runはそのままにする。`triage_failed`はattention（`triage by hand`）で、人がinboxかplannerのsessionから`ready` / cancelなどを打つ。supervisorは同じrunをもう一度triageしない（resumeされれば別）。
8. **askの回答**: fill passごとに、`asked_by: supervisor`の`decide`のaskで回答済み・未closeのもの（`triage_answers`）のうち、回答が`retry` / `resume` / `cancel`のどれかで、runが`failed` / `interrupted`でleaseが無いものを`decide_triage`で1トランザクションで適用する（runがtaskの最新runで、askが回答済みかつ未closeであることを中で再検査するので、2つのsupervisorが同じ回答を二重に適用しない。staleなleaseの行は同じトランザクションで消すので、止まっていたtriageのsupervisorが起きてもleaseを更新して書けない。[ADR-0039](../../adr/0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)の決定7）: `retry`はtaskを`ready`、`resume`はrunを`needs_session`（`last_error`は直近の`triage_finished`の`reason`に「ask Nで人がresumeを選んだ」を添えたもの。`ResumeKind::Triage`でresumeされる）、`cancel`はtaskを`canceled`にし、askを閉じ、`triage_decided`（`ask_id`、`answer`、`reason`、`status`）を記録する。taskが既に`in_progress`でないか、より新しいrunがあれば（人が`ready`を打ったなど）適用するものが無いのでaskを閉じるだけにする。人の`resume`はverdictの`resume`と違って試行の上限とworktreeを確かめないので、上限に達したrunは`needs_session`に戻ってもすぐ試行を使い切ったものとして`failed`と`decide`のask（`retry` / `cancel`）になる（[`needs_session`](needs-session.md#needs_session)の6）。それ以外の回答は人が読む（`read the answer of ask N and close it`）。`answer`はこの条件のaskの`ask_answered`に`runtime_delivers: true`を書き、attentionにしない（`status`では`applying the answer of ask N (runtime)`）。
