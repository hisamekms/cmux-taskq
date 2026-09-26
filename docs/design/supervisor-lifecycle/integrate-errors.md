---
id: design-supervisor-lifecycle-integrate-errors
type: design
title: "errorと復旧"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0008
---

# errorと復旧

mainを進める前のGit・ファイル・DBのerror（worktreeがない、mainのcheckoutにローカル変更があって`--ff-only`が失敗する、など）は`abort_integration`で`integration_error`イベントと`last_error`を書き、runを着地開始時のstatus（`awaiting_integration` / `needs_session`）に戻してleaseを解放する。`integrate`は非0で終わり、原因を直して再実行する。

`integrate`プロセスが途中で死ぬとrunは`integrating`のまま、leaseはstaleになる。`doctor`が`integrating`のrunをlease付きで報告し、leaseのPIDが死んでheartbeatが30秒以上古ければ`recover RUN_ID`が`awaiting_integration`に戻す（`run_recovered`の`previous_status: integrating`）。次の`integrate`は途中のrebaseをabortしてやり直す。mainを進めた後にDBの更新が失敗した場合はrunを戻さず、error messageに着地commitを含める（[ADR-0008](../../adr/0008-merge-queue-squash-landing.md)のConsequences）。
