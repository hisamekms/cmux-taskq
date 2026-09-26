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

receiptより前にClaude Code自身のダイアログ（folder trust、LSP pluginの推奨、auto modeの案内など）で止まったsessionは、`Stop`が発火せず画面も変わらないまま待ち続ける。supervisorは画面を読んでこれを検知し、記録してinbox宛てのaskにするだけでキーは送らない（ADR-0019の決定6、ADR-0044の決定17。応答は人）。

- **条件**: agentの登録（`agent_started`）をそのsupervisorが最初に見てから`WorkspaceBackend::prompt_wait`（cmuxは90秒）以上経ち、receiptも`idle.json`もなく、closeされていない`worker_question`もなく（askで止まったworkerは回答を待っているのでダイアログ待ちではない）、wrapperが生きて（未終了でheartbeatが有効）agentのPIDも生きているrun。画面の読み取りは`WorkspaceBackend::capture`で、多くとも10秒に1回（`prompt_wait`がそれより短ければその間隔）。引き継いだrunは引き継いだ時点から数える。読み取りの失敗はsupervisor logに書くだけでrunには影響させない。
- **判定**: `infrastructure::claude::detect_prompt(screen) -> Option<PromptKind>`（純粋関数。`runtime`から再公開。supervisorは`AgentSignals::detect_prompt`越しにkindの名前だけを受け取る）が、画面の空行を除いた末尾30行について、枠線（`│`など）と前後の空白を除いた行で見る。`Do you trust`で始まる行か`trust this folder`を含む番号付き選択肢があれば`trust`、`❯`で始まる番号付き選択肢（`❯ 1. …`）の前後3行以内にも番号付き選択肢があれば（選択肢の文が折り返しても）`choice`、`Enter to confirm`か`Esc to cancel`で始まる行があれば`confirm`。文中や引用の中の同じ文言（作業中の出力やコード）は行頭にないので数えない。
- **記録**: 兆候があれば`prompt_waiting`（`workspace_id`、`excerpt`=空行を除いた末尾15行、`screen_hash`=excerptのSHA-256、`prompt`=判定の種類）を記録してlogに書く。同じ`screen_hash`の間は再記録せず、別のダイアログに変わればもう一度記録する。記録した後に兆候が消えるか、idle markerが書かれるかagentのPIDが死ねば`prompt_cleared`（`workspace_id`）を記録する（receiptが来たときは`receipt_observed`がダイアログを終わらせるので記録しない）。引き継いだrunは最後の`prompt_waiting`（その後に`prompt_cleared` / `receipt_observed`が無いもの）の`screen_hash`を引き継ぎ、同じ画面を再記録しない。記録済みのダイアログがある間は`prompt_wait`を待たずに読み取りの間隔で画面を読むので、supervisorの不在中に応答されたダイアログもすぐ`prompt_cleared`になる。
- **ask**（task 100）: `prompt_waiting`を記録するたびに、supervisorは`kind: answer_prompt`のaskをinbox宛てに開く（`ask_answer_prompt`。`asked_by: supervisor`、taskとrunに紐づき、optionsは無し。questionはrun id・task id・ダイアログの種類・workspaceのUUIDと、「答えはそのworkspaceで入力され、ダイアログが消えればこのaskは自分で閉じる」旨、末尾に`excerpt`）。askの登録なので`cmux notify`が1回inboxへ飛ぶ。画面が変わっても（別のダイアログ、時刻の表示の更新など）openなaskはそのまま残し（同じrunとkindのopenなaskが返るので通知は1回だけ）、`prompt_cleared`、`receipt_observed`、sessionの終了、triageのときにaskを閉じる（`close_answer_prompt_asks`。未回答なら`the dialog is gone; closed by the runtime`などを答えに書いて`ask_answered`（`runtime_closed: true`、attentionではない）を記録し、回答済みならcloseだけ）。`prompt_waiting`自体はattentionイベントではなく（`next: answer the prompt in workspace <id>`は消えた）、runもそのためのattentionを持たない。answerを見て画面にキーを送るのは人で、inboxが`dagq-recover`の`reference/session.md`に従って行う。resumeしたsessionのダイアログはaskにしない（resumeのtimeoutがその試行を終わらせる）。
