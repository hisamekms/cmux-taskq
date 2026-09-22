---
id: adr-0012
type: adr
title: supervisorが死んだrunは、wrapperが生きていれば次のsupervisorが引き継ぐ
status: accepted
created: 2026-09-22
updated: 2026-09-22
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - persistence
related:
  - adr-0003
  - adr-0007
  - adr-0010
  - design-supervisor-lifecycle
  - design-persistence
---

# ADR-0012: supervisorが死んだrunは、wrapperが生きていれば次のsupervisorが引き継ぐ

## Context

実運用のqueueのtask 15（2026-09-22）で、常駐supervisorをバイナリとschemaの更新のために止めた（kill）。そのsupervisorが持っていた`run_leases`の行はstaleになったが、runは止まらなかった: workerのClaude sessionはcmux workspaceの中でwrapperの下で動き続け、作業を終えてreceiptを書いた。ところがそのreceiptを見るsupervisorがいない。maintainerはworkspaceで`/exit`を打ち、`recover`でrunを`interrupted`にし、`ready`で新しいrunを作り、新しいworkerが同じ作業をやり直した。完成していた成果とworker sessionの文脈が捨てられた。

このときのruntimeの契約は次のとおりだった。[supervisor-lifecycle](../design/supervisor-lifecycle.md)は「supervisorの再起動ではrunごとのleaseとheartbeatを確認し、孤児プロセスを勝手に再実行しない」、[ADR-0007](0007-run-level-leases-parallel-execution.md)と[persistence](../design/persistence.md)は「leaseは自動で奪わない」で、staleなleaseの扱いはすべて`recover`（人の判断）だった。`recover`はrunを`interrupted`にするだけで、再実行は新しいTaskRunになる。[ADR-0003](0003-supervisor-owns-lifecycle.md)のとおりrunのlifecycleを所有するのはsupervisorで、wrapperはheartbeatと終了コードを記録するだけなので、supervisorがいない間にrunが`running`から先へ進む経路はない。

[ADR-0010](0010-maintainer-and-resident-supervisor.md)でsupervisorはlaunchdの`KeepAlive`で常駐するようになり、killや更新のたびに新しいsupervisorプロセスがすぐ立つ。つまり「staleなleaseの隣に、それを引き継げるsupervisorがいる」状況が通常になった。

## Decision

`supervise`プロセスは、次の条件がすべて成り立つrunを再実行せずに**引き継ぐ（adopt）**。

- (a) runが`running`または`validating`である。
- (b) そのrunに`run_leases`の行があり、tokenが自分のものでなく、`recover`が使うのと同じ規則でstaleである: leaseのpidが死んでいる、またはheartbeatが30秒（lease TTL）より古い。
- (c) wrapperが`run_processes`に登録済みで、生きていてheartbeatが30秒以内か、`exited_at`が記録済み（`session_exited`と`supervision_finished`の間でsupervisorが死んだ）である。

引き継がないもの: `claimed` / `starting`のrun（`register_wrapper`はclaimしたtokenを要求するので、引き継いでもwrapperを登録できない。`recover`に任せる）、lease行のないrun（runtime errorでabandonされたか、`recover`済み）、`integrating`のrun（着地は`recover`が`awaiting_integration`に戻す）、wrapperが死んでいるか30秒以上黙っているrun（`recover`の対象。`doctor`のblockersと`recoverable`は変わらない）。

引き継ぎは1つの`BEGIN IMMEDIATE`トランザクション（`adopt_run(run, previous_token, token, pid, wrapper)`）で行い、その中で(a)と(b)を再検査し、lease行の`token`・`pid`・`heartbeat_at`を引き継ぐsupervisorのものに更新し、`task_runs.supervisor_token`も引き継ぐsupervisorのtokenにし、`run_adopted`イベント`{previous_token, previous_pid, previous_heartbeat_age_secs, wrapper: {pid, alive, exited_at}, token, pid}`を書く。同じrunを2つのsupervisorが同時に引き継ごうとしても、勝つのは1つで、負けた方は旧tokenのleaseが見つからず（0行更新）何もしない。

`task_runs.supervisor_token`を引き継ぐsupervisorに更新する理由: `finish_supervision`、`finish_validation`、`workspace_closed`、`cleanup_failed`はleaseに加えてこの列が自分のtokenであることを要求する。claimしたsupervisorのtokenを残すと、これらの述語を緩めるか2つのtokenを持ち回ることになる。この列は「いまこのrunを動かしているsupervisor」であって履歴ではなく、claimしたsupervisorのtokenは`lease_acquired`（pid）と`run_adopted`の`previous_token`が持つ。`status` / `doctor`はleaseをtokenでsupervisor登録に結び付けるので、lease行のtokenは引き継ぐsupervisorのものでなければならず、列もそれに揃える。

ループの位置: 各fill passの先頭（claimの前）と、idleの間、active runが`--parallel`未満のときに、他のtokenのleaseを持つ`running` / `validating`のrunを調べて引き継ぐ。引き継いだrunはclaimしたrunと同じくslotを占める。slotはDBから組み立て直す: workspace_id、run dir、receipt path、idle markerは`run_planned`のpathから、`receipt_seen`はreceiptファイルの存在と`receipt_observed`イベントの有無から、`exit_requested`イベントがあれば`/exit`を再送せず終了待ちのtimeoutをいまから数え直し、wrapperは登録済みなので登録待ちの45秒timeoutは持たない。`validating`のrunは検証をはじめからやり直す（検証はreceiptとworktreeだけの関数で、再実行しても同じ結果になる）。

引き継ぎの補集合として、leaseを失ったsupervisorはそのrunに触らない: 各tickの先頭で自分のtokenのlease行があることを確認し、なければそのrunをslotから外し、DBには何も書かず（`abandon`もしない）、結果の`errors`に理由を載せる。tickの途中で失った（lease付きの書き込みが拒まれた）場合も同じ扱いで、`last_error`は書かない。監視は、wrapperが`exited_at`を記録済みのsessionに`/exit`を送らない（引き継いだ時点で終わっていたsessionのworkspaceにcmuxが`send`を拒んでも、runを手放さないため）。leaseの再検査は引き継ぐ側の保護、この確認は失う側の保護で、両方で「1つのrunを動かすsupervisorは高々1つ」を保つ。

引き継ぎはstderrと`--log-dir`のlogに1行で記録する（旧token、旧pid、heartbeatの経過秒数、wrapperの状態、task、workspace）。

変えないもの: `recover`、`doctor`、`integrate`、`supervisors`表、`up`のprune。`doctor` / `status`は引き継いだrunを他のrunと同じく引き継いだsupervisorのtokenの下に出す。schema migrationは不要（既存の列とイベント表だけで表せる）。

## Alternatives

- **手動の`recover` + `ready`のまま**: 決定前の状態。完成した成果とworker sessionが失われ、同じ作業をもう1回払う。task 15で実際に起きた。
- **wrapperがrunの状態機械を持つ**: wrapperがreceiptの検証と`awaiting_integration`への遷移まで行えばsupervisorの死に影響されない。しかしsupervisorがlifecycleを所有する[ADR-0003](0003-supervisor-owns-lifecycle.md)と矛盾し、検証（Git、検証コマンド、workspaceのclose）をrunごとのプロセスに複製する大きな変更になる。
- **`supervise`の起動時だけ引き継ぐ**: 起動時に一度staleなleaseを拾う案。別のsupervisorが常駐している間に1つが死んだ場合（`--parallel`を分けて2つ動かしている、`integrate`中に片方が落ちた、など）を取りこぼす。fill passごとに見れば、生きているsupervisorが空きslotの範囲で拾える。
- **heartbeatが古いだけ（pidは生きている）のleaseは引き継がない（AND規則）**: `recover`のblockersはpid生存とheartbeatを別々に見る。killされたsupervisorはpidが死んだ時点で引き継げるべきで、heartbeatの30秒を待つ理由はない。逆にpidが生きたままheartbeatが止まったsupervisor（SIGSTOP、DBに書けずに抜けられない状態）は、`status`でも`stale`で、そのrunを見ていない。どちらも「見ているsupervisorがいない」の観測で、`doctor` / `up`の`stale` / `lease_stale`と同じOR規則に揃えた。止まっていたsupervisorが動き出しても、leaseを失ったsupervisorの確認（上記）で退く。

## Consequences

- 「leaseは自動で奪わない」は「生きているwrapperのstaleなleaseは引き継ぐ。それ以外は手動のまま」になる。`recover`は引き続き、wrapperが死んだ・黙ったrun、`claimed` / `starting`のrun、`integrating`のrun、leaseのないrunの経路で、その判定と`doctor`の出力は変わらない。
- supervisorの入れ替え（バイナリ更新、kill、launchdの再起動）でrunが失われない。`up`の後、新しいsupervisorが最初のfill passで引き継ぐ。maintainerがworkspaceで`/exit`を打つ必要も、`recover` + `ready`でやり直す必要もない。
- `run_adopted`がrunの履歴に増える。`show`のイベントでどのsupervisorがいつ引き継いだかを追える。`task_runs.supervisor_token`は「最後にそのrunを動かしたsupervisor」を指す。
- 引き継ぎのたびに`validating`の検証はやり直しになる（検証コマンドの再実行）。`exit_requested`のtimeoutは引き継ぎ時点から再び120秒数える。止まっていたsupervisorが検証の途中で引き継がれると、その検証threadは記録されずに走り切り、同じworktreeで引き継いだ側の検証と並ぶことがある（検証コマンドが同時実行に弱ければ、引き継いだ側が拒否して`failed`になりうる。稀で、`ready`で再試行できる）。
- 2つのsupervisorが同じrunを触る窓は、staleと判定した後にトランザクションで再検査するので閉じている。leaseを失った側は次のtick（1秒）で退く。その1秒の間に旧supervisorがreceiptの観測や`/exit`の送信を行う可能性は残る（`/exit`の再送は同じ経路で人が打つのと同じで、runの状態は変えない）。
- 引き継ぎの候補はfill passごとに`run_leases`と`task_runs`のjoinを1回読む。runの数は小さいので無視できる。
