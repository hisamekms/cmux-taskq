---
id: design-supervisor-lifecycle
type: design
title: Supervisor and workspace lifecycle
status: current
created: 2026-09-21
updated: 2026-09-22
last_verified: 2026-09-22
scope: runtime
related:
  - adr-0002
  - adr-0003
  - adr-0006
  - adr-0007
  - adr-0008
  - design-persistence
  - design-provider-lifecycle
---

# Supervisor and workspace lifecycle

```text
ready task (dependencies completed)
  → claim TaskRun + run lease      ─┐
  → create Git worktree              │ up to --parallel N runs at once,
  → create cmux workspace            │ each with this state machine
  → start session wrapper            │
  → start Claude/Codex               │
  → running                          │
  → completion receipt               │
  → validate commit, tests, clean state (own thread)
  → close cmux workspace             │
  → release run lease               ─┘
  → awaiting_integration / succeeded
  → integrate (one at a time, FIFO by validation):
      integrating → rebase onto main → re-validate → squash-land on main
      → run integrated (result_commit = landed commit), task completed
      → worktree and branch removed; history kept at refs/taskq/runs/<run-id>
    conflict / failed re-validation → needs_session
      → SV resumes a session in the worktree; it resolves, reruns verification,
        rewrites the receipt → integrate ID again (failed receipt → run failed)
  → dependents become candidates; the resident loop claims them from the landed main
```

## Implementation status

ステップ3で`claim`から`running`、セッション終了検知までを、ステップ4の[005](../journal/005-receipt-validation.md)でreceiptの検証と`awaiting_integration`への遷移を、[006](../journal/006-workspace-close.md)で受理後のworkspace終了を、[007](../journal/007-session-exit-request.md)でreceipt受領後の終了要求を、[009](../journal/009-doctor-recover.md)で`doctor`/`recover`を、[008](../journal/008-integration-confirm.md)で統合確認`integrate`と`completed`への遷移を`src/runtime.rs`に実装した。ステップ6の[017](../journal/017-parallel-runs.md)（[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)）でleaseをrun単位にし、`supervise`を上限付き並列の常駐ループにした。ステップ7の[018](../journal/018-merge-queue.md)（[ADR-0008](../adr/0008-merge-queue-squash-landing.md)）で`integrate`を手動mergeの確認からruntimeによる着地（rebase → 再検証 → squash）に置き換えた。

## `supervise`

`cmux-taskq supervise [--parallel N] [--once]`はrepository内の専用ターミナルで実行する常駐ループで、依存が解けたtaskを上限N（既定4）まで同時に実行する。queueはcwdから解決し（[persistence](persistence.md)のQueue location）、repositoryのcheckoutもcwdを使う。`--db PATH`と`--repo REPO`はそれぞれの明示override（[016](../journal/016-queue-per-repository.md)）。

起動時:

1. DBのpathを正規化し、checkoutのroot、Git common directoryを取得する。DBはworktree外か、common directory配下に置く（ユーザーDIRのqueueは常に満たす）。worktreeの作成元は`repo_path`に記録したcheckout。
2. cmux（`ping`）とClaude（`--version`）のpreflightを行う。
3. queueをrepositoryに束縛する（`bind_repository`）。別repositoryに束縛済みなら開始しない。queue全体の排他はなく、同じqueueに別のsupervisorがいても構わない。
4. supervisorプロセスのtoken（UUID）を作り、別スレッドで2秒ごとにそのtokenの全leaseのheartbeatを更新する（`UPDATE run_leases ... WHERE token=?`）。heartbeatの失敗はループで検知し、全runに`runtime_error`を記録してleaseを残したまま終了する（プロセス終了後にstaleになる）。

ループ（1秒ごと）:

5. **claim**: active runが上限未満で、`candidates`が空でなければ、`refs/heads/main`を読み直してbase commitにし、`claim_for_supervisor`でrun・`supervisor_token`・lease行を1トランザクションで作る。`integrate`で依存が解けたtaskは次のループで、先行taskを含む`main`から始まる。
6. **provision**: run管理領域（DBと同じdirの`runs/<run-id>/`）のpath、branch `taskq/<run-id>`、worktree（`runs/<run-id>/worktree`）、receipt、logのpathを`run_planned`として先にDBへ保存し、ディレクトリ、`prompt.txt`、runtimeバイナリのスナップショット`runner`、worktreeを作り、cmux workspaceを`--cwd worktree --command '<runner> --db ... session --run ... --lease <token> --claude ...'`で作成して、`identify`で解決したUUIDを`workspace_created`として保存する。wrapperにはDBのpathを`--db`で明示的に渡す。provisioningの失敗は環境要因とみなし、そのrunをabandon（下記）した上で以後のclaimを止め、active runをdrainしてから非0で終了する。
7. **監視**: runごとの`SessionWatch`が、wrapperの登録（45秒以内）、wrapper heartbeat（30秒以内）、receiptファイルの出現、idle marker、wrapperの終了を確認する。receiptの出現は`receipt_observed`（`validated: false`）として記録するだけで、セッション終了とは別に扱う。receipt観測後にidle markerがreceiptより新しければ`session_idle_observed`を記録し、`WorkspaceBackend::send_exit`で一度だけ終了を要求して`exit_requested`を記録する（下記）。
8. wrapper終了後に画面を`terminal-final.txt`へ保存し、`supervision_finished`でrunを終了コード0なら`validating`、それ以外なら`failed`にする。非0のときは同じトランザクションで`last_error`に`session exited with code N`を書き、`show`だけで理由が分かるようにする。Taskは`in_progress`のまま残す。
9. `validating`のrunはreceipt検証（下記）をrunごとのthread（専用SQLite接続）で行い、ループは完了を待ちながら他のrunを監視し続ける。完了したら`validation_finished`でrunを`awaiting_integration`または`failed`にする。
10. `awaiting_integration`になったrunだけ`cmux workspace close <workspace_id>`でworkspaceを閉じ、`OK workspace:N`の応答を確認して`workspace_closed`（`task_runs.workspace_closed_at`）を記録する。worktreeとbranchは統合まで残す。closeが失敗したら`cleanup_failed`イベントと`last_error`に記録し、runは`awaiting_integration`、`workspace_closed_at`はnullのままにする。
11. `awaiting_integration`または`failed`になったrunのleaseを解放する（`lease_released`）。
12. active runがなく、`--once`か停止要求（下記）か、provisioning失敗でclaimを止めていればループを抜ける。それ以外はactive runがない間2秒ごとに`candidates`を見る。

結果は`{"outcome": "finished" | "stopped", "runs": [休止したrun], "errors": [{run_id, task_id, message}]}`。SIGINT/SIGTERMは1回目でclaimを止めてactive runの終了を待ち（graceful drain）、2回目で既定の動作（即終了）になる。即終了したsupervisorのleaseは30秒でstaleになる。

### 1 runの異常（abandon）

wrapper heartbeat切れ、終了要求のtimeout、検証処理そのもの（Git呼び出しやDB）の失敗、closeの記録失敗など、監視中のruntime errorは**そのrunだけ**を手放す: `last_error`と`runtime_error`イベント（`lease_released: true`）を書き、そのrunのlease行を削除し、status・`run_processes`・workspace・worktreeは変えない。supervisorは他のrunを続け、結果の`errors`にそのrunを載せる。leaseを消すのは、常駐supervisorが生きている間も`recover`がrunのprocessだけで判定できるようにするため。taskは未完了runで占有されたままなので二重実行にはならず、未登録のwrapperはleaseがなければ登録できずClaudeを起動しない。`show`と`doctor`で確認し、[recover](#recover-run_id)で扱う。

## `session` wrapper

cmux workspaceが起動する隠しコマンド。TTYが必要で、パイプからは起動しない。

1. `workspace_id`が保存されるまで待ち（45秒以内）、wrapperのPIDを一度だけ登録する。leaseが無効なら登録できない。
2. `prompt.txt`を読み、providerのコマンドでagentを起動して`agent_started`を記録し、runを`running`にする。
3. 1秒ごとにheartbeatを更新しながら子プロセスをwaitする。DB障害中も子プロセスの所有を手放さない。
4. 終了コードを`session_exited`として記録する。agent起動後のエラーでは子プロセスが生きている可能性を考慮し、終了を記録しない。

## Receipt and session exit

receiptの受領とセッション終了は別の事象である。agentはreceiptを`<run-dir>/receipt.json`へ一時ファイルからrenameして公開し、応答完了後もセッションを維持する。セッション終了はwrapperが記録する終了コード（`session_exited`）だけで確認し、画面文言は使わない。

receipt受領後の終了要求は次の順で自動化している。

1. **idle判定**: Claude adapterがrunごとの`<run-dir>/claude-settings.json`に`Stop` hookを書き、`--settings`で渡す。hookはClaudeが応答を終えるたびにstdinのイベントJSON（`session_id`、`hook_event_name`など）を`<run-dir>/idle.json`へ一時ファイル + renameで書く。supervisorはreceiptを観測した後、`idle.json`のmtimeが`receipt.json`のmtime以上なら「receipt提出後に応答が完了した」と判定する。receiptより古いmarker（operatorへの質問で止まった以前のturnなど）は無視する。判定の根拠（両ファイルのmtime、hookのフィールド）は`session_idle_observed`に記録する。権限確認や質問で止まっているturnでは`Stop`が発火しないため、その間は終了要求を送らない。
2. **終了要求**: `WorkspaceBackend::send_exit(workspace_id)`で、cmuxでは`cmux send --workspace <uuid> -- /exit`の後に`cmux send-key --workspace <uuid> -- enter`を送る。operatorが打つのと同じ経路で、1回だけ送り、再送やプロセスのkillはしない。`exit_requested`に`timeout_secs`を記録する。
3. **終了確認**: wrapperの`session_exited`を待ち、通常どおり`supervision_finished`へ進む。要求から`WorkspaceBackend::exit_timeout`（cmuxは120秒）以内に終了しなければ`exit_request_timed_out`を記録し、runtime errorとして`supervise`を終える。runは`running`、workspace・worktree・leaseはそのまま残り、人が`/exit`を送るか`recover`（[009](../journal/009-doctor-recover.md)）で扱う。この場合もwrapperは後から`session_exited`を記録する。

operatorの手動`/exit`はいつでも有効で、markerがない（hookが無効化されているなど）場合は従来どおり手動終了を待つ。

receiptの形式は`src/domain.rs`の`Receipt`で、promptとREADMEに同じ契約を書いている。

```json
{"run_id": "...", "result": "succeeded | failed", "commit": "full SHA",
 "tests": {"status": "passed | failed | not_applicable", "evidence_or_reason": "..."},
 "e2e": {...}, "subagent_review": {...}, "summary": "..."}
```

## Validation

`validating`のrunに対して、supervisorが終了したセッションと同じleaseの下で、runごとのthreadで次を順に確認する。最初に外れた項目が`failed`の理由（`last_error`）になり、以降は確認しない。

1. receiptが存在し、`Receipt`として解釈できる。
2. `run_id`が一致し、`result`が`succeeded`である。`tests`/`e2e`/`subagent_review`は`failed`でなく、`passed`には証跡、`not_applicable`には理由が空でなく書かれている。`commit`は完全なSHAである。
3. worktreeのHEADがrun branch `taskq/<run-id>`を指し、そのcommitがreceiptの`commit`と一致する。
4. commitがbase commitと異なり（commitなしを拒む）、base commitの子孫である。
5. `git status --porcelain --untracked-files=all`が空である。untracked fileもdirtyとみなす。
6. taskの`verification_commands`を順に`/bin/sh -c`でworktree内で実行する。出力は`<run-dir>/verify-N.log`、終了コードと末尾は`verification_command`イベントに記録する。1件でも非0なら失敗。各コマンドは30分でタイムアウトし、その場合は検証処理のエラーとして扱う。

結果は`validation_finished`イベント（`status`、`result_commit`、`reason`、receiptの内容）と`task_runs.result_commit`/`last_error`に保存する。4以降で拒否した場合もcommitは確認済みなので`result_commit`を残す。成功しても`awaiting_integration`はTaskを`in_progress`のまま保持し、下記の統合確認まで依存taskを解放しない。

## `integrate`

`cmux-taskq integrate ID`（taskの`awaiting_integration`または`needs_session`のrun）と`cmux-taskq integrate --next`（`awaiting_integration`のrunを検証完了の古い順に1件）は、検証済みのrunをruntimeが`main`へ着地させる操作で、SVがレビュー後にrepository内で実行する（[ADR-0008](../adr/0008-merge-queue-squash-landing.md)）。`--db PATH`と`--repo REPO`は明示override。repositoryは`GitRepository::inspect`で開き、common directoryが`queue_repository.git_common_dir`と一致することを要求する。

1. **スロット**: runを`integrating`にし、このプロセスのtokenで`run_leases`の行を作る（`begin_integration`、`integration_started`）。同時に`integrating`のrunは1件（`one_integrating_run_per_queue`）で、別のrunが着地中ならerror。leaseは`supervise`と同じthreadで2秒ごとにheartbeatし、`status`/`doctor`に`integrating`のrunとして並ぶ。`--next`の順序は`validation_finished`イベントのid順で、`needs_session`のrunは取らない。
2. **worktreeの前処理**: worktreeが存在し、run branch `taskq/<run-id>`をcheckoutしていること。途中のrebase（`rebase-merge` / `rebase-apply`）が残っていれば`git rebase --abort`する（`integration_rebase_aborted`）。
3. **receiptの検査**: `<run-dir>/receipt.json`がparseでき、`result`が`succeeded`で（`failed`ならrunを`failed`にして終わる。下記）、`Receipt::check`を通り、`commit`がworktreeの現在のHEADに一致する。衝突なしのrunではHEAD = `result_commit`なので検証済みのreceiptがそのまま通る。`needs_session`から戻るrunでは、セッションが新しいheadでreceiptを書き直したことの検出になる。worktreeはcleanであること。
4. **rebase**: `git rebase --no-autostash --no-verify <main head>`（main headは着地開始時に読んだ`refs/heads/main`）。すでにmainの上にあればno-op。衝突したら`git diff --name-only --diff-filter=U`とGitの出力を取り、`rebase --abort`でworktreeを検証済みheadに戻して`needs_session`にする。成功したら`integration_rebased`（`head_before`、`head_after`）を記録する。
5. **再検証**: rebase後のHEADがmain headと異なり（同じなら「commitが残らない」として`needs_session`。変更が不要ならセッションが`failed` receiptを書く）、main headの子孫であること。`git status --porcelain --untracked-files=all`が空であること。taskの`verification_commands`を順に`/bin/sh -c`でworktree内で再実行し、出力を`<run-dir>/integrate-verify-N.log`、結果を`verification_command`イベント（`phase: integration`）に残す。1件でも非0なら`needs_session`。
6. **着地**: `git commit-tree <HEAD>^{tree} -p <main head>`で1 commitを作る。messageはtaskのtitle、receiptの`summary`（空なら省略）、trailer `Taskq-Task: <task id>` / `Taskq-Run: <run id>`。`refs/taskq/runs/<run-id>`をrebase後のHEADに向けてから、mainをcheckoutしているworktree（`git worktree list --porcelain`）があればそこで`git merge --ff-only <commit>`、なければ`git update-ref refs/heads/main <commit> <main head>`でmainを進める。
7. **完了**: 1トランザクションでrunを`integrated`、`result_commit`を着地commit、`last_error`をnull、Taskを`completed`にし、lease行を消して`run_integrated`（`result_commit`、`source_commit`、`main_before`、`history_ref`、`message`、`git_common_dir`）、`lease_released`、`task_status_changed`を記録する（`finish_integration`）。
8. **後始末**: `git worktree remove --force <worktree>`と`git branch -D taskq/<run-id>`（`worktree_removed`）。失敗は`cleanup_failed`イベントと`last_error`に残し、statusは変えない。

結果は`IntegrationOutcome`: `{"outcome":"integrated","task":…,"run":…}`、`{"outcome":"needs_session","run":…,"main":…,"reason":…}`、`{"outcome":"failed","run":…,"reason":…}`、`--next`で対象がなければ`{"outcome":"no_run_awaiting"}`。

### `needs_session`

衝突（4）と再検証の失敗（5）は`defer_integration`でrunを`needs_session`にし、理由を`last_error`、詳細（衝突ファイル、Gitの出力の末尾、rebase後のheadなど）を`integration_deferred`イベントに書いて、lease行を消しスロットを空ける。worktreeは衝突なら検証済みhead、再検証の失敗ならrebase済みのheadに置いたまま残す。runはTaskを占有し続け、`ready`/`cancel`はできない。

SVは`cmux workspace create --cwd <worktree> --command "claude --resume <run-id>"`でセッションを開き直し、`last_error`の理由と「mainへrebaseして解消し、検証コマンドを再実行し、新しいheadでreceiptを書き直す」指示を送る。完了を確認したら`integrate ID`で再開する。手順は1から同じで、rebaseはmainが動いていなければno-op、動いていれば再びrebaseする（再衝突すれば再び`needs_session`）。receiptの`commit`が現在のHEADと一致しなければ、セッションが終わっていないものとして理由付きで`needs_session`のまま。

セッションが変更不要と判断した場合はreceiptを`result: failed`と理由（`summary`）で書き直す。`integrate ID`は`fail_integration`でrunを`failed`にし（`integration_failed`）、mainには触れない。worktreeは残る。再試行は`ready ID`、取り消しは`cancel ID`。

### errorと復旧

mainを進める前のGit・ファイル・DBのerror（worktreeがない、mainのcheckoutにローカル変更があって`--ff-only`が失敗する、など）は`abort_integration`で`integration_error`イベントと`last_error`を書き、runを着地開始時のstatus（`awaiting_integration` / `needs_session`）に戻してleaseを解放する。`integrate`は非0で終わり、原因を直して再実行する。

`integrate`プロセスが途中で死ぬとrunは`integrating`のまま、leaseはstaleになる。`doctor`が`integrating`のrunをlease付きで報告し、leaseのPIDが死んでheartbeatが30秒以上古ければ`recover RUN_ID`が`awaiting_integration`に戻す（`run_recovered`の`previous_status: integrating`）。次の`integrate`は途中のrebaseをabortしてやり直す。mainを進めた後にDBの更新が失敗した場合はrunを戻さず、error messageに着地commitを含める（[ADR-0008](../adr/0008-merge-queue-squash-landing.md)のConsequences）。

## Cleanup and recovery

workspaceの終了はsupervisorが行う。leaseはrun単位で（[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)）、1 runの復旧が他のrunに影響しない。receipt検証を通った`awaiting_integration`のrunだけが対象で、workspaceだけを閉じ、worktreeとbranchは`integrate`が着地するまで残す（着地後に`integrate`が削除する）。`failed`（非0終了、検証拒否）、provisioningや検証処理のエラー、wrapper heartbeat切れの場合はworkspaceもworktreeも調査のため残し、closeを呼ばない。

cmux 0.64.25の`workspace create --command`はコマンドをログインシェルに打ち込む形で起動し、wrapperが終了してもシェルとworkspaceは残る（[010](../journal/010-failure-path-smoke.md)の実機確認。[015](../journal/015-e2e-happy-path.md)が観測した「終了後1〜2秒で自動的に閉じる」挙動は010の環境では再現せず、cmuxの設定に依存するとみられる）。どちらの場合もsupervisorの手順は同じで、先にworkspaceが消えていれば`cmux workspace close`は`not_found`で失敗して`cleanup_failed`になり、wrapper終了直後の`read-screen`も`screen_capture_failed`になりうる。`failed`・`interrupted`のrunのworkspaceは誰も閉じないので、調査が済んだらoperatorが`show`の`workspace_id`を`cmux workspace close`に渡して閉じる（`doctor`は未完了runしか列挙しないので`failed`・`interrupted`のrunは出ない）。

closeの成否は`task_runs.workspace_closed_at`で表す。nullは「閉じたことを確認していない」で、closeの失敗だけでなく、cmuxが閉じた後にDBへ書けなかった場合も含む。closeの失敗は`cleanup_failed`イベントと`last_error`に残るが、run状態は変えない。閉じていないworkspaceをcleaned扱いにせず、再試行は`doctor`/`recover`（[009](../journal/009-doctor-recover.md)）で扱う。

supervisorの再起動ではrunごとのleaseとheartbeatを確認し、孤児プロセスを勝手に再実行しない。ユーザーが`recover`で明示的に復旧した後に新しいTaskRunを作る。

### `status`

`cmux-taskq status`はprocessを調べずにleaseだけを返す。`supervisors`はleaseのPIDごとに`pid`、`alive`、`run_ids`、`heartbeat_age_secs`、`stale`（着地中の`integrate`プロセスも1つとして並ぶ）。`runs`は未完了run（`claimed`/`starting`/`running`/`validating`/`integrating`）ごとに`run_id`、`task_id`、`status`、`workspace_id`、`lease`（なければnull）。`awaiting_integration`と`needs_session`はプロセスを持たないので並ばない。

### `doctor`

`cmux-taskq doctor`は状態を変えずにJSONで報告する。

- `supervisors`: `status`と同じ。
- `runs`: `claimed`/`starting`/`running`/`validating`/`integrating`のrunごとに、`workspace_id`、worktreeとrun directoryとreceiptの存在、`last_error`、そのrunの`lease`（PID、`kill -0`による生存、heartbeatの経過秒数、30秒を超えた`stale`。なければnull）、登録済みwrapper/agentプロセスのPID・生存・heartbeat経過秒数・終了コード。`exited_at`が記録済みのプロセスはPIDが再利用されうるため生存確認せず`alive: null`にする。
- `blockers`: そのrunの`recover`を拒む理由の一覧。そのrunのprocessとleaseだけを見る。空なら`recoverable: true`。

cmux workspaceの存在は確認しない（cmuxなしで動く）。IDを見てユーザーが`cmux workspace list`で確認する。

### `recover RUN_ID`

1. runが`claimed`/`starting`/`running`/`validating`/`integrating`でなければ拒否する。
2. `doctor`と同じ確認を行い、未終了として登録されたプロセスのPIDが生きている、そのrunのleaseのheartbeatが30秒以内、leaseのPIDが生きている、のいずれかなら拒否する。heartbeatが止まったまま生きているsupervisorはleaseを奪わず、ユーザーが止める。supervisorがabandonしたrunはleaseがないので、processが止まれば復旧できる。
3. `BEGIN IMMEDIATE`の中でそのrunのleaseが新鮮でないことと`run_processes`の行数が確認時と同じことを再検査し、runを`interrupted`（`integrating`なら`awaiting_integration`: 検証済みの成果は残っており、次の`integrate`が途中のrebaseをabortしてやり直す）にし、確認した内容を`run_recovered`イベント（`previous_status`、`status`、`lease_deleted`、`run`）に記録し、そのrunのleaseだけを削除する。

他のrun、そのlease・process、`run_processes`、worktree、branch、workspace、run directoryは触らない。Taskは`in_progress`のまま残る。再試行は`ready ID`（編集するなら`draft ID`）で行い、動いているsupervisor（または次のsupervisor）が新しいTaskRunと新しいworktreeを作る。`recover`はTaskを`ready`に戻さない: 復旧と再実行は別の判断であり、`failed`で止まったTaskの再試行と同じ経路にまとめるため。
