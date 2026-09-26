---
id: adr-index
type: design
title: Architecture decision records
status: current
created: 2026-09-21
updated: 2026-09-27
last_verified: 2026-09-27
tags:
  - architecture
  - documentation
---

# Architecture decision records

ADRは、将来の実装や運用に大きな影響を与える決定の理由を残す。規則は[ADR-t598-1](2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)に従う。

- IDは書くtaskのIDと枝番の`adr-t<task ID>-<N>`（1本でも`-1`）、ファイル名は`<YYYY-MM-DD>-t<task ID>-<N>-<slug>.md`で、日付は`accepted_on`。参照は`ADR-t<ID>-<N>`で日付を含めない。既存の4桁の番号（0001〜）と登録済みのtaskが予約した4桁の番号はそのまま使う。
- 1 ADRに決定1つ（密に結びついた数個まで）、本文はおおむね100行以内。書くのは変えるのに人の判断が要るもの（問題と文脈、方針・原則・境界・不変条件、退けた案、結果）で、eventやflagの名前、既定値・閾値の数値、関数やファイルの名前は[docs/design/](../design/)に書く。今の姿はdesignが、なぜそうしたかはADRが持つ。
- `accepted`のADRだけが現在の決定で、本文の決定はすべて有効（`amended_by`を持つものは、その決定だけ後のADRが変えている）。`superseded`のADRは`superseded_by`を辿り、`accepted`に着くまで読む。
- 新しい形の小さなADRの決定を変えるときは、新しいADRで丸ごと置き換える。決定の多い既存のADR（0047・0044・0073など）は凍結し、一部を変えるときは小さな新しいADRの`amends`に変える決定を書き、元のADRに`amended_by`を足し、同じ変更でdesignを今の姿に直す。
- 置き換えは後継を`accepted`にする変更と同じ変更で行う。`proposed`の後継は何も置き換えない。
- 本文はappend-onlyで、後から変えてよいのはstatus・`accepted_on`・`superseded_by`・`superseded_on`・`deprecated_on`・`amended_by`とH1直後の注記1行だけ（`supersedes`と`amends`は本文と一緒に書く）。`superseded_on`は`superseded`にした日（後継の`accepted_on`と同じ）、`deprecated_on`は`deprecated`にした日。欄と注記の書式は[frontmatter仕様](../frontmatter.md)と[template](0000-template.md)にある。
- ADRのstatusを変える変更は、同じ変更でこの索引の2つの表も更新する。新しい形の行は4桁の行の後ろに`accepted_on`の順で並べる。
- `sh scripts/check-adr-numbers.sh`が4桁の番号の重複とidの食い違い、新しい形のファイル名の形・idとの一致・IDの重複・日付と`accepted_on`の一致を検査する。

## Status

- `proposed`: 検討中。決定はまだ有効ではない
- `accepted`: 採用済み。本文の決定がすべて現在有効
- `rejected`: 不採用
- `superseded`: 後継のADRに丸ごと置き換え済み（`superseded_by`が後継を指す）
- `deprecated`: 後継なしで廃止済み（H1直後の注記が理由を示す）

新しいADRは [template](0000-template.md) をコピーして作る。

## 有効なADR

`status: accepted`のADR。`accepted_on`はgit logで`status: accepted`が入ったcommitの日（ADR-0009はgoal 1で実装済みのため、ADRの棚卸しの変更で`accepted`にした日）。0001〜0034のうち後のADRに決定を上書きされたものと、それを丸ごと置き換える統合ADRの組は[ADRの棚卸し](../plans/adr-inventory.md)にあり、統合ADRが`accepted`になるときにこの表から下の対応表に移る。

| ADR | Title | accepted_on |
| --- | --- | --- |
| [ADR-0004](0004-agent-provider-abstraction.md) | ClaudeとCodexをagent providerとして抽象化する | 2026-09-22 |
| [ADR-0008](0008-merge-queue-squash-landing.md) | runtimeのmerge queueが最新mainへrebase・再検証し、1 task = 1 commitにsquashしてmainへ着地させる | 2026-09-22 |
| [ADR-0009](0009-goal-groups-tasks.md) | 複数のtaskが解く上位の課題をgoalとして表現し、workerのpromptに流す | 2026-09-25 |
| [ADR-0010](0010-maintainer-and-resident-supervisor.md) | 役割名をsupervisor / maintainer / workerに統一し、supervisorをlaunchdで常駐させてupとdownで起動・停止する | 2026-09-22 |
| [ADR-0011](0011-cmux-socket-password-and-in-cmux-fallback.md) | launchd常駐のsupervisorにはcmuxのsocket passwordを前提とし、up --in-cmuxをlaunchdなしのfallbackにする | 2026-09-22 |
| [ADR-0013](0013-layered-architecture-and-type-function-style.md) | domain / application / infrastructureのレイヤーと「型＋関数」でruntimeを構成する | 2026-09-22 |
| [ADR-0016](0016-maintainer-notification-and-compact-output.md) | maintainerを使い捨てのsessionにし、status / watch / doctorの通知経路と圧縮出力、pluginの起き直しhookを持たせる | 2026-09-23 |
| [ADR-0018](0018-run-workspace-named-after-the-task.md) | runのcmux workspace名はtaskのtitleにし、run IDはdescriptionに置く | 2026-09-23 |
| [ADR-0021](0021-maintainer-and-supervisor-workspace-names-follow-the-run-style.md) | maintainer / supervisor / resumeのcmux workspace名もrunと同じ`[<repo>]dagq <role>`にそろえる | 2026-09-23 |
| [ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md) | 相談をqueueのask / answerにし、upがinboxとplannerを開き、着地は疑義のあるときだけ人に聞き、cmux notifyはinbox宛てにする | 2026-09-23 |
| [ADR-0026](0026-identify-workspaces-by-uuid-env-and-queue-group.md) | cmux workspaceをtitleではなくqueue DBのUUIDで識別し、roleとqueueを--envで持たせ、queueごとのworkspace groupにまとめる | 2026-09-23 |
| [ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md) | workerのsessionをreviewの後まで残し、機械的な指摘はrevise verdictで生きているworkerに返し、着地前にmerge-treeで衝突を事前判定する | 2026-09-23 |
| [ADR-0028](0028-workspace-titles-are-repo-and-role.md) | cmux workspaceのtitleを`[<repo>]<role>`にし、planner / inboxの名前とrole値を定義する | 2026-09-23 |
| [ADR-0029](0029-task-declares-paths-and-verification-follows-the-kind-of-change.md) | taskが変更してよいパス（add --paths）を宣言し、validatingとintegrateが宣言外の変更を拒否し、verification_commandsを変更の種類で軽くする | 2026-09-24 |
| [ADR-0030](0030-publish-to-crates-io-on-tag-push-with-trusted-publishing.md) | crates.ioを追加の配布経路にし、tag pushでTrusted Publishingによって自動でpublishする | 2026-09-24 |
| [ADR-0031](0031-color-pill-and-pin-for-inbox-and-planner-and-unpin-before-close.md) | upがinbox / plannerのworkspaceに役割の色・status pill・ピンを当て、dagqのworkspace closeはピンを外してから閉じる | 2026-09-24 |
| [ADR-0036](0036-delete-frozen-work-records.md) | 凍結済みのdocs/journal/を削除し、今も効く手順と観測事実だけをdesign文書へ移す | 2026-09-25 |
| [ADR-0038](0038-task-depends-on-a-goal-until-it-is-achieved.md) | taskがgoalに依存でき、依存先のgoalがachievedで閉じるまでclaimされない | 2026-09-25 |
| [ADR-0039](0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md) | supervisorが死んだrunは、wrapperが生きていれば次のsupervisorが引き継ぎ、自分のtokenのままstaleになったleaseは更新して続ける | 2026-09-25 |
| [ADR-0046](0046-full-text-search-related-and-duplicate-of.md) | taskの全文検索（search）と決まった規則の関連（related）と重複の記録（cancel --duplicate-of）を持ち、plannerとplan reviewはその候補だけをLLMで判断する | 2026-09-25 |
| [ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md) | イレギュラーをruntimeの自動修正・復旧job・inboxの3層で扱い、askに人が要る理由の分類を必須にし、自動修正を数え、goalの達成をgoal review jobが判断する（ADR-0019・ADR-0043・ADR-0044を統合） | 2026-09-26 |
| [ADR-0048](0048-record-claude-sessions-by-kind-with-open-and-active-time.md) | dagqが使うClaude sessionをkindごとの区間としてrun_eventsに記録し、開いている時間と、transcriptのturnから導く稼働時間をstatsで集計する | 2026-09-26 |
| [ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md) | 検証をintegrateの1回にし、reviewをsupervisorの工程にし、dagq.tomlでrunのenvを渡してsccacheでcompileの結果をrun間で共有し、taskの5段階の優先度と解放数でclaim順を決め、statsで詰まりを数える（ADR-0040を統合） | 2026-09-26 |
| [ADR-0051](0051-kpi-time-series-report-and-push.md) | KPIを決まった規則でrun_eventsから導き、期間・taskの種類・変更の印で比べ、目標割れを判定し、supervisorが日次のHTMLとJSONのレポートを書き、ホストの設定のコマンドにpushし、KPIのfindingから作る改善のproposalの数に上限を付ける | 2026-09-26 |
| [ADR-0052](0052-rust-single-binary-and-plugin-with-cmux-first.md) | runtimeをRustの単一バイナリdagqとpluginで配り、cmuxを最初のworkspace backendにする（ADR-0001・ADR-0002・ADR-0005・ADR-0015を統合） | 2026-09-26 |
| [ADR-0053](0053-queue-in-data-dir-run-paths-from-queue-and-rebind.md) | repositoryごとのqueueをデータディレクトリに置き、runのpathをqueueから解決し、repositoryとqueueの移動をrebindで扱う（ADR-0006・ADR-0017・ADR-0020を統合） | 2026-09-26 |
| [ADR-0054](0054-run-lease-ownership-parallel-supervisors-and-recover.md) | supervisorがrun単位のleaseでrunのlifecycleを所有して並列に実行し、死んだsupervisorのrunを引き継ぎ、手放したrunをrecoverに回す（ADR-0003・ADR-0007・ADR-0025を統合） | 2026-09-26 |
| [ADR-0068](0068-recheck-waiting-runs-after-each-landing.md) | 着地のたびに着地待ちのrunをmerge-treeと軽い検査で先回りして確かめ、着地しなくなったrunは人の回答や着地の順番を待たずにresumeする | 2026-09-26 |
| [ADR-0069](0069-do-not-claim-tasks-overlapping-hot-files.md) | 衝突の多いファイルで進行中のrunと重なるtaskはそのpassでclaimせずに次の候補へ進み、控えた理由と時間をstatusとstatsに出す | 2026-09-26 |
| [ADR-0071](0071-runs-waiting-in-revise-and-resume-leave-the-slot.md) | 人の答えを待つrunを、最初のsessionと/exitに加えて差し戻しと解消依頼の段でもslotから外し、待ちのあいだ段の計時を止め、leaseを持ったまま軽く見張り、戻り待ちも含めて待ちの数に上限を付け、待ちが終わったrunを新しいclaimより先にslotへ戻す（ADR-0062を統合） | 2026-09-26 |
| [ADR-0073](0073-kind-additions-are-compatible.md) | 固定バイナリをbuild識別子で見分け、queueを開いただけではmigrateせず、互換の範囲のschemaを受け入れ、askとeventのkindの追加を互換として扱い、supervisorを待たずに引き継ぎで入れ替え、up --auto-updateで着地のたびに自動で更新する（ADR-0045を統合） | 2026-09-26 |
| [ADR-0076](0076-run-the-coverage-gate-tests-with-nextest.md) | integrateのcoverageの関門のtestをcargo-nextestでbinaryをまたいで並列に流し（cargo llvm-cov nextest）、cargo-nextestは人がhostに入れる | 2026-09-26 |
| [ADR-0078](0078-one-integration-test-binary.md) | e2eとplugin以外のintegration testを1つのtest binary（tests/it）にまとめ、testファイルの行数の制約はファイル単位のまま残す | 2026-09-26 |
| [ADR-0079](0079-record-task-weight-predictions-and-trial-model-effort-selection.md) | plan reviewでtaskの重さの予測を記録し、限定の試しでworkerのmodel / effortを選び、taskに由来する失敗で段上げする | 2026-09-26 |
| [ADR-t598-1](2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md) | ADRのIDを書くtaskのIDにし、1 ADR 1決定・記載の粒度・今の姿はdesign・大きなADRはamendsで直すと決める（ADR-0042を置き換え） | 2026-09-26 |
| [ADR-t610-1](2026-09-27-t610-1-landing-runs-fill-the-slot-in-status-and-stats.md) | statusのslots.usedとstatsのidle_slotsを、supervisorがclaimと戻りの判定に使うslotと同じ集合で数え、着地中のrunも埋まったslotに数える（ADR-0071決定12・13をamends） | 2026-09-27 |

## 置き換え・廃止されたADR

`status: superseded` / `deprecated`のADRと後継の対応。`deprecated`の行は`superseded_by`を空にし、日付の列に`deprecated_on`を書く。0001〜0034の棚卸し（後続のtask）で統合ADRが`accepted`になるときにも行が加わる。

| ADR | Status | superseded_by | superseded_on / deprecated_on |
| --- | --- | --- | --- |
| [ADR-0001](0001-rust-runtime.md) | superseded | [ADR-0052](0052-rust-single-binary-and-plugin-with-cmux-first.md) | 2026-09-26 |
| [ADR-0002](0002-cmux-first.md) | superseded | [ADR-0052](0052-rust-single-binary-and-plugin-with-cmux-first.md) | 2026-09-26 |
| [ADR-0003](0003-supervisor-owns-lifecycle.md) | superseded | [ADR-0054](0054-run-lease-ownership-parallel-supervisors-and-recover.md) | 2026-09-26 |
| [ADR-0005](0005-binary-and-plugin-distribution.md) | superseded | [ADR-0052](0052-rust-single-binary-and-plugin-with-cmux-first.md) | 2026-09-26 |
| [ADR-0006](0006-queue-per-repository.md) | superseded | [ADR-0053](0053-queue-in-data-dir-run-paths-from-queue-and-rebind.md) | 2026-09-26 |
| [ADR-0007](0007-run-level-leases-parallel-execution.md) | superseded | [ADR-0054](0054-run-lease-ownership-parallel-supervisors-and-recover.md) | 2026-09-26 |
| [ADR-0012](0012-adopt-stale-lease-of-live-wrapper.md) | superseded | [ADR-0039](0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md) | 2026-09-25 |
| [ADR-0014](0014-up-replaces-a-supervisor-of-another-binary-version.md) | superseded | [ADR-0045](0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md) | 2026-09-25 |
| [ADR-0015](0015-rename-to-dagq.md) | superseded | [ADR-0052](0052-rust-single-binary-and-plugin-with-cmux-first.md) | 2026-09-26 |
| [ADR-0017](0017-resolve-run-paths-from-the-queue-directory.md) | superseded | [ADR-0053](0053-queue-in-data-dir-run-paths-from-queue-and-rebind.md) | 2026-09-26 |
| [ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md) | superseded | [ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md) | 2026-09-26 |
| [ADR-0020](0020-rebind-queue-to-a-moved-repository.md) | superseded | [ADR-0053](0053-queue-in-data-dir-run-paths-from-queue-and-rebind.md) | 2026-09-26 |
| [ADR-0023](0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md) | superseded | [ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md) | 2026-09-25 |
| [ADR-0024](0024-retire-maintainer-into-jobs-and-observer.md) | superseded | [ADR-0041](0041-on-demand-planners-proposals-submitted-and-plan-review-job.md) | 2026-09-25 |
| [ADR-0025](0025-leaseless-unfinished-run-is-a-recover-run-attention.md) | superseded | [ADR-0054](0054-run-lease-ownership-parallel-supervisors-and-recover.md) | 2026-09-26 |
| [ADR-0035](0035-adr-is-superseded-whole-with-dates-and-banner.md) | superseded | [ADR-0042](0042-adr-is-superseded-whole-and-deprecation-date-is-deprecated-on.md) | 2026-09-25 |
| [ADR-0037](0037-follow-up-triage-job-decides-follow-up-drafts.md) | superseded | [ADR-0041](0041-on-demand-planners-proposals-submitted-and-plan-review-job.md) | 2026-09-25 |
| [ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md) | superseded | [ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md) | 2026-09-26 |
| [ADR-0041](0041-on-demand-planners-proposals-submitted-and-plan-review-job.md) | superseded | [ADR-0044](0044-findings-proposals-from-findings-and-quiet-observer.md) | 2026-09-26 |
| [ADR-0042](0042-adr-is-superseded-whole-and-deprecation-date-is-deprecated-on.md) | superseded | [ADR-t598-1](2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md) | 2026-09-26 |
| [ADR-0043](0043-detect-stalled-worker-sessions-nudge-once-then-ask.md) | superseded | [ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md) | 2026-09-26 |
| [ADR-0044](0044-findings-proposals-from-findings-and-quiet-observer.md) | superseded | [ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md) | 2026-09-26 |
| [ADR-0045](0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md) | superseded | [ADR-0073](0073-kind-additions-are-compatible.md) | 2026-09-26 |
| [ADR-0062](0062-runs-waiting-for-a-person-leave-the-slot.md) | superseded | [ADR-0071](0071-runs-waiting-in-revise-and-resume-leave-the-slot.md) | 2026-09-26 |
