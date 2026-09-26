---
id: design-supervisor-lifecycle-disk-space
type: design
title: "空き容量を確かめる（claimと着地の検証の前）"
status: current
created: 2026-09-27
updated: 2026-09-27
last_verified: 2026-09-27
scope: runtime
related:
  - adr-0047
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-claim-hold
  - design-supervisor-lifecycle-run-worktrees
  - design-supervisor-lifecycle-run-environment
  - design-supervisor-lifecycle-status
  - design-supervisor-lifecycle-stats
---

# 空き容量を確かめる（claimと着地の検証の前）

2026-09-25に、終わったrunの`target/`が残って空きが1.6GiBまで減り、task 276の`integrate`の検証（`cargo llvm-cov`）が`No space left on device (os error 28)`で落ちた。supervisorはclaimの前と着地の検証の前に、queueのdirectory（run worktreeを置く`runs/`）のファイルシステムの空きを確かめ、足りなければclaimと着地を控えて掃除し、それでも足りないときだけinboxに1件知らせる（[ADR-0047](../../adr/0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定44、goal 34、task 377）。検証が容量不足で落ちる代わりに、容量不足という理由で止まる。

控えの仕組みと記録は[load averageによるclaimの控え](claim-hold.md)（task 327）と同じもので、理由`disk_space`を足し、着地の検証の控えにも同じ形の記録を使う。

## 閾値

`domain::disk::DiskConfig::needs`が、直近のrunのビルドの大きさから決める。

- 直近のrunのビルドの大きさ: `build_outputs_removed`（終わったrunのworktreeから`target/`などを消したときの記録。[Run worktrees](run-worktrees.md)）の新しい順に`sample_runs`件（既定20）の`bytes`の最大値
- claimの前: その最大値 × `claim_factor`（既定2）
- 着地の検証の前: その最大値 × `integrate_factor`（既定1.5）
- どちらも`min_free_bytes`（既定なし）を下限にする。記録がまだ無く`min_free_bytes`も無ければ確かめない

値は`dagq.toml`の`[disk]`（`sample_runs`・`min_free_bytes`は正の整数、`claim_factor`・`integrate_factor`は正の数。書式は[Run environment](run-environment.md)）で変える。読むのはmain checkoutの作業ファイルで、supervisorの起動時に読む（読めなければwarnを出して既定値）。libraryの`SuperviseOptions::disk`で上書きできる（testが使う）。直近のビルドの大きさは60秒ごとに読み直す。

空きは`statvfs(3)`の`f_bavail × f_frsize`（一般のprocessが使える量。`infrastructure::adapters::free_disk_bytes`）で、supervisorのpassごとに読む。読めなければ控えない。

## 判定と掃除

supervisorはpassの初め（`[run.env]`のprogramの検査の次、drainやhandoffの途中も）に`Supervisor::check_disk`で確かめる。

1. 空きがclaimかlandingの閾値の大きい方を下回れば、掃除を自動で走らせる（60秒に1回まで）: 終わったrunのビルド成果物の消し残しと、`completed` / `canceled`のtaskのworktreeの消し残し（[Run worktrees](run-worktrees.md)の`clean_ended_worktrees`と同じ）、`git worktree prune`。何か消えれば、queueイベント`auto_repaired`（`repair: disk_cleanup`、`layer: runtime`、`bytes`、`conditions: {free_bytes, needed_bytes, free_bytes_after}`、`detail: {bytes, runs}`、`supervisor`）を記録する。掃除の後に読み直した空きで判定する
2. claim: `fill_slots`の`hold_claims`が、空きとclaimの閾値を`ClaimHold::judge`に渡す。足りなければ理由`disk_space`で控え（load averageより先に判定する）、`claim_held`（`value`は空きbytes、`threshold`は要るbytes）を記録する。空きが戻れば`claim_resumed`
3. 着地: 着地slotを待つrun（`Phase::AwaitingSlot`）は、空きが着地の閾値を下回る間`begin_integration`をしない。runは`awaiting_integration`のままleaseを持ち、rebaseも検証も始めない。`approve_landing`の`land`のanswerも適用を待つ。着地を待つrunがあり足りない間はqueueイベント`landing_held`（payloadは`claim_held`と同じ。`message`は着地の文）を、空きが戻るか待つrunが無くなれば`landing_resumed`を記録する（`domain::claim_hold::LANDINGS`、`transition_of`）。着地の控えはhostのものではなく記録したsupervisorのslotのものなので、別の生きているsupervisorは`landing_resumed`で終えない（そのsupervisorが止まれば終えてよい）。drainやhandoffのsupervisorは待たず、leaseを返してrunを`awaiting_integration`のまま人に残す（`[run.env]`のprogramが無いときと同じ）

走っているrun（session・validation・review・resume・triage）には触れない。

## inboxに知らせる

掃除の後もclaimか着地の閾値を下回れば、queueで1件の`cost`のask（`kind: queue_hold`、`reason_category: cost`、`subject: disk`、optionsは`done` / `wait`）を開く（ADR-0047の決定42）。着地を待つrunは`affected`に足され（`ask_updated`）、questionの末尾の`Affected runs:`に並ぶ。ログインのaskと違い、sessionを止めるものではないので、`hold_of`（sessionの待ちと停滞の見張りがログインの控えを見る）には当たらない。claimだけが足りないときはrunを持たない（`NewHold::run_id`が`None`）。通知（`cmux notify`）は最初の1回だけ。

- 同じ不足の間は開き直さない。supervisorはこの不足でaskを開いたことを覚え、空きが戻るまで新しく開かない
- `done`（人が空けた）: supervisorがaskを閉じ、すぐ掃除し直して確かめる。まだ足りなければもう1件開く
- `wait`: supervisorがaskを閉じ、空きが戻るまで開き直さない
- 空きが戻れば、supervisorは開いているdiskのaskにruntimeとして答えて閉じ（`ask_answered`の`runtime_closed: true`、`answered_by: runtime`）、控えを解く

supervisorを起動し直すと、覚えていた「開いた」は消えるので、足りないままなら開いているaskに合流するか、無ければ1件開く。

## `status`と`stats`

- `status`: claimを控えているsupervisorの項目の`claim_hold`（理由`disk_space`を含む）と同じ形で、着地を控えているsupervisorは`landing_hold`（最新の`landing_held`のpayloadと`since`）を持つ（[`status`](status.md)）
- `stats`: `claim_holds.by_reason.disk_space`がclaimの控え、`landing_holds`（`claim_holds`と同じ`{count, secs, by_reason, held}`）が着地の控え。load averageの控え（`load_average`）とは理由で区別できる。掃除は`auto_repairs`の`by_layer.runtime.by_repair.disk_cleanup`に数える（[`stats`](stats.md)）
