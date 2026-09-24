---
id: adr-index
type: design
title: Architecture decision records
status: current
created: 2026-09-21
updated: 2026-09-24
last_verified: 2026-09-24
tags:
  - architecture
  - documentation
---

# Architecture decision records

ADRは、将来の実装や運用に大きな影響を与える決定の理由を残す。決定を変更するときは既存ADRを削除・書き換えず、新しいADRを追加する。

## Status

- `proposed`: 検討中
- `accepted`: 採用済み
- `rejected`: 不採用
- `superseded`: 新しいADRに置き換え済み

新しいADRは [template](0000-template.md) をコピーして作る。

## 一覧

- [ADR-0001](0001-rust-runtime.md): Rustでruntimeを実装する（accepted）
- [ADR-0002](0002-cmux-first.md): cmuxを最初のworkspace backendにする（accepted）
- [ADR-0003](0003-supervisor-owns-lifecycle.md): supervisorがagentとworkspaceのライフサイクルを所有する（accepted）
- [ADR-0004](0004-agent-provider-abstraction.md): ClaudeとCodexをagent providerとして抽象化する（accepted）
- [ADR-0005](0005-binary-and-plugin-distribution.md): runtimeをバイナリ、agent integrationをpluginとして配布する（accepted）
- [ADR-0006](0006-queue-per-repository.md): repositoryごとに1つのqueueをユーザーのデータディレクトリに置き、cwdから解決する（accepted）
- [ADR-0007](0007-run-level-leases-parallel-execution.md): leaseをrun単位にし、依存が解けたtaskを上限付きで並列に実行する（accepted）
- [ADR-0008](0008-merge-queue-squash-landing.md): runtimeのmerge queueが最新mainへrebase・再検証し、1 task = 1 commitにsquashしてmainへ着地させる（accepted）
- [ADR-0009](0009-goal-groups-tasks.md): 複数のtaskが解く上位の課題をgoalとして表現し、workerのpromptに流す（proposed）
- [ADR-0010](0010-maintainer-and-resident-supervisor.md): 役割名をsupervisor / maintainer / workerに統一し、supervisorをlaunchdで常駐させてupとdownで起動・停止する（accepted）
- [ADR-0011](0011-cmux-socket-password-and-in-cmux-fallback.md): launchd常駐のsupervisorにはcmuxのsocket passwordを前提とし、up --in-cmuxをlaunchdなしのfallbackにする（accepted）
- [ADR-0012](0012-adopt-stale-lease-of-live-wrapper.md): supervisorが死んだrunは、wrapperが生きていれば次のsupervisorが引き継ぐ（accepted）
- [ADR-0013](0013-layered-architecture-and-type-function-style.md): domain / application / infrastructureのレイヤーと「型＋関数」でruntimeを構成する（accepted）
- [ADR-0014](0014-up-replaces-a-supervisor-of-another-binary-version.md): upはbinary versionの違うsupervisorをdrainして入れ替える（accepted）
- [ADR-0015](0015-rename-to-dagq.md): cmux-taskqをdagqに改名する（accepted）
- [ADR-0016](0016-maintainer-notification-and-compact-output.md): maintainerを使い捨てのsessionにし、status / watch / doctorの通知経路と圧縮出力、pluginの起き直しhookを持たせる（accepted）
- [ADR-0017](0017-resolve-run-paths-from-the-queue-directory.md): runのqueue配下のpathは読むたびにqueueディレクトリとrun IDから解決する（accepted）
- [ADR-0018](0018-run-workspace-named-after-the-task.md): runのcmux workspace名はtaskのtitleにし、run IDはdescriptionに置く（accepted）
- [ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md): maintainerの定型作業をruntimeに移す（needs_sessionの自動resume、exit timeoutで放棄しない、push、follow_ups、evidence、prompt待ち）（accepted）
- [ADR-0020](0020-rebind-queue-to-a-moved-repository.md): repositoryの移動はrebindサブコマンドでqueueの束縛を付け替える（accepted）
- [ADR-0021](0021-maintainer-and-supervisor-workspace-names-follow-the-run-style.md): maintainer / supervisor / resumeのcmux workspace名もrunと同じ`[<repo>]dagq <role>`にそろえる（accepted）
- [ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md): 相談をqueueのask / answerにし、upがinboxとplannerを開き、着地は疑義のあるときだけ人に聞き、cmux notifyはinbox宛てにする（accepted）
- [ADR-0023](0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md): 検証をintegrateの1回にし、reviewをsupervisorの工程にしてpassなら着地し、dagq.tomlでrunのenvを渡し、graphと解放数でclaim順を決め、statsで詰まりを数える（accepted）
- [ADR-0024](0024-retire-maintainer-into-jobs-and-observer.md): maintainerを退役させ、review・triage・observerをheadlessのjobにし、observerの権限をnoteとdraft goalとaskに限り、goalにdraft状態を足す（accepted）
- [ADR-0025](0025-leaseless-unfinished-run-is-a-recover-run-attention.md): supervisorが手放した未完了runをattention（recover run）にする（accepted）
- [ADR-0026](0026-identify-workspaces-by-uuid-env-and-queue-group.md): cmux workspaceをtitleではなくqueue DBのUUIDで識別し、roleとqueueを--envで持たせ、queueごとのworkspace groupにまとめる（accepted）
- [ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md): workerのsessionをreviewの後まで残し、機械的な指摘はrevise verdictで生きているworkerに返し、着地前にmerge-treeで衝突を事前判定する（accepted）
- [ADR-0028](0028-workspace-titles-are-repo-and-role.md): cmux workspaceのtitleを`[<repo>]<role>`にし、planner / inboxの名前とrole値を定義する（accepted）
- [ADR-0029](0029-task-declares-paths-and-verification-follows-the-kind-of-change.md): taskが変更してよいパス（add --paths）を宣言し、validatingとintegrateが宣言外の変更を拒否し、verification_commandsを変更の種類で軽くする（accepted）
- [ADR-0030](0030-publish-to-crates-io-on-tag-push-with-trusted-publishing.md): crates.ioを追加の配布経路にし、tag pushでTrusted Publishingによって自動でpublishする（accepted）
- [ADR-0031](0031-color-pill-and-pin-for-inbox-and-planner-and-unpin-before-close.md): upがinbox / plannerのworkspaceに役割の色・status pill・ピンを当て、dagqのworkspace closeはピンを外してから閉じる（accepted）
- [ADR-0032](0032-classify-records-into-domain-events-diagnostics-coordination-and-bodies.md): 記録をドメインevent・診断telemetry・協調状態・本文の4つに分類し、それぞれの送り先（Web / OTLP / ローカル）を決める（proposed）
- [ADR-0033](0033-one-tracing-pipeline-with-local-json-lines-and-optional-otlp.md): 計測をRustのtracingの1系統にし、出口としてローカルのJSON Lines（常に有効）とOTLP（既定は無効）を足す（proposed）
- [ADR-0034](0034-domain-events-carry-reason-codes-actor-and-configuration-changes.md): ドメインeventに失敗・保留・中断の分類コード、実行者（actor）、設定の変更を持たせ、goal 21ではその前提（分類コード、taskのkind、versionと実行条件と所要時間とコストの記録）を実装する（proposed）
- [ADR-0035](0035-follow-up-triage-job-decides-follow-up-drafts.md): follow_upのdraft taskの採否をsupervisorが起動するheadlessのfollow-up triage jobが決め、判断がつかないものだけinboxで人に聞く（accepted）
