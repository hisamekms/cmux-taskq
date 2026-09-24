---
id: adr-0023
type: adr
title: 検証をintegrateの1回にし、reviewをsupervisorの工程にしてpassなら着地し、dagq.tomlでrunのenvを渡し、graphと解放数でclaim順を決め、statsで詰まりを数える
status: superseded
created: 2026-09-23
updated: 2026-09-23
accepted_on: 2026-09-23
superseded_by: adr-0040
superseded_on: 2026-09-25
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - operations
  - performance
related:
  - adr-0008
  - adr-0016
  - adr-0019
  - adr-0022
  - design-supervisor-lifecycle
  - design-persistence
  - design-provider-lifecycle
---

# ADR-0023: 検証をintegrateの1回にし、reviewをsupervisorの工程にしてpassなら着地し、dagq.tomlでrunのenvを渡し、graphと解放数でclaim順を決め、statsで詰まりを数える

> **置き換え済み（2026-09-25）**: このADRの決定は現在有効ではない。現行の決定は[ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)を読む。

## Context

2026-09-23の20 runのrun_eventsを集計すると、時間の使い方は次のとおりだった。

- **workerの作業**は中央値17分（合計318分）。**着地待ち**（`awaiting_integration`から`integrated`まで）は中央値16.5分・最大60分（合計270分）で、作業時間とほぼ同量。着地待ちの大半はmaintainerのsubagentレビューと人の承認待ち。
- **検証が2回走る。** supervisorの`validating`が`verification_commands`を実行し（`src/runtime.rs`のverificationのloop）、`integrate`がrebase後にもう一度実行する。task 48でrebaseがheadを動かさないときは`integrate`の再検証をskipするようにしたが（`integration_verification_skipped`）、mainが先に進んでいればrebaseはheadを動かすので、並列運用ではほとんどの着地で2回走る。1回2〜6分、task 69は合計12分。
- **build cacheが無い。** worktreeごとに`target/`を一から作るので、`cargo test`が57〜188秒、`cargo llvm-cov`が113〜231秒。run dirの合計は6.2 GB。
- **丸ごとの再実行。** 失敗してrunを作り直したtaskが4つあり、各13〜29分の作業を捨てた。task 64は`llvm-cov`の閾値未達だけで22分かけて再実行した。
- **claim順が並列度を見ていない。** supervisorの`fill_slots`は`candidates()`（`READY_QUERY`、`ORDER BY t.id`）の登録順でclaimする。goal 8は5 taskがtask 65待ちで並んだが、plannerにもsupervisorにも、どのtaskを先に流せば後ろが解放されるかが見えない。
- **数字を取るのが手作業。** 上の集計は`events`の出力を手で突き合わせて作った。

[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)は着地を「レビューが通ればmaintainerが`integrate`を呼び、疑義のあるときだけ`approve_landing`のaskで聞く」に改めたが、レビューはmaintainerのsubagentのままで、maintainerが`watch`で起きてレビューを回すまで着地は進まない。

## Decision

**原則。** 同じcommitに対する高価な処理は1回にし、判断を含まない待ちはsupervisorの工程にする。run_eventsのkindは追加だけで、既存のkind名とpayloadは変えない。schemaを変えるtaskは`user_version`を上げて`migrations/`に追加する。以下の5点を決める。

1. **`validating`はreceiptの照合だけを行い、`verification_commands`は`integrate`のrebase後に必ず1回走らせる。**
   - `validating`が見るのは、receiptの整合（形式、`run_id`、`result`）、commitがrun branchのheadで`base_commit`の子孫であること、worktreeがcleanであること、taskが要求するevidence（[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)の決定5）だけ。`verification_command`のeventは`validating`では記録しなくなる。
   - `integrate`はrebaseの後、headが動いたかどうかにかかわらず`verification_commands`を1回実行する。task 48の「rebaseがno-opなら再検証しない」判定は廃止する。event kindの`integration_verification_skipped`は消さず、以後は記録されないだけにする。`integrate`の出力の`verification_skipped`fieldも消さず、常に`false`を返す。
   - 検証の失敗は既存どおりrunを`needs_session`にし、goal 8の自動resume（[ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)の決定1）が同じsessionで直す。runを作り直さないので、作業を捨てない。
   - 1つのcommitに対する`verification_commands`の実行は`integrate`の1回だけになり、`validating`の所要時間はreceiptの照合だけになる。
2. **reviewをsupervisorの工程にし、passならsupervisorが着地させる。**
   - `awaiting_integration`になったrunに対し、supervisorは`review ID`（task 60の`review.md`生成）を実行し、`review_started`を記録する。
   - review本体は`AgentProvider`のportに足すheadless実行で行う。Claudeでは`claude -p`をrun dirの設定（`settings`）で起動し、promptに`review.md`のpathとverdictのJSON schemaを渡し、stdoutのJSONを読む。cmux workspaceは作らない。
   - reviewが見る観点は[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定3の疑義と同じにする: receiptや差分が受け入れ条件と食い違う、taskの指示にない変更を含む、レビューとしての指摘がある。どれかがあればconcernにする。
   - verdictのJSONは`{"verdict": "pass" | "concern", "reasons": [...], "summary": "..."}`に固定し、`review_finished`のpayloadに記録する。
   - **pass**: supervisorが着地させる。着地は`integrate`と同じland関数と単一の着地slotを使い、push（ADR-0019の決定3）まで行う。rebaseの衝突や検証の失敗は`integrate`と同じく`needs_session`になり、自動resumeの後はADR-0019の`integration_approved`と同じく承認済みとして再び着地に進む。
   - **concern**: [ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の`approve_landing`のaskを作り（goal 10）、`reasons`と`summary`をquestionに載せて待つ。ADR-0022の決定3のとおり、answerが着地を認めればmaintainerが`integrate`を呼んで着地させ、認めなければ差し戻すかtaskをcancelする。
   - **headless実行の失敗**（起動できない、timeout、stdoutがschemaに合わない）: runを`awaiting_integration`のまま残し、`review_failed`をattentionとして記録する（next: review by hand）。maintainerが従来どおりsubagentでレビューし、`integrate`を呼ぶ。
3. **repository rootの`dagq.toml`の`[run.env]`をworkerと検証に渡す。**
   - `dagq.toml`は当面`[run.env]`だけを持つ（キーが環境変数名、値が文字列）。それ以外の設定を足すときは別のADRで決める。
   - 値には`${DAGQ_QUEUE_DIR}`（そのrepositoryのqueue directory）を展開できる。それ以外の変数は展開しない。
   - supervisorはworkerのworkspaceを作るときにgoal 9の`workspace --env`で渡し、`validating`・`integrate`が検証コマンドを実行するときに`Command`のenvで渡す。reviewのheadless実行にも同じenvを渡す。
   - この repositoryでは`CARGO_TARGET_DIR = "${DAGQ_QUEUE_DIR}/target"`を置き、worktree間でbuild cacheを共有する。共有した状態の`cargo test` / `llvm-cov`の所要時間を決定5の`stats`で単独buildと比べる。
4. **`graph`で依存の木と解放数を出し、supervisorのclaim順を解放数の多い順にする。**
   - `graph`は未完了（`completed` / `cancelled`でない）taskの依存木と、各taskが完了すると解放されるtask数を返す。解放数は依存の推移閉包で数える（直接の後続だけでなく、その後続に依存するtaskも含む。未完了のtaskだけを数える）。
   - supervisorの`fill_slots`はcandidatesを解放数の多い順に、同数なら登録順（task id）にclaimする。登録順と違う順でclaimしたときは`claim_reordered`を記録する。
5. **`stats [--since <cursor>]`で時間と閾値超えを返す。**
   - run単位: claim→receipt（作業）、receipt→`validation_finished`（validating）、`validation_finished`→`integrated`（着地待ち）、resume回数、reviewのverdict。
   - goal単位: 上の各区間の合計と中央値。
   - 閾値超え: `awaiting_integration`が15分を超えたrun、3回目の`needs_session`、60分答えられていないask、同じtaskの`failed`が2回、作業時間がそのgoalの中央値の2倍を超えたrun、空きslotがあるのにcandidatesがゼロの時間帯。
   - `--since`は`status` / `events --after`と同じcursorを受ける。集計はrun_eventsから再導出し、新しい表は持たない。

実装はgoal 11の後続taskが行う。本ADRの時点では未実装。

## Alternatives

- **`validating`で検証を残す**: worker直後に失敗を見つけられる（早期発見）が、同じcommitに対して2回走り、1回2〜6分のCPUと時間を毎run払う。早期発見で節約できるのは失敗したrunの着地待ちだけで、失敗は`integrate`の`needs_session`と自動resumeで同じsessionが直せるので、2回分のコストに見合わない。
- **reviewをmaintainerのsubagentのままにする**: 実装は要らないが、maintainerが`watch`で起きてレビューを回すまで着地が進まず、着地待ち（中央値16.5分）が人とmaintainerの都合に左右されたままになる。
- **sccacheでbuild cacheを共有する**: crateの単位でcacheでき`llvm-cov`のinstrumentもcacheできるが、導入の依存（sccacheのinstallとwrapperの設定）が増える。まず`CARGO_TARGET_DIR`の共有で所要時間を測り、足りなければ別のADRで検討する。
- **claim順を登録順のままにし、plannerが登録順で並列度を調整する**: runtimeの変更は要らないが、依存の追加やcancelで最適な順が変わるたびに登録し直すことになる。解放数はqueueから再計算できるので、runtimeが持つ。

## Consequences

- [ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md)のConsequencesの「runtimeが自発的に`integrate`を呼ぶことはなく、承認済みのrunの再着地だけを引き受ける」は、「承認済みのrunの再着地に加え、reviewがpassしたrunはsupervisorが着地させる」に改まる。ADR-0019のAlternativesが見送った「承認なしの自動着地」は、headlessのreviewがpassしたrunに限って採る。
- [ADR-0016](0016-maintainer-notification-and-compact-output.md)の決定5（`integrate`は自動で呼ばれない、着地の承認はユーザーに残す）は、[ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md)（goal 10）で「疑義のあるときだけ`approve_landing`で聞く」に改めた前提の上で、さらにレビューの主体をmaintainerからsupervisorに移す。ADR-0022の決定3の「runtimeが自発的に`integrate`を呼ばないことは維持する」は、reviewがpassしたrunについて本ADRで改める。
- maintainerの担当から通常のレビューと着地が外れ、`review_failed`とconcernのaskへの対応が残る。ADR-0016の決定7（maintainerが`review.md`をsubagentに渡してレビューする）は`review_failed`のときの手作業の手順になり、決定2の`awaiting_integration`のattentionはreviewの結果が出るまでmaintainerを起こす必要が無くなる（attentionの判定の変更は実装taskが決める）。
- [ADR-0008](0008-merge-queue-squash-landing.md)の再検証は常に行われるようになり、task 48のskip判定は無くなる。
- run_eventsのkindに`review_started` / `review_finished` / `review_failed` / `claim_reordered`が加わる。`verification_command`は`integrate`の中でだけ記録される。
- `dagq.toml`がrepository rootに加わる。`CARGO_TARGET_DIR`を共有すると、並列のrunが同じ`target/`のlockを取り合う。`stats`の所要時間でこの待ちを含めて単独buildと比べる。
- [supervisor-lifecycle](../design/supervisor-lifecycle.md)、[persistence](../design/persistence.md)、[provider-lifecycle](../design/provider-lifecycle.md)は各実装taskで更新する。
