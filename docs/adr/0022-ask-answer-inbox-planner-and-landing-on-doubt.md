---
id: adr-0022
type: adr
title: 相談をqueueのask / answerにし、upがinboxとplannerを開き、着地は疑義のあるときだけ人に聞き、cmux notifyはinbox宛てにする
status: accepted
created: 2026-09-23
updated: 2026-09-23
accepted_on: 2026-09-23
owners:
  - hisamekms
tags:
  - runtime
  - maintainer
  - plugin
  - operations
related:
  - adr-0008
  - adr-0010
  - adr-0016
  - adr-0019
  - adr-0021
  - design-overview
  - design-supervisor-lifecycle
  - design-plugin-integration
  - design-persistence
---

# ADR-0022: 相談をqueueのask / answerにし、upがinboxとplannerを開き、着地は疑義のあるときだけ人に聞き、cmux notifyはinbox宛てにする

## Context

[ADR-0016](0016-maintainer-notification-and-compact-output.md)はsupervisorからmaintainerへの経路を`status` / `watch`のpull型にしたが、maintainerから人への経路と、workerからの相談経路は決めなかった（Consequencesの対象外に「`ask` / `answer`によるworkerからmaintainerへの相談経路」と「承認なしの自動着地」を挙げた）。[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)は定型作業をruntimeへ移したが、着地の承認はユーザーに残した。

その結果、人への相談は次の2つに依存している。

- **maintainerから人へ**: maintainerのterminalでの対話と`AskUserQuestion`。人がmaintainer workspaceを見ていないと、maintainerは回答が来るまで止まる。2026-09-23にはreceiptの`follow_ups`の採否とADR番号の確認の回答待ちで数時間止まった。
- **workerからmaintainerへ**: workerはterminalに質問を書いて待つ（AGENTS.mdのworker節）。maintainerが`read-screen`で見つけるまで誰も気づかない。

加えて、着地のたびに承認を求めると、承認は人への相談の中で最も件数が多い。レビューが通り、受け入れ条件どおりのrunまで人を待つと、相談経路を作っても詰まりは戻る。

`cmux notify`はADR-0016の決定4でattentionのたびにmaintainer workspaceへ送ることにしたが、maintainerはwatchで起きるので通知は要らず、人は答えるべきことが無い通知（runの遷移）まで受け取る。

## Decision

**原則。** 人の判断を要する相談はqueueの行（ask）にし、pull型で届ける。相談した側はaskを登録して手を離し、答える側は自分宛てのaskだけを`watch`で受ける。runtimeはどのsessionのterminalにも打ち込まない（[ADR-0016](0016-maintainer-notification-and-compact-output.md)の決定3を維持する）。例外はworkerへのanswerの送信だけで、workerはaskの後にidle markerを書いて止まっているので入力可能と判定できる。以下の5点を決める。

1. **askをqueueに持ち、`ask` / `answer` / `asks`で扱う。**
   - 新しい表`asks`を足す。列は`id`、`kind`、`task_id`、`run_id`、`question`、`options`（選択肢のJSON配列）、`answer`、`asked_by`（登録したsessionのrole）、`created_at`、`answered_at`。`answer`と`answered_at`がNULLのaskがopen。
   - `kind`は`approve_landing` / `answer_prompt` / `decide` / `worker_question`の4つ。answerを要求しない知らせはaskにせず、attention（[ADR-0016](0016-maintainer-notification-and-compact-output.md)）かnoticeにする。
   - askは（`run_id`または`task_id`、`kind`）で一意にし、openなaskの二重登録は既存の`id`を返す。
   - CLIは`ask`（登録）、`answer ID`（回答を書き戻す）、`asks`（一覧。既定はopenだけ）。
   - run_eventsのkindに`ask_opened`（inbox向けのattention）と`ask_answered`（maintainer向けのattention）を足す。既存のkind名とpayloadは変えない。
   - `watch --role <role>`で自分宛てのattentionだけを受ける。`--role inbox`は`ask_opened`を、`--role maintainer`は`ask_answered`とADR-0016のattentionを受ける。`status`もopenなaskを出す。
   - schemaを変えるtaskは`user_version`を上げて`migrations/`に追加する。
2. **workerは`dagq ask RUN --question ...`で相談して止まり、answerはruntimeがworkerのterminalに送る。** workerはterminalに質問を書いて待つ代わりに`worker_question`のaskを登録し、idleになる。askが答えられたら、supervisorはidle markerでworkerが入力可能なことを確かめてから、answerを定型文としてworkerのterminalへ送る。送るのは[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)の`/exit`・解消依頼と同じく、runtimeが起動・監視しているworkerのsessionに限る。
3. **着地は疑義のあるときだけ人に聞く。** subagentレビューが通れば、maintainerは人を待たずに`integrate`を呼ぶ。次のいずれかがあるときだけ`approve_landing`のaskを作って待つ。
   - receiptや差分が受け入れ条件と食い違う
   - taskの指示にない変更を含む
   - subagentレビューが指摘を返した

   answerが着地を認めればmaintainerが`integrate`を呼び、認めなければ差し戻すかtaskをcancelする。[ADR-0016](0016-maintainer-notification-and-compact-output.md)の決定5のうち「着地の承認はユーザーに残す」を「疑義のあるときだけユーザーに聞く」に改める。runtimeが自発的に`integrate`を呼ばないことは維持する: 呼ぶのはmaintainerで、`watch`の中やイベントの副作用からは呼ばない。
4. **`up`が`[<repo>]inbox`と`[<repo>]planner`を開く。** maintainer・supervisorのworkspaceと同じく`up`が作り、session_workspacesに記録する。workspace名の書式はgoal 9の決定に従う（本ADRの時点の[ADR-0021](0021-maintainer-and-supervisor-workspace-names-follow-the-run-style.md)の書式`[<repo>]dagq <role>`からgoal 9が変える前提で、`[<repo>]inbox` / `[<repo>]planner`と書く）。role値は`planner` / `inbox`（goal 9のtask 77の定義）を使い、workspaceの識別はgoal 9のUUIDと`--env`に従う（名前で探す`find_named`は新設しない）。
   - **inbox**は`watch --role inbox`をbackgroundで回して起き、openなaskを選択肢付きで人に見せ、人の答えを`answer`で書き戻すsession。自分では判断しない。
   - **planner**は人と対話してgoalとtaskを登録し（`goal add` / `add` / `ready`）、goalをcloseするsession。
   - **maintainer**はtaskとgoalの登録とgoalのcloseを持たない。監視、レビュー、着地、askの登録と回答の実行が残る。
5. **`cmux notify`は`ask_opened`のときだけinboxのworkspace宛てに送る。** runの遷移（`awaiting_integration`・`needs_session`・`failed`など）は通知しない。maintainerはattentionを`watch`で受けるので通知は要らず、人には答えるべきaskがあるときだけ知らせる。task 75（`cmux notify`の実装）の対象と宛先をこれに改める。

実装はgoal 10の後続taskが行う（`asks`表とCLI、`watch --role`、workerへのanswer送信、`up`のinbox / planner、`cmux notify`の宛先、skillのinbox / planner / maintainerへの分割）。skillはtask 65の分割後の名前を前提にする。本ADRの時点では未実装。

## Alternatives

- **runtimeがmaintainerのterminalに`cmux send`でanswerや通知を打ち込むpush型**: 仕組みは少ないが、[ADR-0016](0016-maintainer-notification-and-compact-output.md)がpush型を退けたのと同じ理由で退ける。maintainerのUI状態（permission dialogや選択肢が開いているか）が分からず、打ち込んだ文字が選択肢を押しうる。送達確認が無く、Claude CodeのTUIに結合する。workerへのanswerだけは、askの後にidle markerで入力可能と判定できるので例外にする。
- **`AskUserQuestion`のまま（maintainerのterminalでの対話）**: 実装は要らないが、人がmaintainer workspaceを見ていないとmaintainerが止まる。相談の中身がmaintainerのコンテキストにしか残らず、compactionや再起動で失われ、`status`からも再導出できない。
- **毎回承認（全runで`approve_landing`のaskを作る）**: ADR-0016の決定5をそのまま保てるが、askの件数が最多になり、レビューが通って受け入れ条件どおりのrunまで人を待つ。相談経路を作っても詰まりが戻る。

## Consequences

- [ADR-0016](0016-maintainer-notification-and-compact-output.md)の決定5の「着地の承認はユーザーに残す」は「疑義のあるときだけ`approve_landing`で聞く」に改まる。runtimeが自発的に`integrate`を呼ばない点は維持する。[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)のAlternativesが見送った「承認なしの自動着地」は、maintainerがレビュー通過を確かめて呼ぶ形に限って採る。
- ADR-0016の決定8の`maintainer_prompt`（「attentionを報告して承認を待つ」）は、「レビューが通れば着地させ、疑義のあるときだけ`approve_landing`のaskを作る」に改める。
- ADR-0016の決定4（attentionのたびにmaintainer workspaceへ`cmux notify`）は、`ask_opened`のときだけinbox宛てに改まる。ADR-0016のConsequencesが対象外にした「`ask` / `answer`によるworkerからmaintainerへの相談経路」は本ADRで決まり、maintainerへの経路は同ADRの約束どおり`watch`（attentionの追加）を使う。
- [ADR-0010](0010-maintainer-and-resident-supervisor.md)の役割にinboxとplannerが加わり、maintainerの担当から登録とgoal closeが外れる。[overview](../design/overview.md)の用語集にinbox / plannerを足す。
- goal 8のtask 71（resumeの3回失敗のattention）とtask 74（`prompt_waiting`）は人の判断を要するので、ask（`decide` / `answer_prompt`）に乗せ替える後続taskが要る。
- AGENTS.mdのworker節（「判断が要るときはterminalに質問を書いて待つ。maintainerが`read-screen`で拾う」）は`dagq ask`に置き換わる。`read-screen`はworkerの相談を拾う手段ではなくなる。
- 疑義の判定はmaintainerとsubagentレビューに依存する。判定を誤ると、人が見るべきrunがaskなしで着地しうる。着地はmainの1 commitなので、後からrevertで戻せる。
- `asks`表の追加でschemaが変わる。run_eventsのkindは追加（`ask_opened` / `ask_answered`）だけで、既存の公開契約は壊さない。
