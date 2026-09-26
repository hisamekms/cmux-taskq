---
id: design-supervisor-lifecycle-prompt-waiting
type: design
title: "ダイアログ待ちの検知"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
---

# ダイアログ待ちの検知

receiptより前にClaude Code自身のダイアログ（folder trust、LSP pluginの推奨、auto modeの案内など）で止まったsessionは、`Stop`が発火せず画面も変わらないまま待ち続ける。supervisorは画面を読んでこれを検知し、記録してinbox宛てのaskにする（応答は人）。キーを送るのは下の[既知のダイアログ](#既知のダイアログ)だけで、それ以外のダイアログにはキーを送らない（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定29。ADR-0019の決定6の「キーは送らない」を改めた）。

- **条件**: agentの登録（`agent_started`）をそのsupervisorが最初に見てから`WorkspaceBackend::prompt_wait`（cmuxは90秒）以上経ち、receiptも`idle.json`もなく、closeされていない`worker_question`もなく（askで止まったworkerは回答を待っているのでダイアログ待ちではない）、wrapperが生きて（未終了でheartbeatが有効）agentのPIDも生きているrun。画面の読み取りは`WorkspaceBackend::capture`で、多くとも10秒に1回（`prompt_wait`がそれより短ければその間隔）。引き継いだrunは引き継いだ時点から数える。読み取りの失敗はsupervisor logに書くだけでrunには影響させない。
- **判定**: `infrastructure::claude::detect_prompt(screen) -> Option<PromptKind>`（純粋関数。`runtime`から再公開。supervisorは`AgentSignals::detect_prompt`越しにkindの名前だけを受け取る）が、画面の空行を除いた末尾30行について、枠線（`│`など）と前後の空白を除いた行で見る。`Do you trust`で始まる行か`trust this folder`を含む番号付き選択肢があれば`trust`、`❯`で始まる番号付き選択肢（`❯ 1. …`）の前後3行以内にも番号付き選択肢があれば（選択肢の文が折り返しても）`choice`、`Enter to confirm`か`Esc to cancel`で始まる行があれば`confirm`。文中や引用の中の同じ文言（作業中の出力やコード）は行頭にないので数えない。
- **記録**: 兆候があれば`prompt_waiting`（`workspace_id`、`excerpt`=空行を除いた末尾15行、`screen_hash`=excerptのSHA-256、`prompt`=判定の種類）を記録してlogに書く。同じ`screen_hash`の間は再記録せず、別のダイアログに変わればもう一度記録する。記録した後に兆候が消えるか、idle markerが書かれるかagentのPIDが死ねば`prompt_cleared`（`workspace_id`）を記録する（receiptが来たときは`receipt_observed`がダイアログを終わらせるので記録しない）。引き継いだrunは最後の`prompt_waiting`（その後に`prompt_cleared` / `receipt_observed`が無いもの）の`screen_hash`を引き継ぎ、同じ画面を再記録しない。記録済みのダイアログがある間は`prompt_wait`を待たずに読み取りの間隔で画面を読むので、supervisorの不在中に応答されたダイアログもすぐ`prompt_cleared`になる。
- **ask**（task 100）: `prompt_waiting`を記録するたびに、supervisorは`kind: answer_prompt`のaskをinbox宛てに開く（`ask_answer_prompt`。`asked_by: supervisor`、taskとrunに紐づき、optionsは無し。questionはrun id・task id・ダイアログの種類・workspaceのUUIDと、「答えはそのworkspaceで入力され、ダイアログが消えればこのaskは自分で閉じる」旨、末尾に`excerpt`）。askの登録なので`cmux notify`が1回inboxへ飛ぶ。画面が変わっても（別のダイアログ、時刻の表示の更新など）openなaskはそのまま残し（同じrunとkindのopenなaskが返るので通知は1回だけ）、`prompt_cleared`、`receipt_observed`、sessionの終了、triageのときにaskを閉じる（`close_answer_prompt_asks`。未回答なら`the dialog is gone; closed by the runtime`などを答えに書いて`ask_answered`（`runtime_closed: true`、attentionではない）を記録し、回答済みならcloseだけ）。`prompt_waiting`自体はattentionイベントではなく（`next: answer the prompt in workspace <id>`は消えた）、runもそのためのattentionを持たない。answerを見て画面にキーを送るのは人で、inboxが`dagq-recover`の`reference/session.md`に従って行う。resumeしたsessionのダイアログはaskにしない（resumeのtimeoutがその試行を終わらせる）。

## 既知のダイアログ

ADR-0047の決定29の固定の一覧にあるダイアログは、安全の条件がそろうときだけsupervisorが決まったキーで閉じる（task 355）。

- **判定**: `infrastructure::claude::known_dialog(screen) -> Option<DialogAnswer>`（純粋関数。`AgentSignals::known_dialog`越し）。入力欄が描かれている画面（`input_box`がある）は対象外。空行を除いた末尾30行を枠線を除いて見る。
  - **Background work is running**（`background_work`）: `Background work is running`で始まる行と、その下の`❯`の付いた番号付き選択肢の中に`Exit and stop tasks`で始まるものがある。キーは`❯`の位置からその選択肢までの`down` / `up`と`enter`。
  - **Settingsのパネル**（`settings_panel`、`/status`・`/usage`など）: `Settings:`で始まり、タブ名（`Status` / `Config` / `Usage`）を2つ以上含む行と、その下に`Esc to`で始まる行がある。キーは`escape`。
  - それ以外（trust、権限、auto mode、選択肢の無い・`❯`の無い・`Exit and stop tasks`の無い確認画面）は`None`で、キーは送らない。
- **条件**: `background_work`は、supervisorが`/exit`を打った後（exit timeoutでの読み取り。`ResumeWatch`がresumeのtimeoutでダイアログの上に`/exit`を打たなかったときは満たさない）で、worktreeがclean（`Repository::status`が空）、かつreceipt（run idが一致するもの）の`commit`がworktreeのHEADのときだけ。`settings_panel`は段を問わない（条件は空）。
- **いつ読むか**: (1) `/exit`のexit timeout（`ExitWatch`、`SessionWatch`のwrapperが黙ったときの`/exit`、`ResumeWatch`）で、`exit_request_timed_out`や`stuck_exit`のaskの前に画面を読む（`answer_exit_dialog`）。ここで扱うのは`background_work`だけで、Settingsのパネルは閉じても`/exit`は打ち直されないのでexit timeoutのまま進む。キーを送ったらexit timeoutを数え直し、それでも終わらなければ今までどおり`exit_request_timed_out`と`stuck_exit`のaskになる。(2) 上の`prompt_waiting`の読み取りで、`detect_prompt`より先に見る。(3) receiptの無いidleの促し（[idle-without-receipt](idle-without-receipt.md)）の前の画面の読み取りで、パネルが開いていればEscで閉じ、促しは次のtickに回す。
- **記録**: キーを送ったら`auto_repaired`（`layer: runtime`、`repair: dialog_answered`、`dialog`、`keys`、`conditions`（`exit_requested`、`clean`、`head`、`receipt_commit`。パネルは`{}`）、`detail`（`workspace_id`、`excerpt`=末尾15行））を記録する。条件がそろわないか、キーの送信に失敗したら`known_dialog_unanswered`（`dialog`、`conditions`、`error`、`workspace_id`、`excerpt`）を記録し、今までの経路（`prompt_waiting`と`answer_prompt`のask、exit timeoutの`stuck_exit`のask）に進む。
- **回数**: 1つのダイアログは段（最後の`agent_started` / `resume_started` / `revise_requested` / `exit_requested`の後）ごとに1回だけ扱う。同じ段に`auto_repaired`（`dialog_answered`）か`known_dialog_unanswered`が同じ`dialog`であれば、2回目のキーも記録も無く、今までの経路に進む。
- キーは`WorkspaceBackend::send_key`（cmuxは`cmux send-key --workspace <id> -- <key>`）で送る。
