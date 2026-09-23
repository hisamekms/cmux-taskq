---
id: adr-0019
type: adr
title: maintainerの定型作業をruntimeに移す（needs_sessionの自動resume、exit timeoutで放棄しない、push、follow_ups、evidence、prompt待ち）
status: accepted
created: 2026-09-23
updated: 2026-09-23
owners:
  - hisamekms
tags:
  - runtime
  - maintainer
  - supervisor
  - operations
related:
  - adr-0008
  - adr-0010
  - adr-0012
  - adr-0013
  - adr-0016
  - design-supervisor-lifecycle
  - design-plugin-integration
---

# ADR-0019: maintainerの定型作業をruntimeに移す（needs_sessionの自動resume、exit timeoutで放棄しない、push、follow_ups、evidence、prompt待ち）

## Context

2026-09-23にmaintainer session 2本（workspace 351と431）のtranscriptを読んで分けた結果、maintainerの稼働の大半は判断を含まない定型作業だった。

1. **needs_sessionの解消**: `integrate`がrebaseの衝突か再検証の失敗でrunを`needs_session`にするたびに、maintainerは`cmux workspace create --cwd <worktree> --command "claude --resume <run-id>"`でresume workspaceを作り、`last_error`とmain側で着地したtaskを書いた定型文を送り、画面を読んで完了を待ち、receiptとheadを照合して`/exit`を送り、workspaceを閉じて`integrate`を打ち直す。この日だけで5回、同じ手順だった。
2. **exit_request_timed_outでの放棄**: Claude Code自身のダイアログが`/exit`を止めると、supervisorは`exit_request_timed_out`を記録してrunを手放す（[supervisor-lifecycle](../design/supervisor-lifecycle.md)の「1 runの異常（abandon）」）。commitもreceiptもverifyも揃ったrunを`recover` → `ready`で丸ごと再実行させた。
3. **push**: `integrate`のたびに無条件で`git push origin main`を打つ。
4. **follow_ups**: receiptの`follow_ups`をユーザーに報告し、回答待ちのまま持ち越す。
5. **evidenceの目視**: `src/`を変えたrunにe2eのevidenceが無いことを目視で見つける（AGENTS.mdの「runtimeを変えたrunではe2eは必須」は文書上の約束でしかない）。
6. **ダイアログ待ちの発見**: workerがtrust / LSP / auto modeのダイアログで止まっているのを`read-screen`で見つける。

[ADR-0016](0016-maintainer-notification-and-compact-output.md)はmaintainerを使い捨てのsessionにし、状態変化を`status` / `watch` / `notify`で届ける経路を決めたが、「`needs_session`のrunをruntimeが自動でresumeする仕組み」と「承認なしの自動着地」は対象外として後続ADRに送った。本ADRは前者を決め、後者は引き続き見送る。

## Decision

**原則。** 判断を含まない手順はruntime（supervisorと`integrate`）が引き受け、maintainerと人には判断（着地の承認、受け入れ条件の変更、ダイアログへの応答、follow_upの採否）だけを残す。[ADR-0016](0016-maintainer-notification-and-compact-output.md)の決定3（runtimeはmaintainerのterminalに打ち込まない）と決定5（`integrate`は自動で呼ばれない）は維持する。runtimeがworkerに送るのは`/exit`と、resumeしたsessionへの解消依頼の定型文だけ。追加するrun_eventsのkindは`resume_started` / `resume_finished` / `push_finished` / `push_failed` / `push_skipped` / `follow_up_registered` / `evidence_missing` / `prompt_waiting`で、既存のkind名とpayloadは変えない。attentionの判定は`src/watch.rs`と`domain::run_attention`に足す。以下の6点を決める。

1. **`needs_session`のrunはsupervisorがresumeする。**
   - resumeはworkerと同じ経路で起動する: `session` wrapper、run dirの`claude-settings.json`（`Stop` hookのidle marker）を使い、providerのコマンドは`claude --resume <run-id>`にする。起動したら`resume_started`を記録する。
   - 起動したsessionへ定型の解消依頼を送る。内容は衝突理由（runの`last_error`）と、runのbaseから現在のmainまでに着地したtaskのtitleとreceipt summary（mainのcommitの`Dagq-Task` trailerからtaskを引く）、そして「mainへrebaseして解消し、検証コマンドを再実行し、新しいheadでreceiptを書き直す」指示。
   - sessionがreceiptをworktreeの新しいheadで書き直してidleになったら（idle markerがreceiptより新しい）、`/exit`を送りworkspaceを閉じて`resume_finished`を記録する。
   - maintainer（または人）が`integrate`を呼び済みのrunは、runtimeがそのまま着地まで進める。「呼び済み」は新しく足す記録`integration_approved`で表す（`integrate`がrunを`needs_session`にするときに付ける。event・列のどちらで持つかは実装taskが決め、eventにするならkind名は`integration_approved`）。`validating`の`evidence_missing`（決定5）から来たrunは未承認。rebaseが再び衝突すれば再び`needs_session`になり、次のresumeに回る。未承認のrunは`awaiting_integration`に戻し、通常どおり`watch`のattentionとして承認を待つ。
   - 試行はrunごとに3回まで。超えたら従来どおりattention（`resume session`）で人に返す。
2. **`exit_request_timed_out`でleaseを手放さない。** timeoutは`exit_request_timed_out`の記録とattention（従来どおり`send /exit`。送るのは人かmaintainer）・`cmux notify`だけにし、supervisorはleaseを持ったまま`session_exited`を待つ。sessionが終われば通常の`validating`に進む。人が`recover`か`down`で止めるまで待ち続ける。`/exit`の再送はしない。
3. **`integrate`は着地後にmainをoriginへpushする。** 成功は`push_finished`。`--no-push`で抑止し、`origin` remoteが無ければ`push_skipped`を記録する。pushの失敗は`push_failed`（attention）で、runは`integrated`のまま（着地は取り消さない）。pushの再試行は人かmaintainerが行う。
4. **`integrate`はreceiptの`follow_ups`をdraft taskとして登録する。** 着地したtaskと同じgoal（無ければgoalなし）に`draft`で登録し（各要素の`title`をtitle、`description`をdescriptionにする。どちらかが文字列でない要素は登録せず、その要素をeventに残す）、`Task.context`に元のtaskとrunを書き、1件ごとに`follow_up_registered`を記録する。`ready`にするか、cancelするかは人の判断。
5. **taskは要求するevidenceを持つ。** `add --evidence e2e`（繰り返し可。値はreceiptのcheck名: `tests` / `e2e` / `subagent_review`）で指定する。`validating`で、要求したcheckがreceiptで`passed`でないか`evidence_or_reason`が空なら、runを`failed`ではなく`needs_session`（reason: `evidence_missing`）にし、`evidence_missing`を記録して決定1のresumeで補わせる。これは`validating`から`needs_session`への新しい遷移（従来は`integrate`だけが`needs_session`を作る）。判定は`Receipt::check`より先に行う: 要求したcheckが`failed`か`not_applicable`か空のevidenceなら`evidence_missing`で、要求していないcheckと`result`の`failed`は従来どおり`Receipt::check`が`failed`にする。AGENTS.mdの「`src/`を触るtaskはe2e」はこのflagで表す。
6. **supervisorはprompt待ちを検知する。** `agent_started`の後、receiptもidle markerも無いまま一定時間止まったrunについて、supervisorが画面を読み、ダイアログの兆候（先頭に`❯`を持つ番号付き選択肢、`Esc to cancel`など）があれば`prompt_waiting`を1回記録し、attentionと`cmux notify`に出す。キーは送らない。trust / LSP / auto modeへの応答は人かmaintainerが行う。画面の読み取りは`WorkspaceBackend`の後ろに置き、domain / applicationはcmuxを直接参照しない（[ADR-0013](0013-layered-architecture-and-type-function-style.md)）。

実装はgoal 8の後続taskが行い、各taskが`docs/design/supervisor-lifecycle.md`と`dagq-maintain` / `dagq-land` / `dagq-session` skill（task 65の分割後の名前）を更新する。schemaを変えるtaskは`user_version`を上げて`migrations/`に追加する。本ADRの時点では未実装。

## Alternatives

- **`integrate`自体がresumeの完了を待ってblockする**: `integrate`のプロセスが衝突解消の間（数分〜数十分）着地スロットとleaseを握り、他のrunの着地を止める。呼び手（maintainerのBash）もtimeoutする。resumeはsupervisorの常駐ループに置き、`integrate`は`needs_session`で即座に返して、承認（`integration_approved`）だけを残す。
- **timeoutで`/exit`を再送する**: ダイアログが開いている時に`/exit`とEnterを重ねて送ると、ダイアログの選択肢を押してしまう。session側の状態を知らずに入力を重ねない。timeoutは知らせるだけにし、終了はwrapperの`session_exited`で確かめる。
- **ダイアログをruntimeが閉じる（Escや選択肢のキーを送る）**: 画面の構造と選択肢の意味にruntimeを結合し、Claude CodeのUIが変わると誤操作する。trustやauto modeの応答は権限の判断でもある。[ADR-0016](0016-maintainer-notification-and-compact-output.md)がpush型通知を退けた理由（TUIへの非結合、送達確認の欠如）と同じ理由で退け、検知とattentionにとどめる。
- **承認なしの自動着地**: `awaiting_integration`を観測したruntimeが`integrate`を呼べば定型作業はさらに減るが、着地の承認はユーザーに残すとユーザーが判断したので見送る。runtimeが着地まで進めるのは、人かmaintainerが`integrate`を呼んだrunの衝突解消の続きだけ。

## Consequences

- [ADR-0016](0016-maintainer-notification-and-compact-output.md)が対象外にしていた「`needs_session`のrunをruntimeが自動でresumeする仕組み」は本ADRで決まった。ADR-0016の決定5（`integrate`は自動で呼ばれない）は維持する: runtimeが自発的に`integrate`を呼ぶことはなく、承認済みのrunの再着地だけを引き受ける。「承認なしの自動着地」は引き続き対象外。
- maintainerはresume workspaceを作らず、`needs_session`は試行が尽きたときだけattentionとして人に返る。[supervisor-lifecycle](../design/supervisor-lifecycle.md)の`needs_session`節（maintainerが`claude --resume`でsessionを開き直す手順）は実装taskが書き換える。
- `exit_request_timed_out`はabandonの理由から外れ、leaseを持ったまま待つ。人が介入しない限りsupervisorのslotを1つ占有し続ける。
- workerへの送信は`/exit`に加えて解消依頼の定型文が増える。どちらもruntimeが起動・監視しているsessionに限り、maintainerのterminalには打ち込まない。
- `integrate`が`origin`へpushするので、maintainerの手順から`git push`が消える。pushしたくない運用（検証用のrepositoryなど）は`--no-push`を使う。
- follow_upsはdraftとしてqueueに残るので、報告と回答待ちのまま持ち越されない。draftが溜まるので、人は`list --status draft`で採否を決める。
- 要求evidenceが文書上の約束からqueueの検証に移る。evidenceを要求するtaskを`add`する側（maintainer）が`--evidence`を付け忘れると検証されない。
- `prompt_waiting`は画面の文言に依存するヒューリスティックで、見逃しと誤検知があり得る。キーを送らないので誤検知の害はattentionが1件増えることに限られる。
