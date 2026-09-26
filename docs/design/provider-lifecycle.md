---
id: design-provider-lifecycle
type: design
title: Agent provider lifecycle
status: current
created: 2026-09-21
updated: 2026-09-26
last_verified: 2026-09-26
scope: provider
related:
  - adr-0004
  - adr-0040
  - adr-0048
  - adr-0027
  - design-supervisor-lifecycle
---

# Agent provider lifecycle

application層はagent providerの共通契約を使い、CLI引数や出力形式を直接扱わない。

```text
AgentProvider
  preflight()              -- 実装済み: 実行可能性の確認
  command(run, prompt)     -- 実装済み: wrapperが起動するコマンド
  resume_command(run)      -- 実装済み: needs_sessionのrunを同じ会話で開き直すコマンド
  headless_command(cwd, prompt, allowed_tools) -- 実装済み: observerのheadless job（runを持たない）
  review_command(run, prompt) -- 実装済み: supervisorのheadless review（stdoutがverdict JSON）
  review_timeout()         -- 実装済み: headless reviewの上限（既定600秒）
  assign_session_id(command, session_id) -- 実装済み: headless jobのsessionにruntimeが決めたsession_idを付ける（既定は何もしない）
  inspect / interrupt / collect_result  -- 後続

AgentSignals
  detect_prompt(screen)    -- 実装済み: 画面の末尾のダイアログのkind（trust / choice / confirm）
  screen_excerpt(screen)   -- 実装済み: askと`prompt_waiting`に載せる画面の末尾
  idle_hook(content)       -- 実装済み: idle markerの内容（background_running、evidenceに記録するhookのフィールド）
```

`AgentSignals`はsupervisorが生きているsessionのagentについて読むもの（画面とidle marker）で、形式がagent固有なのでproviderのadapterが実装する（Claude Codeは`src/infrastructure/claude.rs`）。applicationはkindの名前・画面の抜粋・background workの有無だけを受け取り、それがrunにとって何を意味するか（askにする、`/exit`を待つ）を決める。

コマンドを返すメソッドは`std::process::Command`ではなくapplicationの`CommandSpec`（program、引数、環境変数の設定と削除、cwdだけを持つ値。`Command`と同じ名前のbuilderを持つ）を返す。起動はapplicationの`Spawner` portが行い、標準入出力の行き先（wrapperの端末を継承、null、`<run-dir>`のファイル）は呼び出す側が`Streams`で決める。実装は`infrastructure::process::LocalSpawner`で、`CommandSpec`を`Command`に変えて子プロセスとして起動する（`infrastructure::process::command`。observerの`observe`もこれで`Command`にする）。こうしてsupervisorとsession wrapperのユースケース（`application::supervise` / `application::session`）はプロセスを直接扱わない（[supervisor-lifecycle](supervisor-lifecycle/supervise.md#supervise)）。

Claude Code adapter（`src/infrastructure/adapters.rs`）はworktreeをcwdにし、`--session-id`にrun IDを渡し、`--debug-file`をrun管理領域に置き、`--add-dir`でrun管理領域への書き込みを許可し、promptを位置引数で渡す。stdin/stdout/stderrはwrapperのTTYを継承する。permission modeは上書きしない。

加えて`command()`は`<run-dir>/claude-settings.json`を書いて`--settings`で渡す。内容は`Stop` hook 1件で、hookのstdin（イベントJSON）を`<run-dir>/idle.json`（`TaskRun::idle_marker_path`）へ一時ファイル + renameで書く。supervisorはこのmarkerをidle判定に使う（[supervisor-lifecycle](supervisor-lifecycle.md)）。`SessionEnd` hookは使わず、セッション終了はwrapperの終了コードで確認する。他のproviderは同じmarkerを自分の仕組みで書けばよく、書かなければ手動終了待ちになる。同じ設定に`autoMode.environment: ["$defaults"]`も入れ、auto modeの初回案内（Teach auto mode）を抑止する（[起動時のダイアログ](#起動時のダイアログ)）。

`review_command()`（[ADR-0040](../adr/0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)の決定2、[ADR-0027](../adr/0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)）はsupervisorが受理したrunをreviewさせる非対話のコマンドを返す。Claude Code adapterは`claude -p --debug-file <run-dir>/claude-review.log --add-dir <run-dir> --settings <run-dir>/claude-review-settings.json --allowedTools Read,Grep,Glob --disallowedTools Bash,Edit,Write,NotebookEdit -- <prompt>`をworktreeで起動する（worktreeは生きているworkerのsessionのものなので、reviewは読むだけ）。`claude-review-settings.json`は`autoMode.environment`だけで`Stop` hookを持たない: reviewの間もworkerのsessionは開いたままなので、reviewがidle markerを書くとsupervisorのidle判定（reviseの往復）を誤らせる。cmux workspaceは作らず、stdin / stdout / stderrはruntimeが繋ぐ（stdinはnull、stdoutとstderrは`<run-dir>/review-<attempt>.out` / `.err`）。runtimeは`review_timeout()`を過ぎたらkillし、stdoutの`{"verdict": "pass" | "revise" | "concern", "reasons": [..], "summary": ".."}`を読む（[supervisor-lifecycle](supervisor-lifecycle/review.md#review-supervisor)）。`headless_command()`と1つのportにしないのは、reviewがrunに属し、そのrun directoryの設定・debug file・`--add-dir`と禁止するtoolを要るのに対し、observerのjobにはrunが無いため。reviewの子プロセスにはruntimeが`DAGQ_ROLE=reviewer`と`DAGQ_QUEUE`を渡し、CLIはreviewerに読むコマンドだけを許す。print mode（`-p`）はfolder trustの判定を飛ばす（[binary から読める判定](#binary-から読める判定)の1）ので、reviewはtrust dialogで止まらない。

headlessのjob（review・triage・plan review・observer）のsessionには、runtimeが起動前にUUIDを作って`assign_session_id()`で付ける（[ADR-0048](../adr/0048-record-claude-sessions-by-kind-with-open-and-active-time.md)の決定4）。Claude Code adapterは`--session-id <uuid>`をoptionの最後（promptの前の`--`の前）に足す。jobのtranscriptはjobが終わる前から`<session_id>.jsonl`として特定でき、`-p`のstdout（verdictのJSON）の形は変わらない。session_idはjobを始めるevent（`review_started`・`triage_started`・`plan_review_started`・`observe_started`）のpayloadの`session_id`に記録し、同じ値がjobのsessionの区間（`session_opened`）に載る。reviewとobserverはsupervisorとobserverが、triageとplan reviewはその開始を記録するqueueが作る。jobのsettingsにhookは足さない。session_idを付けられないproviderは既定の実装（何もしない）のままでよく、区間は`session_id`を持つが、transcriptと突き合わせられない。

## Claude sessionの区間

runtimeが起動するClaude sessionは、kindごとの区間（`session_opened` / `session_closed`のrun_events）として記録する（[ADR-0048](../adr/0048-record-claude-sessions-by-kind-with-open-and-active-time.md)の決定1・2・7）。区間は、それを開始・終了するeventを書く同じトランザクションで、同じ時刻に書く。書くのは`infrastructure::sqlite::event`（taskとrunのevent）と`record_queue_event`（queueのevent）の後に呼ぶ`infrastructure::sessions::follow`だけで、どの区間を開き閉じるかは`domain::sessions::changes`（純粋関数）が、eventのkindとpayload、同じ範囲の開いている区間、runの文脈（worktree、run directory、workspace、`resume_started` / `revise_requested`の数）から決める。

| kind | 開く | 閉じる（`reason`） | session_id |
| --- | --- | --- | --- |
| `worker` | `resume_started`の無いrunの`agent_started`、送れなかった最初のreviseの`revise_unsent` | `revise_requested`（`next_span`）、`session_exited`（`exited`） | `agent_started`の`session_id`（run ID） |
| `resume` | `resume_started`の後の`agent_started` | 同上 | 同上 |
| `revise` | `revise_requested`、送れなかった2回目以降のreviseの`revise_unsent` | 次の`revise_requested`・`revise_unsent`（`next_span`）、`session_exited`（`exited`） | 閉じた`worker` / `resume` / `revise`の区間のもの |
| `review` | `review_started` | `review_finished` / `review_failed` / `review_retried`（`job_finished`） | `review_started`の`session_id` |
| `triage` | `triage_started` | `triage_finished` / `triage_failed`（`job_finished`） | `triage_started`の`session_id` |
| `plan_review` | `plan_review_started`（proposalの最初のtaskのevent） | 同じ`plan_review_id`の`plan_review_finished` / `plan_review_failed`（`job_finished`） | `plan_review_started`の`session_id` |
| `observer` | `observe_started`（taskの無いevent） | 同じ`dir`の`observe_finished`（`job_finished`） | `observe_started`の`session_id` |

- **推定で閉じる（`inferred`）**: runのsessionの区間（`worker` / `resume` / `revise`）は、`session_exited`の無いまま次の`agent_started`が来たとき、`workspace_closed`・`run_recovered`・`triage_started`で閉じる（`run_recovered`と`triage_started`は開いている`review`も閉じる）。`record_runtime_event`はeventと区間を1つの書き込みトランザクション（`BEGIN IMMEDIATE`）で書く。jobの区間は、終わりのeventの無いまま同じkindの次の開始（別のsupervisorが引き継いだreviewの`review_started`、triageの`triage_started`、次の`observe_started`）で閉じ、plan reviewは行を`interrupted`で閉じたとき（supervisorが居ない行は`inferred`、proposalが動いたときは`job_finished`）に閉じる。時刻は、transcriptが読めればその最後のレコードの時刻（区間の開始と閉じたeventの時刻の間に収める）、読めなければ閉じたeventの時刻。
- `session_opened`のpayloadは`kind`、`session_id`、`cwd`（runのsessionとreviewはworktree、triageはrun directory、observerは観測のdirectory、plan reviewは`plan_review_started`の`cwd`＝jobを起動したrepositoryのcheckout。`cwd`を持たない過去の`plan_review_started`からの区間はnull）、`transcript_path`（null）、`attempt`、`workspace_id`（`worker`はrunのworkspace、`revise`は`revise_requested`のもの）、plan reviewは`proposal_id`・`plan_review_id`・`goal_ids`（その時のproposalのtaskのgoal）。`session_closed`は`opened_event_id`・`kind`・`session_id`・`reason`と、稼働時間を記録したか（`active`: `recorded`なら`active_secs`、`unavailable`なら理由のコード`active_unavailable`。下の[transcriptと稼働時間](#transcriptと稼働時間)）。runのsessionの区間（`worker` / `resume` / `revise`）は、transcriptが読めれば作業の内訳`work`も持つ（下の[作業の内訳](#作業の内訳)）。どの区間も、transcriptが読めれば区間のトークン数`tokens`を持つ（下の[トークン数とコスト](#トークン数とコスト)）。1つの区間は1回だけ閉じる（閉じた区間への2回目の終了は何も書かない）。
- inbox・planner・runtimeが立てるplannerの区間（hook）は後続taskが足す。queueのeventとして`session_opened` / `session_closed` / `session_turns`を書けるよう、migration 0035がrun_eventsのCHECKにこの3つを足した（breaking）。このADRが入る前のrunには区間が無く、埋め直さない。

Claude providerはcmux内の通常セッションを起動し、実装、unit test、E2E、subagent review、完了レポートを実行させる。Codex providerはCodexの対応するセッション方式を使う。provider capabilityとしてinteractive、subagents、stream events、structured resultを表現する。

requested providerとactual providerをTaskRunに保存する。Claudeが起動不能の場合はCodexへfallbackできるが、実装途中の一般的な失敗は自動fallbackしない。


### transcriptと稼働時間

区間の稼働時間は、その区間に属するClaude Codeのtranscriptのturnの長さの合計（[ADR-0048](../adr/0048-record-claude-sessions-by-kind-with-open-and-active-time.md)の決定5・8・9）。

- **読む場所は1か所**: transcriptの場所と形式を知るのは`infrastructure::transcripts::ClaudeTranscripts`（applicationのport`Transcripts`の実装）と、行をレコードとturnにする`domain::transcript`（純粋関数）だけ。task 199のトークン数も同じportの`TranscriptRecord::usage`（`assistant`の`message.usage`）を読み、transcriptを自分では開かない。形式が変わったら直すのはこの2つだけ。
- **場所**: 区間の`transcript_path`（記録があれば）、無ければ`$CLAUDE_CONFIG_DIR`（無ければ`~/.claude`）の`projects/<cwdの英数字以外を-にした名前>/<session_id>.jsonl`、それも無ければ`projects/*/<session_id>.jsonl`。読むのは区間を閉じるprocess（wrapper・supervisor・CLI）とsupervisorで、どれも同じhostの同じuserで動く。
- **レコード**: `type`・`timestamp`・`sessionId`を持つ行だけを使う。実際の入力は`type: user`のうち、sidechainでなく、`isMeta`でなく、中身が`tool_result`だけでなく、`isCompactSummary`でないもの。`assistant`と`tool_result`だけの`user`が出力。
- **turn**: 実際の入力から、次の実際の入力の前の最後の出力まで（出力が無ければ0秒）。tool・subagent（sidechain）の時間はturnに入り、sidechainの入力はturnを区切らない。turnは入力の時刻が入っている区間に属し、区間の終わりで切る。
- **読めない**: `Transcripts::read`は、session_idが無い（`session_unknown`）、ファイルが無い（`transcript_missing`）、どの行もJSONでない・UTF-8でない（`transcript_unparsable`）、必要なフィールドを持つレコードが無い（`transcript_unsupported`）、`sessionId`が区間のものと違う（`session_mismatch`）とき`Unreadable`を返す。JSONでない行（書きかけの最後の行など）は飛ばし、数をtracingに書く。
- **reviseの切り替え**: reviseの文面はtask 241より前は`revise_requested`を書く前にsessionへ送っていた（今は送る直前に`sent_at`を取って記録してから送る）ので、`revise_requested`が閉じる区間の`session_closed`と、開く`revise`の区間の`session_opened`は、payloadの`sent_at`（送った時刻、秒）の時刻で書く（`revise_requested`より前のときだけ）。これでreviseの入力のturnは`revise`の区間に属する。送れずに`revise_unsent`で取り消したreviseは、`revise`の区間を閉じ、その前のsessionの区間（送った前のreviseがあれば`revise`、無ければ`resume`か`worker`）を同じ`session_id`で開き直す。`revise`の`attempt`は`revise_unsent`で取り消した分を数えない。
- **閉じるとき**: `sessions::close`は区間のtranscriptを読み、まだ記録していないturn（前の`session_turns`の`through`より後に始まったもの。区間の終わりで切る）を`session_turns`（`opened_event_id`・`kind`・`session_id`・`turns: [[開始, 終了], ...]`・`through`（最後のturnの終わり））として書き、記録済みのturnと合わせた秒を`session_closed`の`active_secs`にする。読めなければ`session_turns`を書かず、`active: unavailable`とコードを書き、理由とClaude Codeのversionを`info`のtracingに書く。どちらでも区間を閉じたevent・run・jobの結果は変わらない。
- **開いている間**: supervisorはループの各passで、前回から`SESSION_TURNS_INTERVAL`（10分）経っていれば、またobserverを起動する前に、`RunStore::record_session_turns`で開いている全区間の完了したturn（次の入力が来たもの）を`session_turns`として区間と同じtask・runに書く。transcriptは書き込みのロックの外で読み、区間ごとに`BEGIN IMMEDIATE`の中で、まだ開いているかと記録済みのturnを確かめ直してから書く（同時に区間を閉じたwrapperと同じturnを二重に書かない）。進行中のturnは書かない。読めない・書けないときはtracingにだけ書き、次の回に読み直す。
- **headlessのjob**: runtimeが`--session-id`を付けるので、reviewとtriageとplan reviewとobserverのtranscriptも同じ規則で読む。session_idを付けられないprovider（Codex）の区間は`session_unknown`か`transcript_missing`で稼働時間を記録しない（出力形式から取る値は今は使わない。トークン数も同じ）。
### 作業の内訳

runのsessionの区間（`worker` / `resume` / `revise`）は、閉じるときに稼働時間と同じtranscriptから作業の内訳を記録する（task 514）。分類の規則は`domain::worktime`（純粋関数。spikeの`worktime.py`を移した）、transcriptの読み取りは上と同じ`domain::transcript`（`TranscriptRecord`の`tool_uses`・`tool_results`・`notification`）。hook（PreToolUse / PostToolUse）は使わない（backgroundのコマンドの終わりが取れないため）。

- **区間**: 区間の開始から終わりまでの各時刻に、優先度の高い順に1つの分類を付ける: foregroundのtool > backgroundのコマンド > subagent > model > idle。foregroundのtoolは`tool_use`から対の`tool_result`まで。backgroundのコマンド（`run_in_background`）とasyncのsubagentは、`tool_use`から完了通知（`queue-operation`か`user`の`<task-notification>`の`<tool-use-id>`が同じもの）まで。foregroundで始めてClaude Codeがbackgroundに移したshellのコマンドも、通知があれば通知までのbackgroundのコマンドにする。通知の無いasyncのsubagentは起動の結果で終わったものとして数える。modelは、main sessionの（sidechainでない）`user` / `assistant`のレコードから、次の`assistant`のレコードまで。どれにも当たらない時間は`idle`。区間の中で始まった`tool_use`だけを数えるので、同じsession_idを続けるresumeとreviseは前の区間のコマンドを取らない。終わりの見えないもの（transcriptが途中で切れた、通知の前にsessionが終わった）は区間の終わりで切り、`finished: false`にする
- **コマンドの分類**: shellのコマンドは`&&`・`||`・`;`・改行で分けた部分のうち最も重いもので決める（`llvm_cov`などcargoのものはcargoのsubcommandで見るので、文字列を含むだけの`grep`や`git commit`は数えない）（`e2e` > `llvm_cov` > `test` > `build` > `fmt` > `wait` > `dagq` > `git` > `other_command`）。重いもの（`e2e` / `llvm_cov` / `test` / `build`）が2種類以上あれば`chain`。shell以外のtoolは`tool`（ScheduleWakeup / Monitor / TaskOutput / BashOutputは`wait`）、`Agent`は`subagent`
- **eventのpayload**: 集約を`session_closed`の`work`（`{total_secs, secs, commands, verification_repeats, full_tests, llvm_cov_runs, heavy}`。`heavy`は`timeline`の重いコマンドの行の元）に書き、同じものに区間の`kind`と`attempt`を足して`session_exited`の`work_breakdown`（その終了が閉じた区間のもの）と`resume_finished`の`work_breakdown`（その`resume_started`の後に開いて閉じた`resume`の区間のもの。reviewに進むためsessionを開いたままにした、終了を待ちきれなかったなど、区間が`resume_finished`より前に閉じていなければ載らず、区間の`session_closed`の`work`にだけ残る）に載せる。eventには値だけを入れ、コマンドの全文やpathは入れない
- **明細**: コマンドごとに`{opened_event_id, kind, attempt, tool, category, start, end, secs, finished, background, exit_code, failed, status, command}`（`command`は先頭300バイト）をrun directoryの`worktime.jsonl`に1行ずつ追記する。書けなくてもtracingに書くだけ。追記は区間を閉じるトランザクションの中で行うので、そのトランザクションが失敗して書き直されると同じ行が重なりうる（`opened_event_id`で見分ける）
- **読めないとき**: transcriptが読めなければ（上のコード）`work`も`work_breakdown`も書かない。理由は稼働時間と同じtracingに出る。区間を閉じたevent・run・jobの結果は変わらない

### トークン数とコスト

どのkindの区間も、閉じるときに稼働時間と同じtranscriptから、区間で使ったトークン数を記録する（task 199）。runのsession（`worker` / `resume` / `revise`）もheadlessのjob（`review` / `triage` / `plan_review` / `observer`）も同じ規則で読むので、jobの出力形式（`-p`の出力）は変えず、出力の読み取りにも手を入れない。集計の規則は`domain::tokens`（純粋関数）、transcriptの読み取りは上と同じ`domain::transcript`（`TranscriptRecord`の`usage`・`message_id`・`cost_usd`）だけが知る。

- **数え方**: 区間の開始から終わりまで（`[開始, 終わり)`）の`assistant`のレコードの`message.usage`の`input_tokens`・`output_tokens`・`cache_read_input_tokens`・`cache_creation_input_tokens`を足す。Claude Codeは1つのmessageの内容のblockごとにレコードを書き、どれにも同じmessageの`usage`を載せるので、`message.id`が同じレコードは1つのmessageとして1回だけ数え、種類ごとにそのレコードの値の最大（最後のblockが最終の`output_tokens`を持つ）を取る。`message.id`の無いレコードはそれぞれ1つのmessageにする。sidechain（subagent）のレコードも数える。同じsession_idを続けるresumeとreviseは時刻で分かれるので、前の区間のmessageを取らない
- **コスト**: Claude Codeがレコードに書いた`costUSD`がある版だけ、数えたmessageのすべてに`costUSD`があるときにその合計を記録する。単価表からは計算しない（2.1系のtranscriptは`costUSD`を書かないので、今は記録されない）
- **eventのpayload**: `session_closed`の`tokens`（`{input, output, cache_read, cache_creation, messages}`と、あれば`cost_usd`。小数6桁）。runのsessionの区間は同じものを`session_exited`の`tokens`（その終了が閉じた区間のもの）と`resume_finished`の`tokens`（`work_breakdown`と同じく、その`resume_started`の後に開いて閉じた`resume`の区間のもの）にも載せる。区間に`assistant`のレコードが無ければ0を記録する
- **読めない版**: transcriptが読めなければ（上のコード）記録しない。読めても、区間の`assistant`のレコードの`usage`に数値の`input_tokens`と`output_tokens`が無いか、`assistant`のレコードがあるのにどれも`usage`を持たなければ、その版の形式を知らないものとして`usage_unsupported`とし、`tokens`を書かない。どちらも理由とClaude Codeのversionを`info`のtracingに書くだけで、区間を閉じたevent・run・jobの結果は変わらない
- Claude Code自身のtelemetry（OTLP）とは別の経路で、ここではOTLPを使わない

## Trust prompt

Claude Code の folder trust dialog（`Quick safety check: Is this a project you created or one you trust?` / `Yes, I trust this folder`、既定の選択は `No, exit`）が run worktree で出るかどうかは、**worktree の親 repository（`git rev-parse --git-common-dir` の親）の root が `~/.claude.json` の `projects` に `hasTrustDialogAccepted: true` で記録されているか**で決まる（通常の対話起動の場合。下の判定 1 と 3 の例外は run worktree には当てはまらない）。worktree の置き場所（scratch でも XDG data dir でも）、adapter が渡す `--session-id` / `--debug-file` / `--add-dir` / `--settings`、`--dangerously-skip-permissions` は無関係。2026-09-22 の実機（cmux 0.64.25、Claude Code 2.1.278）では、使い捨て repository の故障経路のスモーク（[manual-smoke](manual-smoke.md)）で全 run session が dialog で止まり、この repository のドッグフーディングでは一度も出なかった。前者は使い捨て repository を root で一度も開かずに supervise を始めたから、後者は `~/ghq/github.com/hisamekms/dagq` が既に信頼済みだったからで、runtime の挙動は同じだった。

### 実験（2026-09-22、Claude Code 2.1.278、macOS）

起動は Python の `pty.fork` で `~/.local/bin/claude`（`versions/2.1.278` への symlink）を cwd を変えて起動し、`TERM=xterm-256color`、120x40、親（この run session）から継承する `CLAUDE*` 環境変数は削除した（supervise が cmux workspace の login shell から起動するのと同じ条件）。画面に dialog か通常の入力 UI（`Try "..."` / `auto mode on`）が出るまで最大 30 秒待ち、dialog が出たら Esc で終了（decline）か Down + Enter で承認して `/exit`（accept）した。dialog は model を呼ぶ前に出るので token は消費しない。承認の前後で `~/.claude.json` の `projects` のうち `hasTrustDialogAccepted` が真の key を比較した。

準備:

```sh
git init -q repo-B && git -C repo-B commit -q --allow-empty -m seed          # repo-D、repo-E も同じ
git -C repo-B worktree add -q ../wt-B1 -b wt-b1                              # scratch 配下
git -C repo-B worktree add -q ~/.local/share/taskq-trust-probe/runs/c1/worktree -b wt-c1   # XDG data dir 配下
git -C repo-D worktree add -q ~/.local/share/taskq-trust-probe/runs/d1/worktree -b wt-d1   # d2、repo-E の e1 も同じ
# 「adapter flags」= adapter と同じ引数。claude-settings.json は Stop hook で <run-dir>/idle.json を書く 1 件
claude --session-id <uuid> --debug-file <run-dir>/claude.debug.log --add-dir <run-dir> --settings <run-dir>/claude-settings.json
```

| # | cwd | 引数 | dialog | 備考 |
| --- | --- | --- | --- | --- |
| A | git でない新規 directory（scratch） | なし | 出る | |
| B1 | `repo-B` root（未信頼） | なし | 出る | |
| B2 | `wt-B1`（未信頼 `repo-B` の worktree、scratch） | なし | 出る | |
| B3 | `repo-B` root で承認 | なし | 出る → 承認 | `projects[<repo-B root>].hasTrustDialogAccepted: true` だけが追加される |
| B4 | `wt-B1`（B3 の後） | なし | 出ない | |
| C1 | `~/.local/share/taskq-trust-probe/runs/c1/worktree`（信頼済み `repo-B` の worktree） | なし | 出ない | |
| C2 | 同上 | adapter flags | 出ない | |
| D1 | `.../runs/d1/worktree`（未信頼 `repo-D` の worktree、XDG data dir） | なし | 出る | |
| D2 | 同上 | adapter flags | 出る | `--add-dir` / `--settings` は抑止しない |
| D3 | 同上で承認 | なし | 出る → 承認 | 追加される key は worktree の path ではなく `<repo-D root>` |
| D4 | `.../runs/d2/worktree`（`repo-D` の別 worktree、D3 の後） | なし | 出ない | |
| D5 | `repo-D` root（D3 の後） | なし | 出ない | |
| E | `.../runs/e1/worktree`（未信頼 `repo-E` の worktree） | `--dangerously-skip-permissions` | 出る | permission mode は trust dialog を飛ばさない |

B3 / D3 で `~/.claude.json` に残った `projects` の key は repository root の path だけで、worktree の path は一度も書かれない。D3 → D4 / D5 は上の故障経路のスモークの観測と整合する: 並列に起動した 2 つの run session は両方 dialog で止まり、片方で承認すると repository root が信頼済みになるので、その後に起動した session は止まらない。

### binary から読める判定

`strings ~/.local/share/claude/versions/2.1.278` の minified JS で、trust 判定は次の順（関数名は minified のもの）。

1. `CLAUDE_CODE_SANDBOXED` が設定されている、session 内で既に承認済み（`sessionTrustAccepted`）、または print mode（`-p`）なら trusted（実験では未確認）
2. cwd の「project key」= git root。linked worktree は `.git` file から common dir を辿った **canonical root**（親 repository の root）に解決される。`projects[key].hasTrustDialogAccepted` が真なら trusted
3. そうでなければ cwd から親 directory を 1 段ずつ上がり、途中の directory が `projects` で信頼済みなら trusted。上がるのは git root（linked worktree ではその worktree の root）まで、git repository の外では `/` まで
4. 承認時に書く key も同じ canonical root（`oV(Xw(cwd))`）

つまり worktree が repository の外にあっても親 repository の信頼が効き、逆に worktree を repository の中（`.worktrees/` など）に置いても親 repository が未信頼なら出る。

### 推奨する後続

1. 運用手順として文書化する（runtime 変更なし、推奨）: ある repository で初めて `supervise` を流す前に、その repository の root で `claude` を一度起動して dialog を承認する（または `~/.claude.json` の `projects[<root>].hasTrustDialogAccepted` が真であることを確認する）。使い捨て repository のスモーク（[manual-smoke](manual-smoke.md#隔離)）も root を先に信頼する。task 16 で当時の plugin skill（名前は `taskq-run`、task 100 で退役した常駐 session 用の skill の前身）の「every run's worktree is a directory Claude Code has never seen」という誤った本文をこの条件に書き換えた
2. ~~adapter の `preflight()` で `~/.claude.json` を読み、repository root が未信頼なら warning（event か stderr）を出す~~ → task 92 で `up` の preflight にした（warning ではなく error で止める。[起動時のダイアログ](#起動時のダイアログ)）。dialog を抑止する CLI flag は 2.1.278 / 2.1.280 にはないので adapter flag では解決できず、runtime が `hasTrustDialogAccepted` を書き込むのはユーザーの判断を代行することになるので採らない
3. 未信頼の repository で `supervise` を始めてしまった場合の扱いは、task 16 で plugin の skill に入れた（task 65 で run の session 用の skill に分割、task 100 で `dagq-recover` の `reference/session.md` に移した）: 最初の承認より前に起動した session（最大 `--parallel` 件）はすべて dialog で止まる。supervisor はそれぞれを `answer_prompt` の ask として inbox に上げ、人の指示で `send-key down` + `enter` を送る。承認後に起動した session には出ない

## 起動時のダイアログ

2026-09-23 に worker が起動直後に止まったダイアログは trust（5 件）、LSP plugin の推奨、auto mode の初回案内（Teach auto mode）の 3 種類。Claude Code 2.1.280 の binary（`strings ~/.local/share/claude/versions/2.1.280` の minified JS。関数名は minified のもの）から、settings.json と CLI flag で抑止できるかを調べた。結果と runtime の扱い:

| ダイアログ | 表示の条件（2.1.280） | settings / flag で抑止 | runtime の扱い |
| --- | --- | --- | --- |
| folder trust | [Trust prompt](#trust-prompt) のとおり repository root の `projects[<root>].hasTrustDialogAccepted` | できない（flag なし。`--dangerously-skip-permissions` も効かない。`CLAUDE_CODE_SANDBOXED` は sandbox を偽ることになるので使わない） | `up` の preflight で検査して、未信頼なら案内付きの error で止まる |
| Teach auto mode（`Teach auto mode about your environment?`） | `xP()`: auto mode の gate が有効、**settings の `autoMode.environment` が空**、`numStartups >= 5`、`autoModeEnvSetup.denials >= 5`、`dismissed` でなく `dismissedAt` から 7 日（`dnt=604800000`）経過 | できる: `autoMode.environment` が 1 件以上あれば出ない。読むのは `userSettings` / `flagSettings` / `policySettings`（`qwe`）で、`--settings` で渡す run の設定は `flagSettings` | run の `claude-settings.json` に `"autoMode": {"environment": ["$defaults"]}` を書く。`$defaults` は組み込みの environment をその位置に継承するので classifier の挙動は変わらない（`claude --settings <この設定> auto-mode config` の実効 `environment` は設定なしと同じ 21 件で、`$defaults` は展開される。2026-09-23 に確認） |
| LSP plugin の推奨（`LSP plugin recommendation` / `Would you like to install this LSP plugin?`） | `rno()`: global config（`~/.claude.json`）の `lspRecommendationDisabled` が真か `lspRecommendationIgnoredCount >= 5`（`tno=5`）なら出ない。それ以外は session で開いたファイルの拡張子に合う LSP plugin が marketplace にあれば session に 1 回 | できない: 判定は global config だけを見て、settings.json の key も CLI flag も無い（`--bare` は LSP を切るが settings の hook も切るので Stop hook が動かない） | 何もしない。ダイアログは 30 秒（`s$e=30000`）応答が無ければ `timeout` で閉じて `lspRecommendationIgnoredCount` を 1 増やすので、止まるのは最長 30 秒で、5 回無視されると以後出ない（この machine は 2026-09-23 時点で既に 5）。runtime が `~/.claude.json` を書くのはユーザーの設定を代行するので採らない。止めたいユーザーは推奨の `Disable all LSP recommendations` を選ぶ |

- trust の判定は `$CLAUDE_CONFIG_DIR/.claude.json`（未設定なら `~/.claude.json`）の `projects` を、main checkout の root（`git rev-parse --git-common-dir` の親。`up` を linked worktree から打っても同じ key になる。common dir が `.git` でない配置では `--show-toplevel`）の path で引く（`claude_trusts_repository`）。親 directory の信頼は見ない（[binary から読める判定](#binary-から読める判定)の 3 のとおり、Claude Code も git root より上は辿らない）。config が無い、HOME も `CLAUDE_CONFIG_DIR` も無い、key が無い、`hasTrustDialogAccepted` が `true` でない、のどれも未信頼として `up` を止める。parse できない config は別の error。`supervise` 自体は検査しない（`up` を経ない起動は従来どおり dialog で止まり、`prompt_waiting` になる）
- 抑止したダイアログは task 101 の `prompt_waiting` の検知とは重ならない: 検知は画面の兆候を見るだけで、出なくなったダイアログは検知されないだけ。goal 11 の受け入れ条件「`prompt_waiting` が trust 以外で出ない」は、LSP の推奨が 30 秒で閉じる（検知は 90 秒後から）ことと Teach auto mode の抑止で満たす

