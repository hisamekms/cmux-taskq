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
  - design-persistence
  - design-provider-lifecycle
---

# Supervisor and workspace lifecycle

```text
ready task
  → claim TaskRun
  → create Git worktree
  → create cmux workspace
  → start session wrapper
  → start Claude/Codex
  → running
  → completion receipt
  → validate commit, tests, clean state
  → close cmux workspace
  → awaiting_integration / succeeded
  → (manual merge into main)
  → integrate: run integrated, task completed
```

## Implementation status

ステップ3で`claim`から`running`、セッション終了検知までを、ステップ4の[005](../journal/005-receipt-validation.md)でreceiptの検証と`awaiting_integration`への遷移を、[006](../journal/006-workspace-close.md)で受理後のworkspace終了を、[007](../journal/007-session-exit-request.md)でreceipt受領後の終了要求を、[009](../journal/009-doctor-recover.md)で`doctor`/`recover`を、[008](../journal/008-integration-confirm.md)で統合確認`integrate`と`completed`への遷移を`src/runtime.rs`に実装した。

## `supervise`

`cmux-taskq --db PATH supervise --repo REPO`は専用ターミナルで実行し、1件だけ処理して終了する。

1. DBのpathを正規化し、repositoryのroot、Git common directory、`refs/heads/main`のcommitを取得する。DBはworktree外か、common directory配下に置く。
2. cmux（`ping`）とClaude（`--version`）のpreflightを行う。
3. supervisor leaseを取得する。既存leaseがある、未完了runがある、DBが別repositoryに束縛されている場合は開始しない。staleなleaseも自動では奪わない。
4. 別スレッドで2秒ごとにleaseのheartbeatを更新する。heartbeatの失敗は監視ループで検知し、runを保持したまま終了する。
5. `main`をbase commitとしてclaimする。候補がなければleaseを解放して`no_ready_task`を返す。
6. run管理領域`<db>.runs/<run-id>/`のpath、branch `taskq/<run-id>`、worktree、receipt、logのpathを`run_planned`として先にDBへ保存し、その後にディレクトリ、`prompt.txt`、runtimeバイナリのスナップショット`runner`、worktreeを作る。
7. cmux workspaceを`--cwd worktree --command '<runner> --db ... session --run ... --lease ... --claude ...'`で作成し、`identify`で解決したUUIDを`workspace_created`として保存する。
8. 監視ループで、wrapperの登録（45秒以内）、wrapper heartbeat（30秒以内）、receiptファイルの出現、idle marker、wrapperの終了を確認する。receiptの出現は`receipt_observed`（`validated: false`）として記録するだけで、セッション終了とは別に扱う。receipt観測後にidle markerがreceiptより新しければ`session_idle_observed`を記録し、`WorkspaceBackend::send_exit`で一度だけ終了を要求して`exit_requested`を記録する（下記）。
9. wrapper終了後に画面を`terminal-final.txt`へ保存し、`supervision_finished`でrunを終了コード0なら`validating`、それ以外なら`failed`にする。Taskは`in_progress`のまま残す。
10. `validating`なら同じleaseのままreceiptを検証し（下記）、`validation_finished`でrunを`awaiting_integration`または`failed`にする。
11. `awaiting_integration`になったrunだけ、同じleaseのまま`cmux workspace close <workspace_id>`でworkspaceを閉じ、`OK workspace:N`の応答を確認して`workspace_closed`（`task_runs.workspace_closed_at`）を記録する。worktreeとbranchは統合まで残す。closeが失敗したら`cleanup_failed`イベントと`last_error`に記録し、runは`awaiting_integration`、`workspace_closed_at`はnullのままにする。最後にleaseを解放する。

作成や通信（終了要求の送信を含む）に失敗した場合は、セッションが生きている可能性があるためleaseを解放せず、リソースも削除しない。`last_error`と`runtime_error`イベントに原因を記録し、`show`と`status`で確認する。検証の判定ではなく検証処理そのもの（Git呼び出しやDB）が失敗した場合も同じ扱いで、runは`validating`のまま残る。

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

`validating`のrunに対して、supervisorが終了したセッションと同じleaseの下で次を順に確認する。最初に外れた項目が`failed`の理由（`last_error`）になり、以降は確認しない。

1. receiptが存在し、`Receipt`として解釈できる。
2. `run_id`が一致し、`result`が`succeeded`である。`tests`/`e2e`/`subagent_review`は`failed`でなく、`passed`には証跡、`not_applicable`には理由が空でなく書かれている。`commit`は完全なSHAである。
3. worktreeのHEADがrun branch `taskq/<run-id>`を指し、そのcommitがreceiptの`commit`と一致する。
4. commitがbase commitと異なり（commitなしを拒む）、base commitの子孫である。
5. `git status --porcelain --untracked-files=all`が空である。untracked fileもdirtyとみなす。
6. taskの`verification_commands`を順に`/bin/sh -c`でworktree内で実行する。出力は`<run-dir>/verify-N.log`、終了コードと末尾は`verification_command`イベントに記録する。1件でも非0なら失敗。各コマンドは30分でタイムアウトし、その場合は検証処理のエラーとして扱う。

結果は`validation_finished`イベント（`status`、`result_commit`、`reason`、receiptの内容）と`task_runs.result_commit`/`last_error`に保存する。4以降で拒否した場合もcommitは確認済みなので`result_commit`を残す。成功しても`awaiting_integration`はTaskを`in_progress`のまま保持し、下記の統合確認まで依存taskを解放しない。

## `integrate`

`cmux-taskq --db PATH integrate ID [--repo REPO]`は、人がrun branch `taskq/<run-id>`をmainへmergeした後に実行する。supervisorとは独立した操作で、leaseを取らない。

1. taskの`awaiting_integration`のrunを取る。なければerror（`integrated`済みのtaskも同じ）。同じtaskにこの状態のrunは制約で高々1件。
2. repositoryは既定で`task_runs.repo_path`、`--repo`があればそのpathを`GitRepository::inspect`で開き、common directoryが`queue_repository.git_common_dir`と一致することを要求する。
3. `git merge-base --is-ancestor <result_commit> refs/heads/main`で確認する。merge commitでもfast-forwardでもresult commit自体がmainの祖先になる。squash / cherry-pickは別のSHAになるため祖先にならず、`{"outcome":"not_integrated","run":…,"main":…,"reason":…}`を返してDBは変えない（同等性判定は後回し）。
4. 祖先なら1トランザクションでrunを`integrated`、Taskを`completed`にし、`run_integrated`（`result_commit`、`main`、`git_common_dir`）と`task_status_changed`を記録する。statusを条件にしたUPDATEで、二重実行や同時実行は0行更新のerrorになる。

worktreeとbranchは削除しない。統合確認後の削除は人が行う。

## Cleanup and recovery

workspaceの終了はsupervisorが行う。receipt検証を通った`awaiting_integration`のrunだけが対象で、workspaceだけを閉じ、worktreeとbranchはmainへの反映まで残す。`failed`（非0終了、検証拒否）、provisioningや検証処理のエラー、wrapper heartbeat切れの場合はworkspaceもworktreeも調査のため残し、closeを呼ばない。

closeの成否は`task_runs.workspace_closed_at`で表す。nullは「閉じたことを確認していない」で、closeの失敗だけでなく、cmuxが閉じた後にDBへ書けなかった場合も含む。closeの失敗は`cleanup_failed`イベントと`last_error`に残るが、run状態は変えない。閉じていないworkspaceをcleaned扱いにせず、再試行は`doctor`/`recover`（[009](../journal/009-doctor-recover.md)）で扱う。

supervisorの再起動ではleaseとheartbeatを確認し、孤児プロセスを勝手に再実行しない。ユーザーが`recover`で明示的に復旧した後に新しいTaskRunを作る。

### `doctor`

`cmux-taskq --db PATH doctor`は状態を変えずにJSONで報告する。

- `supervisor`: leaseのPID、`kill -0`による生存、heartbeatの経過秒数、30秒を超えた`stale`。leaseがなければnull。
- `runs`: `claimed`/`starting`/`running`/`validating`のrunごとに、`workspace_id`、worktreeとrun directoryとreceiptの存在、`last_error`、登録済みwrapper/agentプロセスのPID・生存・heartbeat経過秒数・終了コード。`exited_at`が記録済みのプロセスはPIDが再利用されうるため生存確認せず`alive: null`にする。
- `blockers`: そのrunの`recover`を拒む理由の一覧。空なら`recoverable: true`。

cmux workspaceの存在は確認しない（cmuxなしで動く）。IDを見てユーザーが`cmux workspace list`で確認する。

### `recover RUN_ID`

1. runが`claimed`/`starting`/`running`/`validating`でなければ拒否する。
2. `doctor`と同じ確認を行い、未終了として登録されたプロセスのPIDが生きている、leaseのheartbeatが30秒以内、leaseのPIDが生きている、のいずれかなら拒否する。heartbeatが止まったまま生きているsupervisorはleaseを奪わず、ユーザーが止める。
3. `BEGIN IMMEDIATE`の中でleaseが新鮮でないことと`run_processes`の行数が確認時と同じことを再検査し、runを`interrupted`にし、確認した内容を`run_recovered`イベント（`previous_status`、`lease_deleted`、`supervisor`、`run`）に記録し、leaseを削除する。

`run_processes`、worktree、branch、workspace、run directoryは触らない。Taskは`in_progress`のまま残る。再試行は`ready ID`（編集するなら`draft ID`）で行い、次の`supervise`が新しいTaskRunと新しいworktreeを作る。`recover`はTaskを`ready`に戻さない: 復旧と再実行は別の判断であり、`failed`で止まったTaskの再試行と同じ経路にまとめるため。
