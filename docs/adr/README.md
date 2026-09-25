---
id: adr-index
type: design
title: Architecture decision records
status: current
created: 2026-09-21
updated: 2026-09-25
last_verified: 2026-09-25
tags:
  - architecture
  - documentation
---

# Architecture decision records

ADRは、将来の実装や運用に大きな影響を与える決定の理由を残す。規則は[ADR-0042](0042-adr-is-superseded-whole-and-deprecation-date-is-deprecated-on.md)に従う。

- `accepted`のADRだけが現在の決定で、本文の決定はすべて現在有効である。`superseded`のADRは`superseded_by`を辿り、`accepted`に着くまで読む。
- 決定を1つでも変えるときは、古いADRのまだ生きている決定も書き直して引き継ぐ統合ADRを書き、古いADRを丸ごと置き換える。「ADR-XXXXの決定Nを上書きする」だけの部分的なADRは書かない。
- 置き換えは後継を`accepted`にする変更と同じ変更で行う。`proposed`の後継は何も置き換えない。
- 本文はappend-onlyで、後から変えてよいのはstatus・`accepted_on`・`superseded_by`・`superseded_on`・`deprecated_on`とH1直後の注記1行だけ（`supersedes`は置き換える側のADRを書くときに本文と一緒に書く）。`superseded_on`は`superseded`にした日（後継の`accepted_on`と同じ）、`deprecated_on`は`deprecated`にした日。欄と注記の書式は[frontmatter仕様](../frontmatter.md)と[template](0000-template.md)にある。
- ADRのstatusを変える変更は、同じ変更でこの索引の2つの表も更新する。

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
| [ADR-0001](0001-rust-runtime.md) | Rustでruntimeを実装する | 2026-09-22 |
| [ADR-0002](0002-cmux-first.md) | cmuxを最初のworkspace backendにする | 2026-09-22 |
| [ADR-0003](0003-supervisor-owns-lifecycle.md) | supervisorがagentとworkspaceのライフサイクルを所有する | 2026-09-22 |
| [ADR-0004](0004-agent-provider-abstraction.md) | ClaudeとCodexをagent providerとして抽象化する | 2026-09-22 |
| [ADR-0005](0005-binary-and-plugin-distribution.md) | runtimeをバイナリ、agent integrationをpluginとして配布する | 2026-09-22 |
| [ADR-0006](0006-queue-per-repository.md) | repositoryごとに1つのqueueをユーザーのデータディレクトリに置き、cwdから解決する | 2026-09-22 |
| [ADR-0007](0007-run-level-leases-parallel-execution.md) | leaseをrun単位にし、依存が解けたtaskを上限付きで並列に実行する | 2026-09-22 |
| [ADR-0008](0008-merge-queue-squash-landing.md) | runtimeのmerge queueが最新mainへrebase・再検証し、1 task = 1 commitにsquashしてmainへ着地させる | 2026-09-22 |
| [ADR-0009](0009-goal-groups-tasks.md) | 複数のtaskが解く上位の課題をgoalとして表現し、workerのpromptに流す | 2026-09-25 |
| [ADR-0010](0010-maintainer-and-resident-supervisor.md) | 役割名をsupervisor / maintainer / workerに統一し、supervisorをlaunchdで常駐させてupとdownで起動・停止する | 2026-09-22 |
| [ADR-0011](0011-cmux-socket-password-and-in-cmux-fallback.md) | launchd常駐のsupervisorにはcmuxのsocket passwordを前提とし、up --in-cmuxをlaunchdなしのfallbackにする | 2026-09-22 |
| [ADR-0013](0013-layered-architecture-and-type-function-style.md) | domain / application / infrastructureのレイヤーと「型＋関数」でruntimeを構成する | 2026-09-22 |
| [ADR-0015](0015-rename-to-dagq.md) | cmux-taskqをdagqに改名する | 2026-09-22 |
| [ADR-0016](0016-maintainer-notification-and-compact-output.md) | maintainerを使い捨てのsessionにし、status / watch / doctorの通知経路と圧縮出力、pluginの起き直しhookを持たせる | 2026-09-23 |
| [ADR-0017](0017-resolve-run-paths-from-the-queue-directory.md) | runのqueue配下のpathは読むたびにqueueディレクトリとrun IDから解決する | 2026-09-23 |
| [ADR-0018](0018-run-workspace-named-after-the-task.md) | runのcmux workspace名はtaskのtitleにし、run IDはdescriptionに置く | 2026-09-23 |
| [ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md) | maintainerの定型作業をruntimeに移す（needs_sessionの自動resume、exit timeoutで放棄しない、push、follow_ups、evidence、prompt待ち） | 2026-09-23 |
| [ADR-0020](0020-rebind-queue-to-a-moved-repository.md) | repositoryの移動はrebindサブコマンドでqueueの束縛を付け替える | 2026-09-23 |
| [ADR-0021](0021-maintainer-and-supervisor-workspace-names-follow-the-run-style.md) | maintainer / supervisor / resumeのcmux workspace名もrunと同じ`[<repo>]dagq <role>`にそろえる | 2026-09-23 |
| [ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md) | 相談をqueueのask / answerにし、upがinboxとplannerを開き、着地は疑義のあるときだけ人に聞き、cmux notifyはinbox宛てにする | 2026-09-23 |
| [ADR-0025](0025-leaseless-unfinished-run-is-a-recover-run-attention.md) | supervisorが手放した未完了runをattention（recover run）にする | 2026-09-23 |
| [ADR-0026](0026-identify-workspaces-by-uuid-env-and-queue-group.md) | cmux workspaceをtitleではなくqueue DBのUUIDで識別し、roleとqueueを--envで持たせ、queueごとのworkspace groupにまとめる | 2026-09-23 |
| [ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md) | workerのsessionをreviewの後まで残し、機械的な指摘はrevise verdictで生きているworkerに返し、着地前にmerge-treeで衝突を事前判定する | 2026-09-23 |
| [ADR-0028](0028-workspace-titles-are-repo-and-role.md) | cmux workspaceのtitleを`[<repo>]<role>`にし、planner / inboxの名前とrole値を定義する | 2026-09-23 |
| [ADR-0029](0029-task-declares-paths-and-verification-follows-the-kind-of-change.md) | taskが変更してよいパス（add --paths）を宣言し、validatingとintegrateが宣言外の変更を拒否し、verification_commandsを変更の種類で軽くする | 2026-09-24 |
| [ADR-0030](0030-publish-to-crates-io-on-tag-push-with-trusted-publishing.md) | crates.ioを追加の配布経路にし、tag pushでTrusted Publishingによって自動でpublishする | 2026-09-24 |
| [ADR-0031](0031-color-pill-and-pin-for-inbox-and-planner-and-unpin-before-close.md) | upがinbox / plannerのworkspaceに役割の色・status pill・ピンを当て、dagqのworkspace closeはピンを外してから閉じる | 2026-09-24 |
| [ADR-0036](0036-delete-frozen-work-records.md) | 凍結済みのdocs/journal/を削除し、今も効く手順と観測事実だけをdesign文書へ移す | 2026-09-25 |
| [ADR-0038](0038-task-depends-on-a-goal-until-it-is-achieved.md) | taskがgoalに依存でき、依存先のgoalがachievedで閉じるまでclaimされない | 2026-09-25 |
| [ADR-0039](0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md) | supervisorが死んだrunは、wrapperが生きていれば次のsupervisorが引き継ぎ、自分のtokenのままstaleになったleaseは更新して続ける | 2026-09-25 |
| [ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md) | 検証をintegrateの1回にし、reviewをsupervisorの工程にし、dagq.tomlでrunのenvを渡し、taskの5段階の優先度と解放数でclaim順を決め、statsで詰まりを数える | 2026-09-25 |
| [ADR-0041](0041-on-demand-planners-proposals-submitted-and-plan-review-job.md) | 役割を5つにし、plannerをproposalごとのオンデマンドのworkspaceにし、taskにsubmitted状態を足し、supervisorが起動するplan review jobだけがreadyにし、follow_upのdraftもruntimeが立てるplannerのproposalとして同じgateを通す | 2026-09-25 |
| [ADR-0042](0042-adr-is-superseded-whole-and-deprecation-date-is-deprecated-on.md) | ADRは丸ごと置き換え、置き換えの日付はsuperseded_on、廃止の日付はdeprecated_onに分けてfrontmatterと本文冒頭の注記に残す | 2026-09-25 |
| [ADR-0043](0043-detect-stalled-worker-sessions-nudge-once-then-ask.md) | supervisorが止まったworkerのsessionを決まった規則で検知し、一度促すかEnterを一度送り直してからinboxのaskにし、statsが走っているrunのalertと閾値ごとの結果を返す | 2026-09-25 |
| [ADR-0045](0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md) | 固定バイナリをbuild識別子で見分け、queueを開いただけではmigrateせず、互換の範囲のschemaを受け入れ、supervisorを待たずに引き継ぎで入れ替え、up --auto-updateで着地のたびに自動で更新する | 2026-09-25 |
| [ADR-0046](0046-full-text-search-related-and-duplicate-of.md) | taskの全文検索（search）と決まった規則の関連（related）と重複の記録（cancel --duplicate-of）を持ち、plannerとplan reviewはその候補だけをLLMで判断する | 2026-09-25 |

## 置き換え・廃止されたADR

`status: superseded` / `deprecated`のADRと後継の対応。`deprecated`の行は`superseded_by`を空にし、日付の列に`deprecated_on`を書く。0001〜0034の棚卸し（後続のtask）で統合ADRが`accepted`になるときにも行が加わる。

| ADR | Status | superseded_by | superseded_on / deprecated_on |
| --- | --- | --- | --- |
| [ADR-0012](0012-adopt-stale-lease-of-live-wrapper.md) | superseded | [ADR-0039](0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md) | 2026-09-25 |
| [ADR-0014](0014-up-replaces-a-supervisor-of-another-binary-version.md) | superseded | [ADR-0045](0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md) | 2026-09-25 |
| [ADR-0023](0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md) | superseded | [ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md) | 2026-09-25 |
| [ADR-0024](0024-retire-maintainer-into-jobs-and-observer.md) | superseded | [ADR-0041](0041-on-demand-planners-proposals-submitted-and-plan-review-job.md) | 2026-09-25 |
| [ADR-0035](0035-adr-is-superseded-whole-with-dates-and-banner.md) | superseded | [ADR-0042](0042-adr-is-superseded-whole-and-deprecation-date-is-deprecated-on.md) | 2026-09-25 |
| [ADR-0037](0037-follow-up-triage-job-decides-follow-up-drafts.md) | superseded | [ADR-0041](0041-on-demand-planners-proposals-submitted-and-plan-review-job.md) | 2026-09-25 |
