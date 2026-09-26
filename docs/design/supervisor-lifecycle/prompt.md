---
id: design-supervisor-lifecycle-prompt
type: design
title: "Prompt"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0009
  - adr-0038
  - adr-0029
---

# Prompt

`prompt.txt`は`src/application/prompt.rs`の`prompt(task, run, goal, predecessors, goal_predecessors, siblings)`（`runtime::prompt`として再公開）が生成するclaim時点のスナップショットで、run中にqueueが変わっても書き換えない。goalの`goal edit`も、兄弟taskの状態変化も、走行中のrunには届かず、次のclaimのpromptから反映される。task単体の情報（ID、run ID、title、description、acceptance、verification_commands）、receiptの契約（pathとJSONの形）に加えて、[ADR-0009](../../adr/0009-goal-groups-tasks.md)の次の4節をこの順で検証コマンドとreceiptの契約の間に載せる。どれも常に書き、該当がなければ`none`にして、promptの節構成をgoal・context・依存・並列の有無で変えない。

- **Goal**: taskに`goal_id`があれば、claim時点の`TaskStore::show_goal()`のgoalを`Goal ID` / `Goal title` / `Goal description` / `Goal acceptance` / `Goal constraints` / `Goal doc`の行で載せる。`doc`はrepository内のpathをそのまま書き、内容は読まない（なければ`Goal doc: none`）。goalのないtaskは`Goal: none, this task stands alone`。
- **Context**: taskの`context`が空白でなければ本文をそのまま載せ、空なら`Context: none`。
- **Predecessor tasks**: taskの直接の依存元（`task_dependencies`のpredecessor）ごとに1行、`- task <ID>: <title>; result commit <sha>; summary: <text>`。`result commit`は依存元の`integrated` runの`result_commit`（`integrate`がmainに積んだsquash commit）、`summary`はそのrunのreceipt（`receipt_path`。なければ`<run-dir>/receipt.json`）の`summary`（空白を1つに畳む。空なら`(no summary)`）。receiptが読めない・parseできない・pathが不明なら`(receipt unavailable)`、integrated runがなければ（手で`completed`にしたなど）`result commit (not landed)`と書き、いずれもprovisionを止めない。取得はqueueの読み取り専用操作`TaskStore::predecessors(task_id)`（ID順。依存元の`Task`と`integrated` runの`Option<TaskRun>`）で、summaryの読み取りはapplication側（`application::prompt::PredecessorSummary::from_predecessor`）が`RunFiles`越しに行う。task依存の行に続けて、taskのgoal依存（[ADR-0038](../../adr/0038-task-depends-on-a-goal-until-it-is-achieved.md)。claim時点ではすべて`achieved`で閉じている）ごとに`- goal <ID> (closed as achieved): <title>; its completed tasks:`を書き、その下にgoalの`completed`のtaskを1行ずつ`  - task <ID>: <title>; result commit <sha>; summary: <text>`で並べる（無ければ`  - none`）。行の作り方はtask依存と同じだが、goalはtaskが多くなりうるのでsummaryを`GOAL_TASK_SUMMARY_CHARS`（200文字）で切って`…`を付ける。取得は`TaskStore::goal_predecessors(task_id)`（goal ID順の`GoalPredecessor`: goalと、その`completed`のtaskの`Predecessor`のID順）、組み立ては`GoalPredecessorSummary::from_goal_predecessor`。依存元もgoal依存もなければ`Predecessor tasks: none`。
- **Sibling tasks in progress**: `TaskStore::tasks_in_progress()`が返す`in_progress`のtask（ID順）から自分のtaskを除き、taskにgoalがあれば同じ`goal_id`のtaskに限定したものを`- task <ID>: <title>`で並べる（`siblings_in_progress`）。goalのないtaskはgoalの有無を問わず全`in_progress` taskを見る。claimは`fill_slots`で1件ずつ順に行うので、同じpassで後にclaimされたtaskのpromptには先にclaimされたtaskが載り、その逆は載らない。`awaiting_integration`や`needs_session`のrunを持つtaskも`in_progress`なので載る。なければ`Sibling tasks in progress: none`。

冒頭（worktreeだけで作業する指示の直後）に、最初に読むものを`WORKER_READING`の一文に限定する: repository instructions（AGENTS.md）のworker節、この下のtask context（とそれが名指す文書）、goal doc、依存元のsummaryだけを読み、`dagq list` / `dagq show`は打たず、docs全体は読まず、他のファイルはtaskが必要とするときだけ開く（goal 11の決定4。runに要る情報はpromptに載っていて、queueの一覧やdocs全体を読むのは最初のcommitを遅らせるだけ）。

判断が要るときの手順も載せる: terminalに質問を書いて待つのではなく、worktreeで`dagq ask --run <run-id> --kind worker_question --question '...'`を打ち、短く報告して止まる。回答は`answer to ask <id>: ...`としてterminalに届く（[workerの質問への回答の送信](worker-question-answer.md#workerの質問への回答の送信)）。

末尾の「receiptを書いたら短く報告して止まる」の直前に`STOP_BACKGROUND`の一文を置く: receiptを書く前に、自分が起動したbackgroundの処理（`run_in_background`のshell、待ちループ、watchなど）をすべて止める。残っているとClaude Codeが`/exit`に「Background work is running — Exit and stop tasks / Move to background and exit / Stay」の確認画面を出して止まり、`exit_request_timed_out`になる（2026-09-23にtask 49・75・74・118で起きた）。同じ一文をresumeの定型の解消依頼（[`needs_session`](needs-session.md#needs_session)）にも手順4として載せる。残った確認画面は、`/exit`のexit timeoutでsupervisorが画面を読み、worktreeがcleanでreceiptの`commit`がHEADのときだけ「Exit and stop tasks」を選ぶ（ADR-0047の決定29。[既知のダイアログ](prompt-waiting.md#既知のダイアログ)）。条件がそろわなければ今までどおり`stuck_exit`のaskになる。

verification commandsは`Verification commands (integrate runs them once after rebasing onto main; that run is the verification of record for the commit):`の見出しで一覧を見せ、その直後に`local_checks`の一文を置く: worktreeで流すのはrepositoryの指示（AGENTS.mdかCLAUDE.md）がworkerに求める検証で、それはverification commandsの一部をintegrateに任せてよく、指示が何も求めないときはverification commandsを流す。同じ一文をresume（`evidence_missing`・`scope_violation`・`sent_back`・triage・rebase（`Landing`）・reviewのpass後の衝突（`Precheck`））とreviseの手順2にも載せ（そこではverification commandsをJSONの一覧で書く）、integrateのrebase（`Landing`）の手順2には「reasonがintegrateのrebase後に落ちた検証コマンドなら、そのコマンドを手元で流して再現して直してよい」を足す。retryが引き継いだrunの節も「上の検証を流し直す」と書く。runtimeはcargoやllvm-covなど特定のツールの名前を決め打ちしない（dagqは他のrepositoryでも動く。どの検証をintegrateだけに任せるかはrepositoryの指示が決める）（task 510）。新しいADRは作らない: [ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定1の「同じcommitのverificationはintegrateの1回が正」に沿ってpromptの文面を直すだけで、決定は変わらないため。

taskに`required_evidence`があれば、verification commandsの直後（4節の前）に`Required evidence: e2e, tests (each must be passed with evidence in the receipt, or the run waits for a session to add it)`の1行を載せ、workerに事前に知らせる（無ければ行ごと出さない）。taskに`paths`があれば、その次に`Paths you may change (globs from the repository root; ...): docs/**, *.md. A commit that changes any other path is not accepted: the run waits for a session to take it out. If the task needs another path, ask instead of changing it.`の1行を載せる（[ADR-0029](../../adr/0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)。無ければ行ごと出さない）。

4節の後に「担当はこのtaskだけ。兄弟taskの範囲を変えず、範囲外の仕事を見つけたら受け持たずにreceiptの`follow_ups`に書く」の一文を置き、receipt JSONの例に任意の`follow_ups`（`{title, description}`の配列。`Receipt::check`は配列であることだけを見る）を含める。

schemaとCLIは変えない。`tests/e2e.rs`のstubはpromptの1行目とreceipt pathの行だけを読み、`follow_ups`のないreceiptを書くので、節の追加に影響されない。
