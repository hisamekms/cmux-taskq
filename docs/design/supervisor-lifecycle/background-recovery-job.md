---
id: design-supervisor-lifecycle-background-recovery-job
type: design
title: "生きているsessionの復旧job"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-triage
  - adr-0047
---

# 生きているsessionの復旧job

[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定39・40（task 360、task 441、`application::supervise::recovery`）。復旧jobを、生きているsessionのalertにも広げたもの。runtimeの自動修正（決定25・29など）が直さなかったものを、jobが状況を読んで許された操作で直し、直せないか自信が無いときだけ、今までそのalertがなっていたaskでinboxに上げる。終わったrun（`failed` / `interrupted` / `resume_exhausted`）は[Triage](triage.md#triage-supervisor)が同じverdictで扱い、receiptの無いidle（`stalled`）は[receiptの無いidleの検知](idle-without-receipt.md#receiptの無いidleの検知)のaskのまま（task 442が扱う）。

## alert

| alert | watch | 起きるとき | 許す操作 | escalateのask |
| --- | --- | --- | --- | --- |
| `long_background` | 最初のsession（`SessionWatch`）の、receiptの前で`/exit`を要求していないpoll | idle markerの`background_tasks`に`running`の処理があり、その処理が最初に載ったmarkerの時刻（`stats`と同じく`idle.log`から。task 331。読めなければmarkerのmtime）から`[stall].background_alert_secs`（既定1800秒）を超え、closeされていない`stalled`のaskが無い。1つのidle markerにjobは1回で、新しいmarkerか`wait`の再確認の時刻が来たときだけもう一度起動する | `stop_processes`、`send_instruction`、`wait` | `stalled`（`wait` / `intervene`） |
| `stuck_exit` | `SessionWatch`、判定の後の`/exit`（`ExitWatch`）、resumeのsession（`ResumeWatch`） | `/exit`が終了の時間切れ（`exit_timeout`）まで終わらず、既知のダイアログへの自動の応答（決定29）も当てはまらなかった。`ExitWatch`では、`/exit`がcmuxの時間切れで一度も届かず（`exit_unsent`）閉じて着地する条件（決定25、task 354）もそろわないときも | `answer_known_dialog`、`stop_processes`、`wait`。判定の後に着地するrun（`AfterExit::Land`）の`ExitWatch`では`close_and_proceed`も | `stuck_exit`（`exit` / `wait`） |
| `prompt_waiting` | `SessionWatch`（`watch_prompt`） | 画面に既知でないダイアログがあり（`prompt_waiting`を記録した後）、closeされていない`answer_prompt`のaskが無い。ダイアログが消えたら（`prompt_cleared`）走っているjobは止める | `answer_known_dialog`、`stop_processes`、`wait` | `answer_prompt`（optionsなし） |

- **回数と同時性**: 同じrunで同じalertのjobは`MAX_RECOVERY_ATTEMPTS`（3）回まで（`recovery_requested`の件数）で、使い切ったら起動せずにescalateにする。1つのwatchで同時に走るjobは1つで、別のalertはそのjobが終わるまで待つ。jobはsessionのslotの中で動き（run slotを増やさない）、sessionが終わったか次の段に進んだら止めて`recovery_finished`（`outcome: session_ended`、`prompt_waiting`はダイアログが消えたら`dialog_cleared`）を記録する。jobが走っている間、そのrunは人の答えの待ち（[ADR-0062](waiting.md)）に入らない。
- **起動**: `recovery_requested`（`alert`、`attempt`、`evidence`（`stuck_exit`は最新の`exit_request_timed_out`、`prompt_waiting`は最新の`prompt_waiting`のevent ID）、`workspace_id`とalertの事実。`long_background`は`idle_secs`、`background_since_ms`、`threshold`、`threshold_secs`、`background_tasks`、`marker_at_ms`。`stuck_exit`は`timeout_secs`、`ExitWatch`では`unsent`と`then`（`land` / `rest`）、`ResumeWatch`では`resume_attempt`。`prompt_waiting`は`prompt`、`screen_hash`、`excerpt`）を記録し、run directoryに`recovery-<alert>-<attempt>.prompt.txt`を書いて、終わったrunと同じ`headless_command`（読むtoolだけ、`DAGQ_ROLE=reviewer`）をそのdirectoryで起動する。stdout / stderrは`recovery-<alert>-<attempt>.out` / `.err`、timeoutはreviewと同じ。起動できなければすぐescalateにする。
- **prompt**（`prompt::recovery_prompt`）: taskのdescription・acceptance、alertの意味と事実、`capture`した画面の末尾、runのプロセスの一覧（pid、親pid、経過秒、cwd、command）、worktreeのHEADとreceiptの`commit`と`git status`、そのrunの過去の`recovery_finished` / `auto_repaired`、許された操作とverdictのschema、許されない操作の一覧。
- **runのプロセス**（`domain::recovery::run_processes`）: `ProcessControl::list`（`ps -U <uid> -o pid=,ppid=,etime=,command=`と、cwdは`/proc/<pid>/cwd`か`lsof -a -d cwd -u <uid> -Fpn`）の中で、cwdがrunのworktreeの下にあるか、sessionのwrapperの子孫のもの。wrapperとagent、それらの祖先（wrapperを開いたterminal）、supervisor自身とその祖先と子孫（worktreeで動くreview jobなど）、pid 1は含めない。wrapperかagentが登録されていなければ一覧を作らない（`stop_processes`は前提の崩れとしてescalate）。wrapperがsupervisorかその祖先（testのようにsupervisorのprocessがwrapperを兼ねる）なら子孫の規則は使わない。
- **verdict**（`domain::recovery::RecoveryVerdict`、未知のfieldは拒否）: `{"verdict": "repair" | "escalate", "confidence": "high" | "low", "diagnosis", "actions", "question", "options", "reason_category"}`。

## 適用

`confidence: high`の`repair`だけを適用する（`recovery::apply_live`）。先にすべての操作の前提を検査し、1つでも崩れていれば何もせずにescalateにする。

- 許す操作の外（上の表。`retry` / `retry_inherit` / `resume`は生きているsessionには当てはまらない）は崩れ。
- `stop_processes`: pidがすべてその時点のrunのプロセスである。適用の直前にもう一度一覧を取り直し、もうrunのプロセスでないpid（自分で終わった）は触らずに`gone`と記録し、残りにSIGTERMを送って、3秒の猶予の後に残ったものへSIGKILLを送る。止める途中の失敗はescalateにする（runは手放さない）。`stuck_exit`では適用の後にsessionへ終了の時間切れをもう一度与える。
- `send_instruction`: ダイアログが無く、idle markerが最後に打った文より新しい。決定31の確認付きで1回送る。
- `answer_known_dialog`（task 355の規則）: 画面に既知のダイアログがあり（`dialog`を名指したならそれ）、その段でキーをまだ送っていない（runtimeの`dialog_answered`かjobの`answer_known_dialog`の`auto_repaired`が無い。runtimeが条件の崩れで記録した`known_dialog_unanswered`はjobの再検査を妨げない）、ダイアログの条件がそろう（Background work is runningはsupervisorが`/exit`を打った後で、worktreeがcleanでreceiptがHEAD）。キーを送り、`auto_repaired`（`layer: recovery`、`repair: answer_known_dialog`、`dialog`、`keys`、`conditions`）を記録し、`stuck_exit`では終了の時間切れをもう一度与える。
- `close_and_proceed`（task 354の前提）: 判定の後に着地するrunで、receiptがworktreeに対してまだ成り立ち（commitがbranchのHEAD、worktreeがclean、evidenceとscope）、そのHEADがreviewの通ったcommit（`landable_without_exit`）。workspaceを閉じ、sessionが終わったときと同じく`workspace_closed`を記録して`answer_prompt`のaskを閉じ、着地に進む。閉じられなければescalate。
- `wait`: 何もしない。`recheck_after_secs`（上限3600秒）の後にalertが続いていればもう一度jobを起動する（alertの3回に数える）。

操作ごとに`auto_repaired`（`layer: recovery`、`repair`、`alert`、`attempt`と操作の記録。`stop_processes`なら止めた`processes`（pid、ppid、command、cwd、`killed`）、`send_instruction`なら`instruction`、`close_and_proceed`なら`workspace_id`と`conditions`。`wait`には書かない）を、最後に`recovery_finished`（`alert`、`attempt`、`verdict`、`confidence`、`diagnosis`、`applied`、`escalated: false`、`marker_at_ms`、`recheck_at_ms`、`duration_secs`）を記録する。

## escalate

`escalate`、`confidence: low`の`repair`、前提の崩れた`repair`、試行の使い切りは、inbox宛てのそのalertのaskにする（決定40のkind）。optionsはそのkindのものにjobの`options`を足したもの、`reason_category`はjobの`discard` / `scope`、それ以外は`recovery_failed`。questionは今までのaskの文面に、escalateの理由、`Why a person: <reason_category>`、jobの`diagnosis`、推奨の操作（jobの`actions`）、jobの`question`、`recovery-<alert>-<attempt>.prompt.txt`のpathを足したもの。`recovery_finished`（`escalated: true`、`why`、`reason_category`、`ask_id`）を記録する。

- `long_background`: `kind: stalled`のask。jobの間にreceiptの無いidleの検知の`stalled`のaskが開いていれば、そのaskに混ぜずに`recovery_finished`（`escalated: false`、`outcome: already_asked`）を記録するだけにする。askは[receiptの無いidleの検知](idle-without-receipt.md#receiptの無いidleの検知)の`StallWatch`が自分のaskと同じく扱う（sessionが動けば閉じ、`wait` / `intervene`を適用する）。その`stall_resolved`の`threshold`は`background_alert_secs`。
- `stuck_exit`: `kind: stuck_exit`のask（[receiptとsessionの終了](receipt-and-session-exit.md)）。`ResumeWatch`ではaskを開いた後、今までどおりsessionを`unresolved`として手放す。
- `prompt_waiting`: `kind: answer_prompt`のask（[ダイアログ](prompt-waiting.md)）。ダイアログが消えれば閉じる。

## jobの失敗

jobの失敗（起動できない、非0終了、timeout、verdictが無い）はaskにせず、決定40のとおり`recover by hand`のattentionにする（`recovery::fail_live`）。`recovery_finished`（`outcome: job_failed`、`error`、`reason_category: recovery_failed`）と`recovery_failed`（`code: job_failed`、`alert`、`attempt`、`error`、`workspace_id`、`reason_category: recovery_failed`）を記録し、sessionには触らない。`recovery_failed`は`ATTENTION_KINDS`のひとつで`event_attention`は`recover by hand`（`AttentionNext::RecoverByHand`）を返し、`status`はleaseのある生きているrunでも、未解消の`recovery_failed`（`domain::recovery::failed_live`）があればそのrunのattention（`kind: recovery_failed`、`reason_category: recovery_failed`、`last_error`はjobのerror、`last_error_code: job_failed`）を出す。解消するのは、その後に`session_exited`・`workspace_closed`・`supervision_finished`・`run_recovered`・`runtime_error`か、`prompt_waiting`なら`prompt_cleared`、`long_background`なら`receipt_observed`が記録されたとき。未解消の間、そのalertのjobはもう起動しない。`stuck_exit`はaskを作った後と同じく人を待つ（`ExitWatch` / `SessionWatch`はsessionの終了を待ち続け、`ResumeWatch`は今までどおりsessionを`unresolved`として手放す）。人はinboxから画面を読んで`dagq-recover`の手順で直す。

## 引き継ぎ

adoptしたsupervisorは、そのrunの`long_background`の最新の`recovery_finished`の`marker_at_ms`を起動済みのmarker、`recheck_at_ms`を再確認の時刻として読む。`stuck_exit`は、終了の時間切れが記録済みでaskが無ければ（`exit_asked`が偽）もう一度jobを起動する。前のsupervisorが走らせていたjobは引き継がず（試行の回数には数える）、前のjobの`claude -p`のprocessは止めない。supervisorの入れ替え（[handoff](handoff.md)）では、生きているsessionのjobを止めてから引き継ぐ。

receiptの形式は`src/domain/views.rs`の`Receipt`で、promptとREADMEに同じ契約を書いている。

```json
{"run_id": "...", "result": "succeeded | failed", "commit": "full SHA",
 "tests": {"status": "passed | failed | not_applicable", "evidence_or_reason": "..."},
 "e2e": {...}, "subagent_review": {...}, "summary": "..."}
```
