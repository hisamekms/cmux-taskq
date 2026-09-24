---
id: adr-0034
type: adr
title: ドメインeventに失敗・保留・中断の分類コード、実行者（actor）、設定の変更を持たせ、goal 21ではその前提（分類コード、taskのkind、versionと実行条件と所要時間とコストの記録）を実装する
status: proposed
created: 2026-09-24
updated: 2026-09-24
owners:
  - hisamekms
tags:
  - runtime
  - observability
  - domain
related:
  - adr-0014
  - adr-0016
  - adr-0023
  - adr-0024
  - adr-0029
  - adr-0032
  - adr-0033
  - design-domain-model
  - design-persistence
---

# ADR-0034: ドメインeventに失敗・保留・中断の分類コード、実行者（actor）、設定の変更を持たせ、goal 21ではその前提（分類コード、taskのkind、versionと実行条件と所要時間とコストの記録）を実装する

## Context

2026-09-24に86 runの記録を調べた（goal 21）。改善プラン（着地の衝突、cmuxのcaptureのtimeout、exit 143）を立てるには、失敗がどの種類かを数えられる必要があった。今の記録では次が足りない。

- 失敗・保留・中断の理由（`task_runs.last_error`、`validation_finished` / `integration_deferred`などの`reason`、`runtime_error`の`message`）が、pathやworkspace IDを含む自由文で、分類コードが無い。数えるには文字列を目で分けるしかない。
- taskの変更の種類（runtime / docs / pluginなど）が無く、titleの接頭辞（`runtime:` / `docs:` / `skill:` / `application:` …）から推定していて揺れる。[ADR-0029](0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)の`--paths`は変えてよい範囲で、種類そのものではない。
- runがどの条件で走ったかが無い: dagqとClaude Codeのversion、claim時の`parallel`とload、検証コマンドごとの所要時間、workerのトークン数とコスト。`supervisors.binary_version`は今のsupervisorの値で、過去のrunがどのversionで走ったかは残らない。
- 誰がそのeventを起こしたか（人か、inbox / plannerのsessionか、supervisorか、jobか）が、`asked_by`・`by`のようにkindごとにばらばらで、多くのkindには無い。
- `dagq.toml`の変更や固定バイナリの入替（[ADR-0014](0014-up-replaces-a-supervisor-of-another-binary-version.md)）の前後で振る舞いが変わっても、いつ変わったかがqueueに残らない。

[ADR-0032](0032-classify-records-into-domain-events-diagnostics-coordination-and-bodies.md)で、ドメインeventを将来Webに送る正本・監査の正本とし、自由文とマシン依存の値は診断に分けると決めた。分類コードは、自由文を診断へ落としてもWeb側で失敗を数えられるようにするための対になる。

## Decision

1. **失敗・保留・中断の理由に分類コードを持たせる。**
   - runが`failed` / `interrupted` / `needs_session`になる遷移と、reasonやerrorを持つevent（`validation_finished`、`scope_violation`、`evidence_missing`、`integration_deferred`、`integration_failed`、`integration_error`、`runtime_error`、`review_failed`、`triage_failed`、`resume_finished`の`error`、`push_failed`、`backend_call_failed`、`supervision_finished`の異常終了、`integration_rebase_aborted`、`conflict_precheck`の`error`、`revise_receipt_rejected` / `conflict_receipt_rejected`、`ask_delivery_failed`、`screen_capture_failed`、`cleanup_failed`、`exit_request_timed_out`、`wrapper_heartbeat_expired`）のpayloadに`reason_code`を足す。reasonの項目を持たない`exit_request_timed_out`と`wrapper_heartbeat_expired`も、それ自体が失敗の記録なので`reason_code`を持つ。今の`reason` / `message` / `error`の自由文は項目も文言も変えずに残す（既存CLIの出力を変えない、[ADR-0016](0016-maintainer-notification-and-compact-output.md)）。
   - runの`last_error`と対に`last_error_code`を持ち、`status`・`show`・`stats`が`reason_code`を項目として足して出す。`stats`はコードごとの件数を出せるようにする。
   - コードはsnake_caseの閉じた集合で、domainに定数として置き、kind名と同じ公開契約にする。種類で前置きしない短い名前にし、どの工程で起きたかはkindが持つ。当てはまらないものは`other`にして自由文を診断に残し、`other`が増えたらコードを足す。
   - 初期の集合は実装taskが今のreasonの文字列と`last_error`の実データから決める。調査で改善プランに要ったものは必ず分ける: 着地のrebaseの衝突、検証コマンドの失敗、receiptの不備（無い・壊れている・commitが違う・worktreeが汚い）、要求evidenceの欠落、宣言外のpathの変更、cmuxの呼び出しのtimeoutと失敗（`op`は別の項目）、sessionのsignalによる終了（exit 143など）、`/exit`の応答なし、heartbeatの途絶、reviewとtriageのjobの失敗、pushの失敗。

2. **taskに種類（`kind`）を持たせる。** `add --kind <kind>`で登録し、draft / readyのうちは変えられる（`set-paths`と同じ扱い）。初期の値は`runtime`・`plugin`・`docs`・`ops`（CIやリリースなどrepositoryの運用）で、前の3つはADR-0029の推奨の検証の組み合わせと対応する（`tests/`だけを変えるtaskもAGENTS.mdのとおり`runtime`）。`ops`は対応する推奨の検証を持たない。titleの接頭辞からは推定しない。既存のtaskと`--kind`の無いtaskは未設定（null）とし、推定値で埋めない。`list`・`show`・`stats`・receiptのtask情報に項目として足す。`kind`は`--paths`や`verification_commands`を自動では決めない（決めるかは使ってから判断する）。

3. **runの実行条件と所要時間とコストを記録する。** どれもドメインeventの数値か文字列で、マシン依存の値ではない（ADR-0032の決定1）。
   - claim時: dagqのversion（`CARGO_PKG_VERSION`）、Claude Codeのversion、`parallel`、1分のload average（取れなければnull）。`run_claimed`のpayloadに足す。
   - 検証: `verification_command`に`duration_secs`を足す（[ADR-0023](0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)の`integrate`の1回の検証ごと）。
   - workerのトークン数とコスト: sessionの終わりに、入力・出力・cache読み書きのトークン数とコスト（USD）を記録する。取り方（workerのsessionのtranscriptのusageを集計するか、Claude Codeのhookから得るか）と、`supervision_finished`に足すか新しいkindにするかは実装taskで決める。headlessのreview / triage / observerのjobも同じ形で記録してよい。取れなかったときはnullにし、runを止めない。

4. **実行者（actor）を持たせる。（将来）** すべてのドメインeventに、誰が起こしたかを同じ形で持たせる: `role`（`person` / `inbox` / `planner` / `supervisor` / `observer` / `worker` / `reviewer`。triage jobはsupervisorの工程なので`supervisor`、reviewのheadless jobは`reviewer`）、binaryのversion、マシン。マシンはhostnameではなく、マシンごとに1つ作る不透明なID（UUID。queueではなくユーザーのdata directoryに置き、同じマシンの全queueで共有する）で表し、ADR-0032の「マシン依存の値を持たない」に反しない形にする。CLIを人が打ったか、inbox / plannerのsessionが打ったかは`DAGQ_ROLE`から判定し、envが無ければ`person`とする。今の`asked_by`・`by`は残し、actorは項目の追加として足す。

5. **設定の変更をeventにする。（将来）**
   - `dagq.toml`: supervisorの起動時とclaim時に内容のhashを見て、前回と違えば`config_changed`をqueueのeventとして記録する。変わったsectionとkeyの名前とhashを持ち、値（特に`[run.env]`）は記録しない。
   - binaryの入替: supervisorの起動時に、前回のsupervisorの`binary_version`と違えば`binary_replaced`（前と後のversion、入替が`up`のdrainによるものか）を記録する（ADR-0014の入替の経過が残る）。
   - どちらも今の`run_events`はqueue単位のevent（taskもgoalも無い）を`backend_call_failed`と`observe_*`で受け付けているので、schemaを変えずに入る。

6. **goal 21の範囲。** 決定1・2・3を実装する。決定4（actor）と決定5（設定変更のevent）は将来のtaskとし、goal 21では実装しない。決定4・5の設計は、Webを協調役にするときの要件（ADR-0032の決定4）と合わせて見直してよい。

## Alternatives

- **自由文のreasonを正規表現で分類する`stats`だけを作る。** 文言が変わるたびに分類が壊れ、pathを含む文字列をWebに送ることになる。記録する時点でコードを決める方が確かで、送るときに自由文を落とせる。
- **既存の`reason`を分類コードに置き換える。** 既存CLIの出力と、`reason`を読む`watch` / `status`の利用者を壊す。項目の追加にする。
- **taskの種類を`--paths`から推定する。** pathの宣言はtaskによって無く、あっても種類とは一致しない（runtimeのtaskは`--paths`を付けない、ADR-0029）。明示の値にする。
- **actorと設定変更もgoal 21で実装する。** actorは全kindの書き込み口に手が入り、マシンIDの作り方とWeb側の要件が決まっていない。goal 21は障害調査に今すぐ効く分類コードと不足していた値に絞る。

## Consequences

- 失敗の種類ごとの件数が`stats`から出て、改善プランを事実から立てられる。自由文は診断として残り、ADR-0033のJSON Linesで詳細を追える。
- 分類コードの集合はkind名と同じ公開契約になり、変えるときはADRが要る。`other`の件数を見てコードを足す運用が要る。
- `run_claimed`・`verification_command`などのpayloadに項目が増える。既存の項目と文言は変わらない。taskの`kind`とrunの`last_error_code`はschemaの追加（migration）で、固定バイナリの入替を伴う（AGENTS.md）。
- トークン数とコストの取り方はClaude Codeの出力の形に依存し、変わったら実装を追従させる。
- actorと設定変更のeventが入るまでは、誰が・どの設定で起こしたかは今のとおり`asked_by` / `by`と`supervisors`の現在値から推すしかない。
