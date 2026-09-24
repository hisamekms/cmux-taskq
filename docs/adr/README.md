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

ADRは、将来の実装や運用に大きな影響を与える決定の理由を残す。規則は[ADR-0035](0035-adr-is-superseded-whole-with-dates-and-banner.md)に従う。

- `accepted`のADRだけが現在の決定で、本文の決定はすべて現在有効である。`superseded`のADRは`superseded_by`を辿り、`accepted`に着くまで読む。
- 決定を1つでも変えるときは、古いADRのまだ生きている決定も書き直して引き継ぐ統合ADRを書き、古いADRを丸ごと置き換える。「ADR-XXXXの決定Nを上書きする」だけの部分的なADRは書かない。
- 置き換えは後継を`accepted`にする変更と同じ変更で行う。`proposed`の後継は何も置き換えない。
- 本文はappend-onlyで、後から変えてよいのはstatus・`accepted_on`・`superseded_by`・`superseded_on`とH1直後の注記1行だけ。欄と注記の書式は[frontmatter仕様](../frontmatter.md)と[template](0000-template.md)にある。
- ADRのstatusを変える変更は、同じ変更でこの索引の2つの表も更新する。

## Status

- `proposed`: 検討中。決定はまだ有効ではない
- `accepted`: 採用済み。本文の決定がすべて現在有効
- `rejected`: 不採用
- `superseded`: 後継のADRに丸ごと置き換え済み（`superseded_by`が後継を指す）
- `deprecated`: 後継なしで廃止済み（H1直後の注記が理由を示す）

新しいADRは [template](0000-template.md) をコピーして作る。

## 有効なADR

`status: accepted`のADR。`accepted_on`が空欄の行は、ADRの棚卸し（後続のtask）でgit logから確かめて埋める。棚卸しで置き換えられるADRはこの表から下の対応表に移る。

| ADR | Title | accepted_on |
| --- | --- | --- |
| [ADR-0001](0001-rust-runtime.md) | Rustでruntimeを実装する |  |
| [ADR-0002](0002-cmux-first.md) | cmuxを最初のworkspace backendにする |  |
| [ADR-0003](0003-supervisor-owns-lifecycle.md) | supervisorがagentとworkspaceのライフサイクルを所有する |  |
| [ADR-0004](0004-agent-provider-abstraction.md) | ClaudeとCodexをagent providerとして抽象化する |  |
| [ADR-0005](0005-binary-and-plugin-distribution.md) | runtimeをバイナリ、agent integrationをpluginとして配布する |  |
| [ADR-0006](0006-queue-per-repository.md) | repositoryごとに1つのqueueをユーザーのデータディレクトリに置き、cwdから解決する |  |
| [ADR-0007](0007-run-level-leases-parallel-execution.md) | leaseをrun単位にし、依存が解けたtaskを上限付きで並列に実行する |  |
| [ADR-0008](0008-merge-queue-squash-landing.md) | runtimeのmerge queueが最新mainへrebase・再検証し、1 task = 1 commitにsquashしてmainへ着地させる |  |
| [ADR-0010](0010-maintainer-and-resident-supervisor.md) | 役割名をsupervisor / maintainer / workerに統一し、supervisorをlaunchdで常駐させてupとdownで起動・停止する |  |
| [ADR-0011](0011-cmux-socket-password-and-in-cmux-fallback.md) | launchd常駐のsupervisorにはcmuxのsocket passwordを前提とし、up --in-cmuxをlaunchdなしのfallbackにする |  |
| [ADR-0012](0012-adopt-stale-lease-of-live-wrapper.md) | supervisorが死んだrunは、wrapperが生きていれば次のsupervisorが引き継ぐ |  |
| [ADR-0013](0013-layered-architecture-and-type-function-style.md) | domain / application / infrastructureのレイヤーと「型＋関数」でruntimeを構成する |  |
| [ADR-0014](0014-up-replaces-a-supervisor-of-another-binary-version.md) | upはbinary versionの違うsupervisorをdrainして入れ替える |  |
| [ADR-0015](0015-rename-to-dagq.md) | cmux-taskqをdagqに改名する |  |
| [ADR-0016](0016-maintainer-notification-and-compact-output.md) | maintainerを使い捨てのsessionにし、status / watch / doctorの通知経路と圧縮出力、pluginの起き直しhookを持たせる |  |
| [ADR-0017](0017-resolve-run-paths-from-the-queue-directory.md) | runのqueue配下のpathは読むたびにqueueディレクトリとrun IDから解決する |  |
| [ADR-0018](0018-run-workspace-named-after-the-task.md) | runのcmux workspace名はtaskのtitleにし、run IDはdescriptionに置く |  |
| [ADR-0019](0019-move-routine-maintainer-work-into-the-runtime.md) | maintainerの定型作業をruntimeに移す（needs_sessionの自動resume、exit timeoutで放棄しない、push、follow_ups、evidence、prompt待ち） |  |
| [ADR-0020](0020-rebind-queue-to-a-moved-repository.md) | repositoryの移動はrebindサブコマンドでqueueの束縛を付け替える |  |
| [ADR-0021](0021-maintainer-and-supervisor-workspace-names-follow-the-run-style.md) | maintainer / supervisor / resumeのcmux workspace名もrunと同じ`[<repo>]dagq <role>`にそろえる |  |
| [ADR-0022](0022-ask-answer-inbox-planner-and-landing-on-doubt.md) | 相談をqueueのask / answerにし、upがinboxとplannerを開き、着地は疑義のあるときだけ人に聞き、cmux notifyはinbox宛てにする |  |
| [ADR-0023](0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md) | 検証をintegrateの1回にし、reviewをsupervisorの工程にしてpassなら着地し、dagq.tomlでrunのenvを渡し、graphと解放数でclaim順を決め、statsで詰まりを数える |  |
| [ADR-0024](0024-retire-maintainer-into-jobs-and-observer.md) | maintainerを退役させ、review・triage・observerをheadlessのjobにし、observerの権限をnoteとdraft goalとaskに限り、goalにdraft状態を足す |  |
| [ADR-0025](0025-leaseless-unfinished-run-is-a-recover-run-attention.md) | supervisorが手放した未完了runをattention（recover run）にする |  |
| [ADR-0026](0026-identify-workspaces-by-uuid-env-and-queue-group.md) | cmux workspaceをtitleではなくqueue DBのUUIDで識別し、roleとqueueを--envで持たせ、queueごとのworkspace groupにまとめる |  |
| [ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md) | workerのsessionをreviewの後まで残し、機械的な指摘はrevise verdictで生きているworkerに返し、着地前にmerge-treeで衝突を事前判定する |  |
| [ADR-0028](0028-workspace-titles-are-repo-and-role.md) | cmux workspaceのtitleを`[<repo>]<role>`にし、planner / inboxの名前とrole値を定義する |  |
| [ADR-0029](0029-task-declares-paths-and-verification-follows-the-kind-of-change.md) | taskが変更してよいパス（add --paths）を宣言し、validatingとintegrateが宣言外の変更を拒否し、verification_commandsを変更の種類で軽くする |  |
| [ADR-0030](0030-publish-to-crates-io-on-tag-push-with-trusted-publishing.md) | crates.ioを追加の配布経路にし、tag pushでTrusted Publishingによって自動でpublishする |  |
| [ADR-0031](0031-color-pill-and-pin-for-inbox-and-planner-and-unpin-before-close.md) | upがinbox / plannerのworkspaceに役割の色・status pill・ピンを当て、dagqのworkspace closeはピンを外してから閉じる |  |
| [ADR-0035](0035-adr-is-superseded-whole-with-dates-and-banner.md) | ADRは丸ごと置き換え、置き換え・廃止の日付とstatusをfrontmatterと本文冒頭の注記に残す | 2026-09-24 |
| [ADR-0036](0036-delete-frozen-work-records.md) | 凍結済みのdocs/journal/を削除し、今も効く手順と観測事実だけをdesign文書へ移す | 2026-09-25 |
| [ADR-0037](0037-follow-up-triage-job-decides-follow-up-drafts.md) | follow_upのdraft taskの採否をsupervisorが起動するheadlessのfollow-up triage jobが決め、判断がつかないものだけinboxで人に聞く |  |

## 置き換え・廃止されたADR

`status: superseded` / `deprecated`のADRと後継の対応。`deprecated`の行は`superseded_by`を空にする。2026-09-24時点で置き換え・廃止されたADRは無い。0001〜0034の棚卸し（後続のtask）で統合ADRが`accepted`になるときに埋まる。

| ADR | Status | superseded_by | superseded_on |
| --- | --- | --- | --- |
