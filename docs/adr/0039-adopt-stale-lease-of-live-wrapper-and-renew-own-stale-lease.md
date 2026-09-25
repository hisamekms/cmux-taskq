---
id: adr-0039
type: adr
title: supervisorが死んだrunは、wrapperが生きていれば次のsupervisorが引き継ぎ、自分のtokenのままstaleになったleaseは更新して続ける
status: accepted
created: 2026-09-25
updated: 2026-09-25
accepted_on: 2026-09-25
supersedes:
  - adr-0012
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - persistence
related:
  - adr-0003
  - adr-0007
  - adr-0012
  - adr-0025
  - adr-0035
  - design-supervisor-lifecycle
  - design-persistence
---

# ADR-0039: supervisorが死んだrunは、wrapperが生きていれば次のsupervisorが引き継ぎ、自分のtokenのままstaleになったleaseは更新して続ける

## Context

[ADR-0012](0012-adopt-stale-lease-of-live-wrapper.md)（2026-09-22）は、supervisorが死んで`run_leases`の行がstaleになったrunを、wrapperが生きていれば次のsupervisorが再実行せずに引き継ぐ（adopt）と決めた。きっかけは実運用のqueueのtask 15で、supervisorをkillした後もworkerのClaude sessionはwrapperの下で動き続けてreceiptを書いたが、見るsupervisorがおらず、人が`recover`と`ready`でrunをやり直し、完成していた成果とsessionの文脈を捨てた。[ADR-0003](0003-supervisor-owns-lifecycle.md)のとおりrunのlifecycleを所有するのはsupervisorで、wrapperはheartbeatと終了コードを記録するだけなので、supervisorがいない間にrunが`running`から先へ進む経路はない。supervisorは`up`で立ち直り（in-cmux modeでは人が`up`を打ち直す）、「staleなleaseの隣に、それを引き継げるsupervisorがいる」状況は通常のことになった。

task 229（2026-09-25）で、ADR-0012が扱わない逆の場合が見つかった。ホストのsleepでwall clockが飛ぶと、supervisorのプロセスは死なずに止まり、起きた時点で自分のleaseのheartbeatは`HEARTBEAT_TIMEOUT_SECS`（30秒）より古い。lease行のtokenはまだ自分のもので、引き継いだsupervisorもいない。ところがleaseを使う書き込み（`finish_supervision`、`finish_validation`など）は「自分のtokenで、heartbeatが30秒以内のlease行がある」ことを要求するので（`run lease is missing or stale`）、次のheartbeatより先に書き込みが来ると拒まれ、supervisorはleaseを失ったものとしてrunを手放す。誰も引き継いでいないのでrunは宙に浮き、[ADR-0025](0025-leaseless-unfinished-run-is-a-recover-run-attention.md)の`recover run`などを経て人が`recover`する。staleという観測は「このrunを見ているsupervisorがいないかもしれない」という他者向けの判定材料であって、所有そのものは他のsupervisorが引き継いでtokenを入れ替えるか、`recover`が行を消すまで変わっていない。

[ADR-0035](0035-adr-is-superseded-whole-with-dates-and-banner.md)の規則に従い、このADRはADR-0012を丸ごと置き換える。ADR-0012の決定は用語を今の役割（[overview](../design/overview.md)の用語集）に合わせて書き直して引き継ぎ（決定1〜6）、新しい決定（決定7）を足す。2026-09-25の`draft`棚卸しで、人がtask 229のfollow_upの方針を下の案A（lease行がまだ自分のtokenなら更新して続ける）に決めた。

## Decision

### 1. 引き継ぎの条件

`supervise`プロセスは、次の条件がすべて成り立つrunを再実行せずに**引き継ぐ（adopt）**。ADR-0012の後にtaskで広げた対象（`awaiting_integration`のrun、`resume_skipped`で進んだrun。[supervisor-lifecycle](../design/supervisor-lifecycle.md)の`supervise`の5）もここに含める。

- (a) runが`running`・`validating`・`awaiting_integration`のどれかである。
- (b) そのrunに`run_leases`の行があり、tokenが自分のものでなく、`recover`が使うのと同じ規則でstaleである: leaseのpidが死んでいる、またはheartbeatが`HEARTBEAT_TIMEOUT_SECS`（30秒）より古い（OR規則。`doctor` / `up`の`stale` / `lease_stale`と同じ）。
- (c) wrapperが`run_processes`に登録済みで、生きていてheartbeatが30秒以内か、`exited_at`が記録済み（`session_exited`と`supervision_finished`の間でsupervisorが死んだ）である。ただし次の2つはwrapperの状態を問わない。
  - `awaiting_integration`のrun（task 236）。reviewはheadlessでsessionを要らず、wrapperが終了を記録せずに死んだsessionは終わったsessionとして扱う。
  - 最後のresumeのイベントが`resume_skipped`のrun（task 148）。sessionを開かずに`validating` / `awaiting_integration`へ進めたrunで、wrapperが無い（`run_adopted`の`wrapper`はnull）か生きていなくても引き継ぎ、sessionは無いものとして扱う。

引き継がないもの: `claimed` / `starting`のrun（`register_wrapper`はclaimしたtokenを要求するので、引き継いでもwrapperを登録できない。`recover`に任せる）、lease行のないrun（runtime errorでabandonされたか、`recover`済み）、`integrating`のrun（着地は`recover`が`awaiting_integration`に戻す）、wrapperが死んでいるか30秒以上黙っている`running` / `validating`のrun（上の`resume_skipped`の例外を除く。`recover`の対象。`doctor`のblockersと`recoverable`は変わらない）。

### 2. 引き継ぎのトランザクション

引き継ぎは1つの`BEGIN IMMEDIATE`トランザクション（`adopt_run(run, previous_token, token, pid, wrapper)`）で行う。その中で(a)と(b)を再検査し、lease行の`token`・`pid`・`heartbeat_at`を引き継ぐsupervisorのものに更新し、`task_runs.supervisor_token`も引き継ぐsupervisorのtokenにし、`run_adopted`イベント`{previous_token, previous_pid, previous_heartbeat_age_secs, wrapper: {pid, alive, exited_at}, token, pid}`を書く。同じrunを2つのsupervisorが同時に引き継ごうとしても勝つのは1つで、負けた方は旧tokenのleaseが見つからず（0行更新）何もしない。

### 3. `task_runs.supervisor_token`の付け替え

`finish_supervision`、`finish_validation`、`workspace_closed`、`cleanup_failed`はleaseに加えてこの列が自分のtokenであることを要求する。claimしたsupervisorのtokenを残すと、これらの述語を緩めるか2つのtokenを持ち回ることになるので、引き継ぐsupervisorのtokenに更新する。この列は「いまこのrunを動かしているsupervisor」であって履歴ではなく、claimしたsupervisorのtokenは`lease_acquired`（pid）と`run_adopted`の`previous_token`が持つ。`status` / `doctor`はleaseをtokenでsupervisor登録に結び付けるので、lease行のtokenは引き継ぐsupervisorのものでなければならず、列もそれに揃える。

### 4. 引き継ぐ時機とslotの組み立て直し

各fill passの先頭（claimの前）と、idleの間、active runが`--parallel`未満のときに、他のtokenのleaseを持つ`running` / `validating`のrunを調べて引き継ぐ。引き継いだrunはclaimしたrunと同じくslotを占める。slotはDBから組み立て直す: workspace_id、run dir、receipt path、idle markerは`run_planned`のpathから、`receipt_seen`はreceiptファイルの存在と`receipt_observed`イベントの有無から組み立てる。`exit_requested`イベントがあれば`/exit`を再送せず終了待ちのtimeoutをいまから数え直す。wrapperは登録済みなので登録待ちのtimeoutは持たない。`validating`のrunは検証をはじめからやり直す（検証はreceiptとworktreeだけの関数で、再実行しても同じ結果になる）。`awaiting_integration`のrunはreviewに進める。

### 5. leaseを失った側は退く

引き継ぎの補集合として、leaseを失ったsupervisorはそのrunに触らない。各tickの先頭で自分のtokenのlease行があることを確認し、なければそのrunをslotから外し、DBには何も書かず（`abandon`もしない）、結果の`errors`に理由を載せる。tickの途中で失った（lease付きの書き込みが拒まれた）場合も同じ扱いで、`last_error`は書かない。監視は、wrapperが`exited_at`を記録済みのsessionに`/exit`を送らない（引き継いだ時点で終わっていたsessionのworkspaceにcmuxが`send`を拒んでも、runを手放さないため）。ここでいう「leaseを失った」は、lease行が無いか、lease行のtokenが自分のものでないことを指す（決定7）。

### 6. 記録と変えないもの

引き継ぎはsupervisorのlog（stderrと`--log-dir`）に1行で記録する（旧token、旧pid、heartbeatの経過秒数、wrapperの状態、task、workspace）。

変えないもの: `recover`、`doctor`、`integrate`、`supervisors`表、`up`のprune。`doctor` / `status`は引き継いだrunを他のrunと同じく引き継いだsupervisorのtokenの下に出す。schema migrationは要らない（既存の列とイベント表だけで表せる）。

### 7. 自分のtokenのままstaleになったleaseは更新して続ける（新しい決定）

leaseを使って書き込むとき、そのrunのlease行がまだ自分のtokenなら、heartbeatが`HEARTBEAT_TIMEOUT_SECS`より古くても（ホストのsleepでwall clockが飛んだとき、supervisorが一時的に止められていたときなど）、lease行の`heartbeat_at`を条件付きのUPDATE（`WHERE run_id = そのrun AND token = 自分`）で更新してから書き込みを続ける。この更新は、その書き込みと同じ`BEGIN IMMEDIATE`トランザクションの中で、今の「heartbeatが30秒以内のlease行があるか」の検査（`assert_lease`）に代えて行い、更新と書き込みの間に他のsupervisorが割り込む隙を作らない。更新が1行に当たれば自分のleaseは新鮮になり、拒まずに続ける。0行なら（他のsupervisorが引き継いでtokenが変わった、または`recover`が行を消した）、今までどおり決定5に従って退く。heartbeatの古さは、leaseを使う書き込みを拒む理由にしない。

「1つのrunを動かすsupervisorは高々1つ」は、引き継ぎのトランザクションでlease行のtokenが入れ替わることで保たれる。自分のstaleなleaseの更新と他のsupervisorの引き継ぎはどちらもlease行への書き込みで、SQLiteの書き込みは直列化される。更新が先に確定すれば、引き継ぐ側のトランザクションは(b)の再検査でleaseが新鮮（pidも生きている）と見て引き継がない。引き継ぎが先に確定すれば、tokenがもう自分のものでないので更新は0行になり、退く。どちらの順でも、runを動かし続けるsupervisorは1つになる。

[ADR-0007](0007-run-level-leases-parallel-execution.md)の「leaseは自動で奪わない」との関係: ADR-0012（決定1〜6として引き継ぐ）がこれを「生きているwrapperのstaleなleaseは引き継ぐ。それ以外は手動のまま」に変えた。決定7は他のsupervisorのleaseに触らず、自分のtokenの行を延命するだけなので、奪うことには当たらず、この規則を広げも狭めもしない。staleという判定は、引き継ぎ（決定1）と`recover` / `doctor`が「見ているsupervisorがいない」と推定するための材料で、所有はtokenの入れ替え（引き継ぎ）か行の削除（`recover`、abandon）でだけ移る。

## Alternatives

- **手動の`recover` + `ready`のまま**（ADR-0012が退けた案）: 完成した成果とworker sessionが失われ、同じ作業をもう1回払う。task 15で実際に起きた。
- **wrapperがrunの状態機械を持つ**（ADR-0012が退けた案）: wrapperがreceiptの検証と`awaiting_integration`への遷移まで行えばsupervisorの死に影響されない。しかしsupervisorがlifecycleを所有する[ADR-0003](0003-supervisor-owns-lifecycle.md)と矛盾し、検証（Git、検証コマンド、workspaceのclose）をrunごとのプロセスに複製する大きな変更になる。
- **`supervise`の起動時だけ引き継ぐ**（ADR-0012が退けた案）: 別のsupervisorが常駐している間に1つが死んだ場合（`--parallel`を分けて2つ動かしている、`integrate`中に片方が落ちた、など）を取りこぼす。fill passごとに見れば、生きているsupervisorが空きslotの範囲で拾える。
- **heartbeatが古いだけ（pidは生きている）のleaseは引き継がない（AND規則）**（ADR-0012が退けた案）: killされたsupervisorはpidが死んだ時点で引き継げるべきで、heartbeatの30秒を待つ理由はない。pidが生きたままheartbeatが止まったsupervisor（SIGSTOP、sleep、DBに書けずに抜けられない状態）は`status`でも`stale`で、そのrunを見ていない。どちらも「見ているsupervisorがいない」の観測で、`doctor` / `up`と同じOR規則に揃えた。止まっていたsupervisorが動き出したときは、引き継がれていれば決定5で退き、引き継がれていなければ決定7で続ける。
- **B: 自分のstaleなrunも引き継ぎの対象にする**: 決定1の(b)から「tokenが自分のものでない」を外し、起きたsupervisorが自分のrunを引き継ぎ直す案。所有を1つの経路（`adopt_run`）で扱えるが、引き継ぎはslotをDBから組み立て直すので、`validating`のrunは検証がはじめからやり直しになり、`exit_requested`済みのrunは`/exit`の終了待ちのtimeoutを数え直す。自分がまだ持っているslotの状態を捨てる理由がなく、sleepのたびに作業が戻る。
- **C: 変えない（sleepの後は人が`recover`する）**: runtimeの変更は要らないが、誰も引き継いでいないrunを自分から手放し、task 229と同じく人の`recover`とrunのやり直しが要る。ADR-0012が避けたかった「完成した成果とsessionの喪失」を、supervisorが死んでいないのに起こす。

## Consequences

- 「leaseは自動で奪わない」は「生きているwrapperのstaleなleaseは引き継ぐ（`awaiting_integration`と`resume_skipped`のrunはwrapperを問わない）。それ以外は手動のまま」のままで、決定7はこれを変えない。`recover`は引き続き、wrapperが死んだ・黙った`running` / `validating`のrun、`claimed` / `starting`のrun、`integrating`のrun、leaseのないrunの経路で、その判定と`doctor`の出力は変わらない。
- supervisorの入れ替え（バイナリ更新、kill、再起動）でrunが失われない。`up`の後、新しいsupervisorが最初のfill passで引き継ぐ。人がworkspaceで`/exit`を打つ必要も、`recover` + `ready`でやり直す必要もない。
- ホストのsleepや一時停止から戻ったsupervisorは、誰にも引き継がれていなければ自分のrunをそのまま続ける。人の`recover`は要らない。
- `run_adopted`がrunの履歴に増え、`show`のイベントでどのsupervisorがいつ引き継いだかを追える。`task_runs.supervisor_token`は「最後にそのrunを動かしたsupervisor」を指す。
- 引き継ぎのたびに`validating`の検証はやり直しになる。`exit_requested`のtimeoutは引き継ぎ時点から再び数える。止まっていたsupervisorが検証の途中で引き継がれると、その検証threadは記録されずに走り切り、同じworktreeで引き継いだ側の検証と並ぶことがある（稀で、`ready`で再試行できる）。
- 2つのsupervisorが同じrunを触る窓は、staleと判定した後にトランザクションで再検査すること（決定2）と、自分のleaseの更新をtokenの条件付きUPDATEにすること（決定7）で閉じている。引き継がれた側は次のtick（1秒）かlease付きの書き込みで退く。その間に旧supervisorがreceiptの観測や`/exit`の送信を行う可能性は残る（`/exit`の再送は同じ経路で人が打つのと同じで、runの状態は変えない）。
- lease付きの書き込みの検査（staleなら拒む）を、tokenの一致の検査と条件付きのheartbeat更新に変える実装が要る。実装と[supervisor-lifecycle](../design/supervisor-lifecycle.md) / [persistence](../design/persistence.md)の更新は後続のtaskで行う。schema migrationは要らない。
- 引き継ぎの候補はfill passごとに`run_leases`と`task_runs`のjoinを1回読む。runの数は小さいので無視できる。
