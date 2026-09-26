---
id: adr-0040
type: adr
title: 検証をintegrateの1回にし、reviewをsupervisorの工程にし、dagq.tomlでrunのenvを渡し、taskの5段階の優先度と解放数でclaim順を決め、statsで詰まりを数える
status: superseded
created: 2026-09-25
updated: 2026-09-25
accepted_on: 2026-09-25
superseded_by: adr-0049
superseded_on: 2026-09-26
supersedes:
  - adr-0023
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - operations
  - performance
related:
  - adr-0008
  - adr-0019
  - adr-0022
  - adr-0023
  - adr-0024
  - adr-0027
  - adr-0029
  - adr-0035
  - adr-0038
  - design-supervisor-lifecycle
  - design-domain-model
  - design-persistence
  - design-provider-lifecycle
---

# ADR-0040: 検証をintegrateの1回にし、reviewをsupervisorの工程にし、dagq.tomlでrunのenvを渡し、taskの5段階の優先度と解放数でclaim順を決め、statsで詰まりを数える

> **置き換え済み（2026-09-26）**: このADRの決定は現在有効ではない。現行の決定は[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)を読む。

## Context

[ADR-0023](0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)（goal 11、2026-09-23）は実行効率のために5つを決めた: 検証を`integrate`の1回にする、reviewをsupervisorの工程にしてpassなら着地する、`dagq.toml`でrunのenvを渡す、`graph`と解放数でclaim順を決める、`stats`で詰まりを数える。その後、次の変更が入った。

- [ADR-0024](0024-retire-maintainer-into-jobs-and-observer.md)が常駐のmaintainerを退役させ、役割をsupervisor / worker / planner / inbox / observerにした。ADR-0023の決定2の「maintainerが手でレビューする」「maintainerが`integrate`を呼ぶ」は、inboxが人に知らせ人が行う操作に読み替えられていた。
- [ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)が決定2のverdictを`pass | revise | concern`にし、reviewの間もworkerのsessionを残し、passの着地の前に`git merge-tree`で衝突を事前判定するようにした。
- task 91の着地前のreviewの指摘を受け、2026-09-23にユーザーが「このrepositoryではtargetを共有せず、`dagq.toml`も置かない」と決めた（決定3の`CARGO_TARGET_DIR`の共有を採らない）。同じtaskで展開できる変数に`${DAGQ_RUN_DIR}`を加えた。
- 決定4の実装（`graph`、`fill_slots`）は、順序が`graph`で再現できるので`claim_reordered`を記録しないことにした。[ADR-0038](0038-task-depends-on-a-goal-until-it-is-achieved.md)がtaskのgoalへの依存を足し、解放数の依存グラフにgoalを経由する辺が加わった。

さらにclaim順（決定4）にtaskごとの優先度が無い。今の順は「解放数（`unblocks`、推移的）の多い順 → IDの小さい順」だけで決まるので、急ぎのtaskを登録してもIDが大きく、既にreadyのtaskの後ろに回る。回避策は他のtaskをdraftに戻すか、依存を歪めることしかない。2026-09-25には、goal 28（この優先度を入れるgoal）を先に流すために、readyだった37のtaskを実際にdraftへ退避した。

[ADR-0035](0035-adr-is-superseded-whole-with-dates-and-banner.md)は決定を1つでも変えるADRに、古いADRのまだ生きている決定を書き直して引き継ぎ、古いADRを丸ごと置き換えることを求める。本ADRはADR-0023を丸ごと置き換える。決定1〜3と5はADR-0023の内容を後継のADRと上のユーザーの決定に合わせて書き直したもので、決定4はgoal 28の人の決定（2026-09-25。goalのconstraints）でclaim順を改めたものである。番号はADR-0023と同じにそろえる。

## Decision

**原則。** 同じcommitに対する高価な処理は1回にし、判断を含まない待ちはsupervisorの工程にする。claim順は人が付けた優先度を最初に、並列度（解放数）を次に見る。run_eventsのkindは追加だけで、既存のkind名とpayloadは変えない。schemaを変えるtaskは`user_version`を上げて`migrations/`に追加する。以下の5点を決める。

1. **`validating`はreceiptの照合だけを行い、`verification_commands`は`integrate`のrebase後に必ず1回走らせる。**
   - `validating`が見るのは、receiptの整合（形式、`run_id`、`result`）、commitがrun branchのheadで`base_commit`の子孫であること、worktreeがcleanであること、taskが要求するevidence（[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)の決定5）と、taskの`paths`（[ADR-0029](0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)）だけ。`verification_command`のeventは`validating`では記録しない。
   - `integrate`はrebaseの後、headが動いたかどうかにかかわらず`verification_commands`を1回実行し、出力を試行ごとの`integrate-<attempt>-verify-N.log`に残す。task 48の「rebaseがno-opなら再検証しない」判定は廃止した。event kindの`integration_verification_skipped`は過去のeventを読むために残し、記録はしない。`integrate`の出力の`verification_skipped`fieldも残し、常に`false`を返す。
   - 検証の失敗はrunを`needs_session`にし、supervisorの自動resume（ADR-0019の決定1）が同じsessionで直す。runを作り直さないので、作業を捨てない。
   - 1つのcommitに対する`verification_commands`の実行は`integrate`の1回だけで、`validating`の所要時間はreceiptとGitの照合だけになる。
2. **reviewをsupervisorの工程にし、passならsupervisorが着地させる。**
   - `awaiting_integration`になったrunに対し、supervisorは`review ID`と同じ関数で`review.md`を書き、`review_started`を記録する。
   - review本体は`AgentProvider`のportのheadless実行で行う。Claudeでは`claude -p`をrun dirの設定（`settings`）で起動し、promptに`review.md`のpathとverdictのJSON schemaを渡し、stdoutのJSONを読む。reviewのためのcmux workspaceは作らない。reviewの間もworkerのsessionとworkspaceは閉じずに残す（ADR-0027の決定1）。
   - reviewが見る観点は[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定3の疑義と同じ: receiptや差分が受け入れ条件と食い違う、taskの指示にない変更を含む、判断を含むレビューの指摘がある。
   - verdictは`pass | revise | concern`（reviseの規則はADR-0027の決定2・3）で、`reasons`と`summary`とともに`review_finished`のpayloadに記録する。
   - **pass**: `git merge-tree`で衝突を事前判定し（ADR-0027の決定4）、衝突しなければsupervisorが着地させる。着地は`integrate`と同じland関数と単一の着地slotを使い、push（ADR-0019の決定3）まで行う。rebaseの衝突や検証の失敗は`integrate`と同じく`needs_session`になり、自動resumeの後は承認済みとして再び着地に進む。
   - **revise**: 生きているworkerのsessionに指摘を返す（ADR-0027）。
   - **concern**: `approve_landing`のask（options `land` / `send_back` / `cancel`）を作り、`reasons`と`summary`をquestionに載せて待つ。askはinboxが人に見せ、人の答えをsupervisorが適用する（`land`は着地、`send_back`は`needs_session`に戻してresumeで直させる、`cancel`はtaskを取り消す）。
   - **headless実行の失敗**（起動できない、非0終了、timeout、stdoutがschemaに合わない）: runを`awaiting_integration`のまま残し、`review_failed`を記録する。inboxが人に知らせ（`review by hand`）、人が手でreviewして`integrate`を呼ぶ（ADR-0024）。
3. **repository rootの`dagq.toml`の`[run.env]`をworkerと検証に渡す。**
   - `dagq.toml`は当面`[run.env]`だけを持つ（キーが環境変数名、値が文字列）。それ以外の設定を足すときは別のADRで決める。
   - 値には`${DAGQ_QUEUE_DIR}`（そのrepositoryのqueue directory）と`${DAGQ_RUN_DIR}`（そのrunのrun directory）を展開できる。それ以外の変数は展開しない。
   - supervisorはworkerのworkspaceを作るときに`workspace --env`で渡し（[ADR-0026](0026-identify-workspaces-by-uuid-env-and-queue-group.md)）、`integrate`が検証コマンドを実行するときに`Command`のenvで渡す。reviewのheadless実行にも同じenvを渡す。
   - runtimeが読むのはmain checkoutの作業ファイルの`dagq.toml`で、runのworktreeのものではない。
   - この repositoryではtargetを共有せず、`dagq.toml`も置かない（2026-09-23のユーザーの決定）。共有した`CARGO_TARGET_DIR`では、`CARGO_BIN_EXE_dagq`をexecするtestが並行する別のrunのbuildが上書きしたバイナリを実行しうるうえ、同時の`cargo llvm-cov`がprofrawを消し合ってcoverageの関門が誤るため。buildの共有はsccacheなど安全な方法を別途検討する。
4. **taskに5段階の優先度を持たせ、claim順を「効く優先度 → goalのrank → 解放数 → ID」にする。**
   - **優先度は5段階のenum**にする。

     | 名前 | DBの整数 | 意味 |
     | --- | --- | --- |
     | `interrupt` | 4 | 割り込み。他のreadyより必ず先（例: goal 28のtask） |
     | `urgent` | 3 | 運用を止めている不具合 |
     | `high` | 2 | 早め。他の作業の前提になる整理 |
     | `normal` | 1 | 既定 |
     | `low` | 0 | 後回し。条件付きの提案など |

     CLIの引数とJSONの出力は名前（小文字）で扱い、数値は受け付けない。DBでは整数で持ち、既定値は1（`normal`）、CHECK制約で0〜4に限る。domainのenumは同じ順（`low` < `normal` < `high` < `urgent` < `interrupt`）の`Ord`を持つ。
   - **付け方と変え方**: `add --priority LEVEL`で付け、`set-priority TASK LEVEL`で変える。変えられるのはtaskが`draft` / `ready`の間だけで、claimされた後（`in_progress`以降）は拒否する。走っているrunを割り込みで止めることはしない。優先度の変更は次のclaimにだけ効く。
   - **効く優先度**: 自分と、自分を推移的に待っている`ready`のtaskの優先度の最大値。待っているtaskは解放数（`unblocks`）と同じ依存グラフ（task依存と、ADR-0038のgoal依存を所属taskへ展開したもの）で辿る。`draft`・`canceled`・`completed`のtaskと、draftのgoalに属するtaskからは継承しない。draftに退避したtaskや流す予定の無いtaskが依存元を押し上げないため。優先度の高いreadyのtaskが待っている依存元は、その優先度で先にclaimされる。
   - **claim順**は次の順に比べる。
     1. 効く優先度の降順
     2. goalの`rank`（goal 13で入れる。未実装の間は飛ばす）
     3. 解放数（`unblocks`）の降順。依存の推移閉包で、未完了のtaskだけを数える
     4. IDの昇順
   - **順序の判定は1か所**: application / domainの純粋関数1つにまとめ、`candidates`・`graph`・supervisorの`fill_slots`が共有する。`graph`は未完了（`completed` / `canceled`でない）taskの依存木と各taskの解放数、claim順に並べた`candidates`、`critical`の鎖を返す。
   - 順序は`graph`で再現できるので、登録順と違う順でclaimしても`claim_reordered`は記録しない（ADR-0023の決定4の`claim_reordered`は採らない）。
   - **飢餓への対策は入れない**: 優先度の低いtaskがいつまでもclaimされない（飢餓）ことへの対策（待ち時間で優先度を上げるagingなど）は入れない。優先度は人が意図して付けるものなので、低い優先度のtaskが待つことも人が分かって選んでいる。待ちが問題になれば人が`set-priority`で上げられる。必要になれば、observerが「readyのまま長く待つtask」をnoteにする形で足す。
5. **`stats [--since <cursor>]`で時間と閾値超えを返す。**
   - run単位: claim→receipt（作業）、receipt→`validation_finished`（validating）、`validation_finished`→`integrated`（着地待ち）、resume回数、reviewのverdict。
   - goal単位: 上の各区間の合計と中央値。
   - 閾値超え: `awaiting_integration`が15分を超えたrun、3回目の`needs_session`、60分答えられていないask、同じtaskの`failed`が2回、作業時間がそのgoalの中央値の2倍を超えたrun、空きslotがあるのにcandidatesがゼロの状態。
   - `--since`は`status` / `events --after`と同じcursorを受ける。集計はrun_eventsから再導出し、新しい表は持たない。observerは`stats --since <前回のcursor>`を入力にする（ADR-0024の決定4）。

決定1〜3と5は実装済み（goal 11とその後のtask）。決定4の優先度はgoal 28の後続taskが実装する。本ADRの時点では、claim順は「解放数の降順 → IDの昇順」のまま。

## Alternatives

- **`validating`で検証を残す**（ADR-0023から）: worker直後に失敗を見つけられるが、同じcommitに対して2回走り、1回2〜6分のCPUと時間を毎run払う。失敗は`integrate`の`needs_session`と自動resumeで同じsessionが直せるので、2回分のコストに見合わない。
- **reviewを常駐sessionのsubagentのままにする**（ADR-0023から）: 実装は要らないが、sessionが起きてレビューを回すまで着地が進まず、着地待ちが人とsessionの都合に左右される。
- **`CARGO_TARGET_DIR`を共有する**（ADR-0023の決定3の当初の案）: buildのcacheは効くが、決定3に書いた2つの理由でtestとcoverageの関門が誤りうる。
- **claim順を登録順のままにし、plannerが登録順で並列度を調整する**（ADR-0023から）: 依存の追加やcancelで最適な順が変わるたびに登録し直すことになる。解放数はqueueから再計算できるので、runtimeが持つ。
- **優先度を自由な整数にする**: 付けるたびに他のtaskより少し大きい数を選ぶ上げ合いになり、値の意味（どの数ならどれくらい急ぐか）が定まらない。人とplannerが同じ基準で付けられるよう、意味を名前で持つ段階にする。
- **-3〜3などの範囲付きの整数にする**: 上げ合いの上限はできるが、各値の意味は依然として決まらず、名前の無い数字を覚えることになる。5段階で運用の場面（割り込み、運用停止、前提の整理、既定、後回し）を覆えるので、段階に名前を付けたenumにする。
- **goalのrankを優先度より先に比べる**: goalの間の順序を常に優先するので、goalの中の急ぎのtask（運用を止めている不具合など）がrankの低いgoalに属すると後ろに回る。優先度は人がtask単位で付ける明示の指示なので、最初に比べる。goal 13のtask 112（draft）はrankを先頭に置く前提で書かれているので、goal 13を始めるときにこの順に合わせて作り直す。
- **優先度を継承しない**: 急ぎのtaskがreadyでも、その依存元が`normal`なら依存元は他の`normal`の後ろに回り、急ぎのtaskはいつまでも着手できない。依存元にも同じ優先度を付けて回る手間は、依存が深いほど増える。
- **draftのtaskからも継承する**: draftに退避したtaskや、まだ流すか決めていないtaskが依存元を押し上げる。goal 28のために37のtaskをdraftに退避したような運用で、退避したtaskの優先度が残りのclaim順を乱す。
- **`add`のときだけ付けられる**: 状況が変わって急ぐことになったtaskに優先度を付けるには、登録し直すことになる。draft / readyの間は依存やgoalの所属と同じく変えられるようにする。
- **走っているrunを割り込みで止める**: `interrupt`のtaskのために走行中のrunを止めると、作業を捨てるかsessionの保存と再開が要る。slotが空くのを待てば次のclaimで先頭に来るので、割り込みはclaim順だけにする。
- **待ち時間で優先度を上げる（aging）**: 飢餓は防げるが、人が付けた優先度の意味が時間で変わり、どのtaskが次に取られるかを`graph`だけで読めなくなる。

## Consequences

- ADR-0023は`superseded`になり、本ADRが検証の1回化・supervisorのreview・run env・claim順・statsの現行の決定を1本で持つ。ADR-0023の決定2の常駐sessionの記述、決定3の`CARGO_TARGET_DIR`の共有、決定4の`claim_reordered`は本ADRでは採らない。
- ADR-0024・0027・0029・0032・0034・0037・0038の本文がADR-0023（とその決定の番号）を参照している箇所は、本ADRの同じ番号の決定として読める（番号をそろえた。本文はADR-0035の決定6のとおり書き換えない）。
- taskに`priority`の列が加わり（schemaの`user_version`が上がる）、`add` / `set-priority`、`list` / `show` / `graph`の出力に優先度が加わる。優先度の変更はtaskのeventとして記録する（kind名は実装taskが決める）。
- `candidates`・`graph`・supervisorのclaimが同じ純粋関数の順を使うので、plannerは`graph`で次にclaimされるtaskを読める。
- 低い優先度のtaskは、それより高い効く優先度のreadyのtaskがある限りclaimされない。飢餓はruntimeが防がず、人とobserverが気づく前提になる。
- goal 13のrankは、効く優先度の次・解放数の前に比べる位置で入る。
- [supervisor-lifecycle](../design/supervisor-lifecycle.md)、[domain-model](../design/domain-model.md)、[persistence](../design/persistence.md)とpluginのskill（各段階の意味と使い方）は、goal 28の各実装taskで更新する。
