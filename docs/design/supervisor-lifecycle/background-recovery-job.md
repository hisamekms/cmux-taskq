---
id: design-supervisor-lifecycle-background-recovery-job
type: design
title: "backgroundの処理が終わらないときの復旧job"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0047
---

# backgroundの処理が終わらないときの復旧job

[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定39・40（task 360、`application::supervise::recovery`）。triage jobを、生きているsessionのalertにも広げたもの。孤児のtest fixtureの根本修正（task 317）の後も、それ以外の原因でbackgroundの処理が終わらないときの受け皿にする。今はalertのうち`long_background`だけをこの経路で扱い、`failed` / `interrupted`は[Triage](triage.md#triage-supervisor)の今のverdict、receiptの無いidle（`stalled`）は[receiptの無いidleの検知](idle-without-receipt.md#receiptの無いidleの検知)のaskのまま。

- **対象と条件**: 最初のsession（`SessionWatch`）で、wrapperのheartbeatが有効で`/exit`を要求していない、receiptの前のpollごと（receiptの後はsession自身のbackgroundの処理の待ち（`resume_timeout`まで）が扱うので、走っているjobは止める）。idle markerの`background_tasks`に`running`の処理があり、その処理が最初に載ったmarkerの時刻（`stats`と同じく`idle.log`から。task 331。読めなければmarkerのmtime）から`[stall].background_alert_secs`（既定1800秒）を超え、そのrunにcloseされていない`stalled`のaskが無い。1つのidle markerにjobは1回で、新しいmarkerか、`wait`の再確認の時刻が来たときだけもう一度起動する。同じrunで`long_background`のjobは`MAX_RECOVERY_ATTEMPTS`（3）回までで、使い切ったら起動せずにaskにする（理由`recovery_failed`）。
- **起動**: `recovery_requested`（`alert: long_background`、`attempt`、`idle_secs`、`background_since_ms`、`threshold`、`threshold_secs`、`background_tasks`、`marker_at_ms`、`workspace_id`）を記録し、run directoryに`recovery-prompt-N.txt`を書いて、triageと同じ`headless_command`（読むtoolだけ、`DAGQ_ROLE=reviewer`）をそのdirectoryで起動する。stdout / stderrは`recovery-N.out` / `recovery-N.err`、timeoutはreviewと同じ。jobはsessionのslotの中で動き（run slotを増やさない）、sessionが終わるかvalidationに進んだら止めて`recovery_finished`（`outcome: session_ended`）を記録する。
- **prompt**（`prompt::recovery_prompt`）: taskのdescription・acceptance、alertの事実、`capture`した画面の末尾、runのプロセスの一覧（pid、親pid、経過秒、cwd、command）、worktreeのHEADとreceiptの`commit`と`git status`、そのrunの過去の`recovery_finished` / `auto_repaired`、許された操作とverdictのschema、許されない操作の一覧。
- **runのプロセス**（`domain::recovery::run_processes`）: `ProcessControl::list`（`ps -U <uid> -o pid=,ppid=,etime=,command=`と、cwdは`/proc/<pid>/cwd`か`lsof -a -d cwd -u <uid> -Fpn`）の中で、cwdがrunのworktreeの下にあるか、sessionのwrapperの子孫のもの。wrapperとagent、それらの祖先（wrapperを開いたterminal）、supervisor自身とその祖先と子孫（worktreeで動くreview jobなど）、pid 1は含めない。wrapperかagentが登録されていなければ一覧を作らない（`stop_processes`は前提の崩れとしてescalate）。wrapperがsupervisorかその祖先（testのようにsupervisorのprocessがwrapperを兼ねる）なら子孫の規則は使わない。
- **verdict**（`domain::recovery::RecoveryVerdict`、未知のfieldは拒否）: `{"verdict": "repair" | "escalate", "confidence": "high" | "low", "diagnosis", "actions", "question", "options", "reason_category"}`。`long_background`で許す操作は`stop_processes`（`pids`）、`send_instruction`（`instruction`）、`wait`（`recheck_after_secs`、上限3600）。ADR-0047の他の操作（`retry`、`resume`など）はschemaとしては読むが、生きているsessionのこのalertには当てはまらないのでescalateとして扱う。
- **適用**: `confidence: high`の`repair`だけを適用する。先にすべての操作の前提を検査し、1つでも崩れていれば何もせずにescalateにする（`stop_processes`はpidがすべてその時点のrunのプロセスであること、`send_instruction`はダイアログが無くidle markerが最後に打った文より新しいこと）。`stop_processes`は直前にもう一度一覧を取り直し、もうrunのプロセスでないpid（自分で終わった）は触らずに`gone`と記録し、残りにSIGTERMを送って、3秒の猶予の後に残ったものへSIGKILLを送る。止める途中の失敗はescalateにする（runは手放さない）。操作ごとに`auto_repaired`（`layer: recovery`、`repair`、`alert`、`attempt`、`stop_processes`なら止めた`processes`（pid、ppid、command、cwd、`killed`））を、最後に`recovery_finished`（`verdict`、`confidence`、`diagnosis`、`applied`、`escalated: false`、`marker_at_ms`、`recheck_at_ms`、`duration_secs`）を記録する。
- **escalate**: `escalate`、`confidence: low`の`repair`、前提の崩れた`repair`、jobの失敗（起動できない、非0終了、timeout、verdictが無い）、試行の使い切りは、inbox宛ての`kind: stalled`のask（optionsは`wait` / `intervene`にjobの`options`を足したもの）にする。questionはalertと経過、escalateの理由、`Why a person: <reason_category>`（jobの`discard` / `scope`、それ以外は`recovery_failed`）、jobの`diagnosis`、推奨の操作（jobの`actions`）、jobの`question`、`recovery-prompt-N.txt`のpathを持つ。jobの間にreceiptの無いidleの検知の`stalled`のaskが開いていれば、そのaskに混ぜずに`recovery_finished`（`escalated: false`、`outcome: already_asked`）を記録するだけにする。`recovery_finished`（`escalated: true`、`why`、`reason_category`、`ask_id`）を記録し、askは[receiptの無いidleの検知](idle-without-receipt.md#receiptの無いidleの検知)の`StallWatch`が自分のaskと同じく扱う（sessionが動けば閉じ、`wait` / `intervene`を適用する）。その`stall_resolved`の`threshold`は`background_alert_secs`。jobの失敗をattention（`triage by hand`）ではなくaskにするのは、生きているsessionでは人に届く経路をaskに1本にするため。askのschemaに理由の分類の列はまだ無い（ADR-0047の決定41の実装taskが入れる）。
- **引き継ぎ**: adoptしたsupervisorは、そのrunの`long_background`の最新の`recovery_finished`の`marker_at_ms`を起動済みのmarker、`recheck_at_ms`を再確認の時刻として読む。前のsupervisorが走らせていたjobは引き継がず、同じmarkerにもう一度jobを起動する（試行の回数には数える）。前のjobの`claude -p`のprocessは止めない。

receiptの形式は`src/domain/views.rs`の`Receipt`で、promptとREADMEに同じ契約を書いている。

```json
{"run_id": "...", "result": "succeeded | failed", "commit": "full SHA",
 "tests": {"status": "passed | failed | not_applicable", "evidence_or_reason": "..."},
 "e2e": {...}, "subagent_review": {...}, "summary": "..."}
```
