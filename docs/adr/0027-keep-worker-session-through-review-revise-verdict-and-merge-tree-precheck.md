---
id: adr-0027
type: adr
title: workerのsessionをreviewの後まで残し、機械的な指摘はrevise verdictで生きているworkerに返し、着地前にmerge-treeで衝突を事前判定する
status: accepted
created: 2026-09-23
updated: 2026-09-23
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - operations
  - performance
related:
  - adr-0019
  - adr-0022
  - adr-0023
  - design-supervisor-lifecycle
  - design-plugin-integration
---

# ADR-0027: workerのsessionをreviewの後まで残し、機械的な指摘はrevise verdictで生きているworkerに返し、着地前にmerge-treeで衝突を事前判定する

## Context

2026-09-23の本番queue 81 runを集計すると、`needs_session`になったrunは18で、うち16はrebaseの衝突（着地待ちの間にmainが進んだ）、2は再検証の失敗だった。reviewの指摘が原因のものは記録に無い。resumeの後にworkerが直すのに使った時間は2〜8分だが、衝突から次の`integrate`までの周期は最近34〜54分（初期は4〜8分）で、大半はmaintainerと人が動くまでの待ちだった。

現在の流れ（実装済みのものと、決定済みで未実装のもの）は次のとおり。

- supervisorの`SessionWatch::poll`（`src/runtime.rs`）はreceiptを受け取った後、sessionがidleになると`/exit`を送る。`session_exited`の後の`Validating`でworkspaceを閉じ（`close_workspace`）、receiptを照合する。
- （決定済み・未実装。task 107）[ADR-0023](0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)の決定2で、`awaiting_integration`のrunはsupervisorがheadlessの`claude -p`でreviewし、verdictは`pass | concern`。passなら着地、concernなら[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定3の`approve_landing`のaskにする。reviewにcmux workspaceは作らない。
- 着地のrebaseが衝突するか再検証が失敗すると`needs_session`になる。決定済みで実装中の（task 71）[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)の決定1では、supervisorが`claude --resume <run-id>`で同じ会話を開き直し、定型の解消依頼を送り、receiptが書き直されてidleになったら`/exit`を送ってworkspaceを閉じる。

workerは`--session-id <run-id>`で起動しているので、resumeは同じ会話の再開であり、消えているのはプロセスとworkspaceだけ。それでもresumeのたびにworkspaceの作り直し、LSPなどのダイアログ、2度目の`/exit`が入る。reviewの指摘は、機械的に直せるものであってもconcernとして人の返答を待つ。着地のrebaseが衝突することは、`/exit`を送る前にmainと比べれば分かる。

ユーザーは2026-09-23に「レビューで問題があれば元のworkerに直させたい」と提起した。workerをマージまで残す案も検討したが、`/exit`をreviewの後ろへずらす方針を採る。

## Decision

**原則。** workerのsessionは、そのsessionで直せる指摘や衝突が出なくなるまで閉じない。人の判断が要るもの（concern）と、閉じた後に起きたもの（着地slotの待ちの間の衝突、再検証の失敗）だけを従来の経路（askとresume）に回す。run_eventsのkindは追加だけにし、既存のkind名とpayloadは変えない。以下の4点を決める。

1. **workerのsessionを閉じる時点を`validating`の後からreviewの後へ動かす。**
   - receiptを受理した（`validating`を通った）runには`/exit`を送らず、workspaceも閉じない。leaseとslotを持ったまま、supervisorのheadless review（[ADR-0023](0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)の決定2）に進む。
   - `/exit`とworkspaceのcloseを行うのは次の3つの時点だけ。
     - verdictが**pass**のとき: 着地（land）の直前。決定4のmerge-treeの判定で衝突しないと分かった後。
     - verdictが**concern**のとき: `approve_landing`のaskを作る前。
     - headless実行が失敗した（`review_failed`）とき: attentionを出す前。
   - `validating`とreviewの間slotを握ることは受け入れる。[ADR-0023](0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)の決定1（task 90）で`validating`はreceiptの照合だけになり、reviewのtimeoutは10分を想定するので、slotを余分に握る時間はreviewの所要時間に限られる。
2. **reviewのverdictを`pass` / `revise` / `concern`の3値にし、reviseは生きているsessionに返す。**
   - verdictのJSONは`{"verdict": "pass" | "revise" | "concern", "reasons": [...], "summary": "..."}`にする。
   - **revise**は人の判断が要らない指摘: テストやevidenceの不足、lint・fmt・clippyの指摘、receiptの記述と差分の食い違いで差分を直せば済むもの、指示された範囲の中の明らかな欠落。
   - **concern**は[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定3の疑義: 受け入れ条件との食い違い、taskの指示にない変更、判断を含む指摘。従来どおり`approve_landing`のaskで人に聞く。
   - reviseのとき、supervisorは`reasons`を定型文にして生きているsessionに`cmux send`で送る（resumeは作らない）。`revise_requested`（payload: `attempt`, `reasons`）を記録する。sessionが直してcommitし、新しいheadでreceiptを書き直してidleになったら（idle markerが書き直したreceiptより新しい）`revise_finished`（payload: `head`）を記録し、`validating` → reviewをやり直す。
   - reviseはrunごとに2回まで。3回目のreviewがpassでなければ、verdictがreviseでもconcernとして扱い、`approve_landing`のaskにする（決定1のとおり、askを作る前に`/exit`とcloseを行う）。
   - 追加するrun_eventsのkindは`revise_requested`と`revise_finished`。`review_started` / `review_finished` / `review_failed`を含む既存のkindとpayloadは変えない（`review_finished`のpayloadの`verdict`に`revise`が入り得るようになる）。
3. **[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)の決定1の自動resume（task 71）で開き直したsessionも同じ扱いにする。**
   - resumeしたsessionがreceiptを書き直してidleになったら、`/exit`を送らず`validating` → reviewに進む。passなら決定4の判定の後、着地の直前に`/exit`とcloseを行う。reviseとconcernは決定2のとおり。
   - `integration_approved`のrun（maintainerか人が`integrate`を呼び済みのrun）は、従来どおりreviewを待たずに`/exit` → closeして着地に進む。
4. **passのとき、`/exit`を送る前に`git merge-tree`で衝突を事前判定する。**
   - supervisorはrun branchのheadと現在のmainを`git merge-tree --write-tree`で比べる。worktreeは触らない（headもindexも動かさない）。
   - **衝突する**: `/exit`を送らず、生きているsessionにtask 71の解消依頼と同じ定型文（rebase先のmainのcommit、baseからmainまでに着地したtaskの一覧、手順）を`cmux send`で送る。sessionがrebaseしてreceiptを書き直してidleになったら、`validating` → review → merge-treeの判定をやり直す。
   - **衝突しない**: `/exit` → close → land（[ADR-0023](0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)の決定2の着地と同じland関数と単一slot）。
   - landのrebaseが着地slotの待ちの間に再び衝突したとき（mainがさらに進んだとき）と、rebase後の再検証が失敗したときだけ、`needs_session`になり[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)の決定1のresumeに回る。

実装はgoal 11の後続taskが行う。本ADRの時点では未実装。

## Alternatives

- **workerをマージまで残す**: 衝突も再検証の失敗も必ず生きているsessionで直せるが、着地待ちの中央値（16.5分）が作業時間の中央値（17分）とほぼ同じなので、slotの実効並列度が半分になる。worktreeは着地で消えるので、sessionはどのみち着地の前に終える必要がある。concernのaskに人が答えるまでslotを握り続ける。
- **従来どおり`validating`の直後に閉じ、指摘も衝突もresumeで直す**: 同じ会話の再開なので文脈は残るが、workspaceの作り直し、resumeを開くときのLSPなどのダイアログ、2度目の`/exit`が毎回入る。reviseの往復には人が要らないのに、毎回resumeを経る。
- **reviewの指摘をすべて人に聞く**（[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)と[ADR-0023](0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)のまま）: 実装は要らないが、機械的な指摘まで人の返答待ちになり、着地待ちが伸びる。
- **衝突の事前判定をworktreeでの`git rebase`で行う**: 衝突しなければそのまま着地できるが、判定のためにheadが動き、receiptのcommitと食い違う（`validating`の照合が崩れる）。worktreeとheadを動かさずに判定できるので、`git merge-tree --write-tree`に限る。

## Consequences

- [ADR-0023](0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)の決定2のうち「verdictは`pass | concern`」を「`pass | revise | concern`」に改める。「reviewにcmux workspaceは作らない」はheadlessのreview自体については維持するが、reviewの間もworkerのworkspaceは閉じずに残る点を改める。goal 11の制約の「verdictのJSONを`{verdict: pass | concern, ...}`に固定する」も本ADRで改める。
- [ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定3の「疑義のあるときだけ人に聞く」はconcernに限って維持し、人の判断の要らない指摘はreviseとしてworkerに返す。reviseの上限を超えたrunはconcernとして人に聞く。
- [ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)の決定1の「receiptを書き直してidleになったら`/exit`を送りworkspaceを閉じる」を、「`validating` → reviewに進み、passなら着地の直前に`/exit`する」に改める。`integration_approved`のrunは従来どおり。ADR-0019のConsequencesの「runtimeがworkerに送るのは`/exit`と解消依頼の定型文だけ」には、reviseの定型文が加わり、解消依頼の定型文はresumeしたsessionに加えて決定4の生きているsessionにも送る。送る先はruntimeが起動・監視しているsessionに限る点は変わらない。
- rebaseの衝突の多く（着地待ちの間にmainが進んだもの）は、`/exit`の前のmerge-treeの判定で生きているsessionに返るので、`needs_session`とresumeは着地slotの待ちの間の衝突と再検証の失敗に減る見込み。
- runはreviewの間とreviseの往復の間slotを握る。slotの占有時間は伸びるが、resumeによるworkspaceの作り直しと人の待ちが減る。`stats`（ADR-0023の決定5）の着地待ちとresume回数で効果を確かめる。
- [supervisor-lifecycle](../design/supervisor-lifecycle.md)の状態機械（`validating`の後にreview、reviseの往復、merge-treeの判定、`/exit`の位置）と、`dagq-land` / `dagq-session` skillの手順が変わる。各実装taskが更新する。
