---
id: design-supervisor-lifecycle-validation
type: design
title: "Validation"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0040
  - adr-0029
---

# Validation

`validating`のrunに対して、supervisorがセッションと同じleaseの下で、runごとのthreadで次を順に確認する。sessionは開いたまま（上の1）のことも、終了済みのこともある。最初に外れた項目が`failed`の理由（`last_error`）になり、以降は確認しない。

1. receiptが存在し、`Receipt`として解釈できる。
2. `run_id`が一致し、`result`が`succeeded`である。`tests`/`e2e`/`subagent_review`は`failed`でなく、`passed`には証跡、`not_applicable`には理由が空でなく書かれている。`commit`は完全なSHAである。
3. worktreeのHEADがrun branch `dagq/<run-id>`を指し、そのcommitがreceiptの`commit`と一致する。
4. commitがbase commitと異なり（commitなしを拒む）、base commitの子孫である。
5. `git status --porcelain --untracked-files=all`が空である。untracked fileもdirtyとみなす。
6. （欠番）taskの`verification_commands`はここでは実行しない。同じcommitに対する実行は`integrate`のrebase後の1回だけ（[ADR-0040](../../adr/0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)の決定1。[`integrate`](integrate.md#integrate)のステップ5）なので、validatingの所要時間はreceiptとGitの照合だけで、`verification_command`イベントも`<run-dir>/verify-N.log`も作らない。検証コマンドで壊れるcommitもここでは`awaiting_integration`になり、`integrate`で`needs_session`になって自動resumeされる。
7. **宣言パス**（[ADR-0029](../../adr/0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)）: taskの`paths`（`add --paths`）が空でなければ、run branchが今のmainから分かれた点（`GitRepository::merge_base`で`refs/heads/main`とreceiptの`commit`のmerge-base。rebaseしていなければbase commit、resumeしたsessionがrebaseしていればそのmain）からreceiptの`commit`までに変わったパス（他のtaskがmainに着地したパスは数えない。`GitRepository::changed_paths`: `git diff --name-only -z --no-renames`。renameは両側、削除も含む）のうち、どのglobにも合わないもの（`domain::scope::out_of_scope`）を探す。1–5をすべて通ったrunだけをここで見る。見つかればrunを`failed`ではなく`needs_session`にし、`last_error`を`changed paths outside the task's --paths: <paths>`にして、`validation_finished`（`status: needs_session`、`scope_violation`に外れたパスの配列、`allowed_paths`にtaskのglob）の後に`scope_violation`イベント（`paths`、`allowed`、`reason`。`status`は持たない）を記録する。8より先に見るので、両方が欠けていれば先にこちらを直させ、resume後の再validationで8を見る。後の扱い（`/exit`、close、自動resume）は8と同じで、resumeの依頼は宣言外のパスをrun branchから外す文面になる。`paths`の無いtaskでは7は何もしない。
8. **要求evidence**（ADR-0019の決定5）: taskの`required_evidence`（`add --evidence`）の各checkについて、receiptのそのcheckの`status`が`passed`で`evidence_or_reason`が空白でないこと（`Receipt::missing_evidence`）。1–5をすべて通ったrunだけをここで見るので、receiptやGitの不整合は従来どおり`failed`になる。要求されたcheckに限り、2の「`failed`でない」と「evidence_or_reasonが空でない」はここに回す（`Receipt::check_requiring`）ので、`failed`・`not_applicable`・空のevidenceはどれも`failed`ではなくここで欠落になる。要求されていないcheckの`failed`は従来どおり2で`failed`になる。欠けていればrunを`failed`ではなく`needs_session`にし、`last_error`を`evidence missing: e2e`（複数は`, `区切り、要求の順）にして、`validation_finished`（`status: needs_session`、`evidence_missing`に欠けたcheckの配列）の後に`evidence_missing`イベント（`checks`、`reason`。`status`は持たない: `stats`が`validation_finished`の`needs_session`で1回数えるため）を記録する。runのsessionには`/exit`を送り、終わったらworkspaceを閉じる（resumeは自分のworkspaceを開く）。supervisorはこのrunを[`needs_session`](needs-session.md#needs_session)の手順で自動resumeし、不足しているcheckの実行とreceiptの書き直しを依頼する。要求の無いtaskでは8は何もしない。

結果は`validation_finished`イベント（`status`、`result_commit`、`reason`、receiptの内容、7で外れたときだけ`scope_violation`と`allowed_paths`、8で欠けたときだけ`evidence_missing`、`receipt_observed`からこの検証までのload averageの`load_avg_mean` / `load_avg_max`（task 197。[`supervise`](supervise.md)の7））と`task_runs.result_commit`/`last_error`に保存する。4以降で拒否した場合もcommitは確認済みなので`result_commit`を残す。成功しても`awaiting_integration`はTaskを`in_progress`のまま保持し、着地まで依存taskを解放しない。

その後: `awaiting_integration`のrunは[Review](review.md#review-supervisor)に進む（leaseとsessionはそのまま）。`integration_approved`のあるrun（`integrate`が呼ばれた後にresumeしたrun）はreviewを待たずに`/exit`→close→着地する（ADR-0027の決定3）。`needs_session`（宣言外のパス、evidenceの欠落）は`/exit`→closeしてleaseを外す。`failed`は`/exit`を送り、workspaceは調査のため閉じずにleaseを外す。
