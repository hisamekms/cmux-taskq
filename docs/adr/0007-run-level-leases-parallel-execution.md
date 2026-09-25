---
id: adr-0007
type: adr
title: leaseをrun単位にし、依存が解けたtaskを上限付きで並列に実行する
status: accepted
created: 2026-09-22
updated: 2026-09-22
accepted_on: 2026-09-22
owners:
  - hisamekms
tags:
  - runtime
  - persistence
  - supervisor
related:
  - design-supervisor-lifecycle
  - design-persistence
  - design-domain-model
  - adr-0003
  - adr-0006
---

# ADR-0007: leaseをrun単位にし、依存が解けたtaskを上限付きで並列に実行する

## Context

ステップ3〜5のsupervisorはqueue全体に1つの`supervisor_leases`行と部分UNIQUE index `one_executing_run_per_queue`を持ち、1件を処理して終了していた。ドッグフーディング（ステップ9）では依存のないtaskを同時に走らせ、`integrate`で依存が解けたtaskを人が再起動せずに拾う常駐のsupervisorが要る。[ADR-0003](0003-supervisor-owns-lifecycle.md)の「supervisorがlifecycleを所有し、孤児runは自動再実行しない」は保つ。

queue単位のleaseのままでは、(1) 常駐supervisorが生きている限りどのrunも`recover`できず、1 runの異常のために全runを止める必要がある、(2) 1つのsupervisorが止まると全runが孤児になる、(3) `doctor`/`recover`がrunごとに判定できない。

## Decision

- leaseはrun単位にする。新テーブル`run_leases(run_id PK → task_runs, token, pid, heartbeat_at)`が「supervisorがいまそのrunを見ている」ことを表し、`task_runs.supervisor_token`は実行者の記録として残る。tokenはsupervisorプロセスに1つで、そのsupervisorの全leaseが同じtokenを持ち、heartbeatは1文で更新する。`supervisor_leases`と`one_executing_run_per_queue`は廃止する（migration 0005、schema v5）。v4のleaseは実行中runの`run_leases`行へ移す。
- claimはlease作成と同じトランザクションで行う（`claim_for_supervisor`）。所有者のないclaimed runは作らない。queue全体の実行枠はなく、Taskごとの未完了run 1件（`one_unfinished_run_per_task`）だけが制約になる。同じqueueに複数のsupervisorがいてもclaimは直列化され、同じtaskに二重runはできない。
- `supervise --parallel N`（既定4）は常駐ループ。候補があれば`refs/heads/main`を読み直して上限までclaim・起動し、各runのwrapper heartbeat・receipt・idle marker・exitを1つのループで監視し、receipt検証はrunごとのthreadで行い、`awaiting_integration`または`failed`になったrunのleaseを解放する。候補がなければ2秒ごとに`candidates`を見る。`--once`は「active runがなく、claimできるtaskもなければ終了」。SIGINT/SIGTERMは1回目でclaimを止めてactive runの終了を待ち、2回目で即終了する。
- 1 runのruntime error（wrapper heartbeat切れ、exit要求のtimeout、検証処理やcloseのエラー）はそのrunだけをabandonする: `last_error`と`runtime_error`イベントを書き、lease行を削除し、status・process・workspace・worktreeは変えない。supervisorは他のrunを続ける。leaseを消すのは、常駐supervisorが生きている間も`recover`がrunのprocessだけで判定できるようにするため。未登録のwrapperはleaseがなければ登録できず、Claudeを起動しない。
- provisioning（plan、worktree、workspace作成）の失敗は環境要因とみなし、そのrunをabandonした上で以後のclaimを止め、active runをdrainしてから非0で終了する。全候補を順に潰さない。
- `doctor`/`recover`はrunごとに判定する。blockerはそのrunのprocessとleaseだけで、`recover`はそのrunのleaseだけを削除する。`status`はleaseのpidごとにsupervisorを並べ、未完了runとそのleaseを返す。

## Alternatives

- `task_runs`に`supervisor_pid`/`supervisor_heartbeat_at`を足す: 「解放済み」をnullで表すことになり、実行の記録と揮発する所有権が同じ行に混ざる。lease行の有無で所有を表す方が`recover`と`doctor`の判定が単純になる。
- runごとにthreadを立てて現行の直列処理をそのまま並べる: 実装は最も小さいが、workspace backendの共有と「1つのループで監視する」という設計方針に反する。検証コマンドだけをthreadに逃がして、監視は1ループに置いた。
- abandon時にleaseを残す: 今までの「sessionが生きているかもしれない間はleaseを解放しない」の延長。常駐supervisorではleaseが新鮮なまま残り、`recover`が「supervisor pidが生きている」で永久に拒否する。taskは未完了runで占有されたままなので、leaseを消しても二重実行にはならない。
- queue全体の排他（supervisor 1つ）を残す: run単位のleaseがあれば不要で、テスト用に`--db`を分けるのと同じ理由で複数supervisorを禁じる根拠がない。
- provisioning失敗後もclaimを続ける: cmuxやGitが落ちていると全候補が`starting`で止まり、それぞれ`recover`と`ready`が要る。

## Consequences

- 依存のないtaskは同時に走り、依存のあるtaskは先行taskの`completed`（`integrate`）まで待って、その後の`main`から始まる。
- `ClaimOutcome::Busy`はなくなり、`claim`は候補がなければ`NoReadyTask`を返す。
- `recover`後に同じsupervisorが生きていても新しいrunを作れる。SVは1 runの復旧のためにsupervisorを止めなくてよい。
- 常駐supervisorはClaudeのworkspaceを4つまで同時に開く。権限確認や質問はworkspaceごとに人が対応する。
- schema v5への移行は`init`または最初のopenで自動。v4のバイナリはv5のDBを拒否する。
- ステップ7（merge queue）は`integrate`だけを置き換え、このループとleaseはそのまま使う。
