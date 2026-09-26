---
id: design-supervisor-lifecycle-handoff
type: design
title: "Handoff"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-up-down
  - adr-0045
  - design-persistence
  - adr-0062
  - design-supervisor-lifecycle-waiting
---

# Handoff

[ADR-0045](../../adr/0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)の決定10・11。supervisorは、sessionの終わりを待たずに、自分のpidのまま別のbinaryをexecして続ける。use caseは`src/application/supervise/handoff.rs`、execは`src/main.rs`の`run`。

- **要求**: `supervisors`の`handoff_accepted`（schema v31。`supervise`が登録の直後に1を書く）が1の登録にだけ、`up`か`install`が`request_handoff(token, binary)`で`handoff_binary`と`handoff_requested_at`を書く。受けられない登録（引き継ぎを知らない前のbinary）には書かず（`false`）、`up`はdrainに、`install`は`not_handed_off`に回す。要求をDBに置くのは、signalはlaunchd modeとin-cmux modeで届け方が違い、区切りまで待たせる必要もあるため。
- **区切り**: ループの各passの先頭で、停止要求（SIGINT/SIGTERM）が無ければ`handoff_request(token)`を読む。要求があれば新しいclaim・adopt・resume・triage・observerの起動を止め、DBとrunのファイルから組み立て直せないslot（`Validating`・`AwaitingSlot`・`Landing`、つまり検証と着地が進行中のもの）だけを進める（`tick(true)`）。全slotが組み立て直せる状態になったら、observerの子プロセスとheadlessのreview / triage / plan reviewのjobを止め（plan reviewの行は同じtokenのまま未完了で残り、次のプロセスが最初のplan reviewでそれを`interrupted`にしてやり直す。triageはleaseを返し、次のプロセスがtriageし直す。reviewはleaseを保ち、次のプロセスがreviewし直す）、DBが持たない状態を、書いたsupervisorのtokenを添えて`runs/<id>/handoff.json`に書き（同じtokenの次のプロセスだけが読み、それ以外が残したfileは読まずに消す。resume中のsessionのworkspace・試行番号・送った依頼と時刻・`/exit`の有無、検証で`failed` / `needs_session`になって`/exit`を待つrunのsessionと`/exit`の有無）、`{"outcome":"handoff","binary","token","handed_over",…}`でループを抜ける。登録は消さない（heartbeatだけが止まり、execは数秒で終わるのでlease TTLの30秒を越えない）。停止要求は引き継ぎに勝ち、通常のdrainになる。
- **exec**: `main`は`supervise`の結果が`handoff`なら、queueの接続がすべて閉じた後で、自分の引数から`--handoff-token`を除いて`--handoff-token <token>`を足し、要求されたbinaryを`CommandExt::exec`で同じpidのまま起動する（SQLiteのfdはclose-on-execで開かれる）。execが失敗したら（fileが無い、実行できない）logに残し、同じbinaryのまま同じ引数で`supervise`をやり直すので、登録は前のbuild識別子で取り戻され、頼んだ側は「別のbuild識別子で戻った」として失敗を知る。in-cmux modeのsupervisorは同じcmux workspaceの同じプロセスのままで、launchd modeのsupervisorは同じpidなので`KeepAlive`は何もしない。
- **取り戻し**: execされた`supervise --handoff-token <token>`は、`resume_registration(token, pid, build識別子)`で登録の`binary_version`とheartbeatを更新し、要求の列を消す（tokenとpidが一致しなければerror）。`mode` / `workspace_id` / `started_at`とleaseは変えない。続けて`runs_leased_by(token)`の各runに`supervisor_handed_off`（`supervisor` / `pid` / `previous_version` / `version` / `status` / `state`）を記録し、slotを組み立て直す: `handoff.json`があればそこから（resumeは`ResumeWatch`、`/exit`待ちは`ExitWatch`。`/exit`は送り直さず、timeoutは今から数え直す）、`claimed` / `starting` / `running` / `validating`はadopt（[supervise](supervise.md)の5）と同じ`resume`、`awaiting_integration`は`adopt_review`（reviewの途中ならreviewし直し、`/exit`は送り直さない）、それ以外はleaseを返す（resumeとtriageが拾う）。adoptではないので`run_adopted`は書かない。execに30秒以上かかってleaseがstaleになり、その間に別のsupervisorがadoptしたrunは、[ADR-0039](../../adr/0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)のとおりそのsupervisorのものになる（取り戻したプロセスの`runs_leased_by`には出ない）。
- **待ち**: 人の答えを待つrun（[人の答えを待つrun](waiting.md)）のslotは組み立て直せるので、引き継ぎはそれを待たない。次のプロセスはslotを組み立て直した後、runのイベントから待ちと戻り待ちを戻す（`run_waiting_started`を記録し直さず、上限を超えていても待ちのまま。[ADR-0062](../../adr/0062-runs-waiting-for-a-person-leave-the-slot.md)の決定11）。
- 走っているrunのwrapperはclaim時の`runner`（前のbinaryのコピー）で動き続ける。互換の範囲のschemaならそのまま動く。
- **test**: `tests/it/runtime_handoff.rs`がin-processで、sessionの作業中の引き継ぎ（同じtokenのleaseのまま次のプロセスがreceipt・1回の`/exit`・検証まで進める）、検証で落ちて`/exit`を待つrunの引き継ぎ（`handoff.json`で`/exit`を送り直さない）、組み立て直せないrunのleaseの返却と消えた登録の拒否を、`tests/it/cli_version.rs`が実プロセスの`supervise`（stubのcmuxとClaude）に`install`と`install --rollback`で2回execさせて同じpidとtokenで続くことを、`tests/e2e.rs`の`install_hands_the_supervisor_over_while_a_session_works_and_the_run_lands`が実cmuxのworkerの作業中に`install`で引き継ぎ、そのrunが着地し、`--rollback`でもう一度引き継ぐことを確かめる。
