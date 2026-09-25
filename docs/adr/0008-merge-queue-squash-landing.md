---
id: adr-0008
type: adr
title: runtimeのmerge queueが最新mainへrebase・再検証し、1 task = 1 commitにsquashしてmainへ着地させる
status: accepted
created: 2026-09-22
updated: 2026-09-22
accepted_on: 2026-09-22
owners:
  - hisamekms
tags:
  - runtime
  - git
  - persistence
  - supervisor
related:
  - design-supervisor-lifecycle
  - design-persistence
  - design-domain-model
  - adr-0003
  - adr-0007
---

# ADR-0008: runtimeのmerge queueが最新mainへrebase・再検証し、1 task = 1 commitにsquashしてmainへ着地させる

## Context

ステップ4（[008](../journal/008-integration-confirm.md)）の`integrate`は、人がrun branchをmainへmerge（merge commitまたはfast-forward）した後に`result_commit`がmainの祖先であることを確認するだけだった。並列実行（[ADR-0007](0007-run-level-leases-parallel-execution.md)）では複数のrunが同じbase commitから同時に`awaiting_integration`になり、2件目以降は最新mainへのrebaseと再検証が要る。人がworktreeでこれを繰り返すのはドッグフーディング（ステップ9）の趣旨に反し、merge commitやfast-forwardが混ざるとmainの履歴がtask単位に読めなくなる。

## Decision

- **着地はruntimeの`integrate`が行う。** `integrate ID`（taskの`awaiting_integration`または`needs_session`のrun）と`integrate --next`（`awaiting_integration`のrunを`validation_finished`イベントの順、すなわち検証完了の古い順に1件）。承認制で、SVがレビュー後に呼ぶ。承認なしの自動着地とpushはruntimeが行わない。
- **統合スロットは1つ。** runは`integrating`になり、`integrate`プロセスがその間だけ`run_leases`の行を持つ（tokenは`integrate`プロセスごと、heartbeatは`supervise`と同じthread）。部分UNIQUE index `one_integrating_run_per_queue`が同時着地を拒む。`integrating`のrunは`status`/`doctor`に並び、プロセスが死んでleaseがstaleになったら`recover`が`awaiting_integration`に戻す（検証済みの成果は失われていないので`interrupted`にしない）。
- **手順**は、途中のrebaseの`--abort` → receiptの検査（parseできる、`result`が`succeeded`、`commit`がworktreeの現在のHEADに一致） → clean → `git rebase <main head>` → HEADがmain headの子孫でmain headと異なる → clean → 検証コマンドの再実行 → `git commit-tree <HEAD>^{tree} -p <main head>`で1 commit → `refs/taskq/runs/<run-id>`をrebase後のHEADに向ける → mainを進める → runを`integrated`、`result_commit`を着地commit、Taskを`completed` → worktreeとbranch `taskq/<run-id>`を削除。
- **mainの進め方。** mainをcheckoutしているworktreeがあればそこで`git merge --ff-only <commit>`（indexとworking treeも一緒に進む。ローカル変更と衝突すれば失敗し、runは`awaiting_integration`に戻る）。なければ`git update-ref refs/heads/main <commit> <main head>`。fast-forwardの対象はruntimeが作ったsquash commitだけで、run branchのfast-forwardやmerge commitは作らない。
- **commit messageの契約。** 1段落目はtaskのtitle、2段落目はreceiptの`summary`（空なら省略）、末尾にtrailer `Taskq-Task: <task id>`と`Taskq-Run: <run id>`。着地commitからqueueのtaskとrun（`refs/taskq/runs/<run-id>`の履歴）へ辿れる。
- **衝突と再検証の失敗は`needs_session`。** rebaseが衝突したら衝突ファイルとGitの出力を`integration_deferred`に記録して`rebase --abort`し、worktreeを検証済みheadに戻す。rebase後の検証コマンド失敗などは、rebase済みのworktreeをそのまま残す。いずれも理由を`last_error`に書き、lease行を消してスロットを空ける。SVは`claude --resume <run-id>`でworktreeにセッションを開き直し、セッションが解消・検証コマンド再実行・新しいheadでのreceiptの書き直しを行う。`integrate ID`で再開し、同じ手順を最初から通す（rebaseはmainが動いていなければno-op）。receiptの`commit`が現在のHEADと一致することが「セッションが終わった」ことの検出で、mtime/hashや`resume`サブコマンドは持たない。
- **`failed` receiptはrunの終了。** セッションが変更不要と判断したら`result: failed`と理由をreceiptに書く。`integrate ID`はrunを`failed`にし、mainには触れない。再試行や取り消しは`failed` runと同じ手動操作。
- **`main`を進める前のエラー**（Git、ファイル、DB）は`integration_error`と`last_error`を書いて元のstatusに戻し、leaseを解放する。

## Alternatives

- 手動mergeの確認を続ける（008）: 並列で溜まったrunのrebaseと再検証を人がworktreeごとに繰り返すことになり、merge commit / fast-forward / squashが混ざる。
- run branchをfast-forwardまたはmerge commitでmainへ入れる: runの中間commitがmainに残り、1 task = 1 commitの直線にならない。revertや履歴の読み取りがtask単位でできない。
- 衝突をruntimeが解消する、または`needs_session`のrunを自動でresumeする: 解消の判断はセッションの仕事。自動resumeは「After first dogfooding」の項目。
- `needs_session`からの復帰を`resume`サブコマンドやreceiptのmtime/hashで検出する: 状態と経路が増える。receiptの`commit`がHEADに一致することは検証に必要な条件でもあり、それだけで足りる。
- `update-ref`だけでmainを進める: mainをcheckoutしているworktreeのindexとworking treeが古いままになり、着地した変更が逆向きの差分に見える。
- 着地後もworktreeとbranchを残す: 着地したtreeはmainに、履歴は`refs/taskq/runs/<run-id>`にあるので、worktreeの数だけディスクとGitのworktree一覧が増えるのを避ける。
- スロットをlease表なしの`integrating` statusだけで表す: `integrate`プロセスが死んだときに`doctor`が生存を判定できず、`recover`の条件が書けない。

## Consequences

- mainはtaskごとに1 commitの直線になり、着地commitのtreeは検証したworktreeのtreeに等しい。run branchの詳細履歴は`refs/taskq/runs/<run-id>`に残る。
- `awaiting_integration`のrunのworktreeは着地まで残り、着地後に消える。`needs_session`のrunのworktreeはセッションが使う。
- 後続taskは着地後のmainから始まる（`supervise`はclaimごとに`refs/heads/main`を読み直す）。
- 着地はGitのidentityと、mainをcheckoutしているworktreeのclean（少なくとも着地ファイルと重ならない）を前提にする。SVはmain checkoutに未commitの変更を溜めない。
- schema v6（migration 0006）。v5のバイナリはv6のDBを拒否する。
- mainを進めた後にDBの更新が失敗した場合、runは`integrating`のまま残る。`recover` → `integrate`ではrebaseでcommitが空になり`needs_session`になる（既知の限界。実機で起きたら[010](../journal/010-failure-path-smoke.md)で扱う）。
- pushはSVが行う。承認なしの自動着地と`needs_session`の自動resumeは後回し。
