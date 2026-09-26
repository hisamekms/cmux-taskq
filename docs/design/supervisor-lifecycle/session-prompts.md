---
id: design-supervisor-lifecycle-session-prompts
type: design
title: "Session prompts"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0022
  - design-plugin-integration
---

# Session prompts

inboxとplannerの初期promptは`src/application/prompt.rs`の`inbox_prompt(db)` / `planner_prompt(db)`（runtimeが立てるplannerは`runtime_planner_prompt`）が生成し（worker promptと同じファイル。[Prompt](prompt.md#prompt)。`runtime`からも再公開）、`inbox_prompt`と`planner_prompt`は5行以内（`runtime_planner_prompt`は指摘とtaskの行が足される。[ADR-0022](../../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)、ADR-0016の決定8）。CLIの手順はpromptに書かずskillに置くので、skillを変えてもpromptは変わらない。compactionと`/clear`からの起き直しはpromptではなくpluginの`SessionStart` hookが役割とskillの1行に続けて`status --role <role>`を出して担う（[plugin-integration](../plugin-integration.md#起き直しhookadr-0016)）。inboxはaskを人に見せてanswerを書き戻し、runtimeが適用しないanswer（`stuck_exit`の`exit`、`answer_prompt`など）は人の指示として`dagq-recover` skillの`reference/session.md` / `reference/stuck-exit.md`に従って実行する（task 100。それまでは退役した常駐sessionが実行していた）。inboxのworkspaceのcommandは`lifecycle`の`inbox_command`で`<claude> [--plugin-dir PATH] -- '<prompt>'`。plannerのworkspaceはsession wrapper `planner-session`を動かし、wrapperが`<planner dir>/prompt.txt`をpromptにして`claude`を起動する（[`plan` / `planners`](plan-planners.md#plan--planners)）。

- **inbox**: このqueue（db path）のinboxで、askとattentionを人に取り次ぎ自分では判断しないこと。`dagq status --role inbox`から始めてdagq pluginの`dagq-inbox` skillに従い、`dagq watch --role inbox --after <cursor>`をbackgroundで回して終了で起き、返ったcursorからwatchし直すこと。`ask_opened`が来たら`dagq asks --open --role inbox`でaskを読み、questionとoptionsを人に見せ（AskUserQuestionが使えるなら使う）、人の答えを`dagq answer ID --text '<answer>'`で書くこと。それ以外のattention（回答済みのask、止まったsupervisor、失敗したreview / triage）は人に知らせ、人の言うことだけをskillのとおり行うこと。queue DBを直接開かずCLIだけを使うこと。
- **planner**: このqueueのplannerの1つで、人の課題を聞いてgoalとtaskにすること。dagq pluginの`dagq-planner` skillに従い、その`dagq` skillの手順で登録してplan reviewにsubmitすること（`ready`にするのはplan review。ADR-0044の決定8）、runの着地とaskへの回答はしないこと。goalの全taskが完了したらreceiptをgoalのacceptanceと照合して`dagq goal close ID --verdict achieved`で閉じること。queue DBを直接開かずCLIだけを使うこと。observerのnoteとdraft goal、follow_upsのdraft taskの扱いはpromptに書かず`dagq-planner` skillが持つ。
