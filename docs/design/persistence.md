---
id: design-persistence
type: design
title: SQLite persistence
status: current
created: 2026-09-21
updated: 2026-09-23
last_verified: 2026-09-23
scope: persistence
related:
  - adr-0003
  - adr-0006
  - adr-0007
  - adr-0008
  - adr-0009
  - adr-0010
  - adr-0011
  - adr-0012
  - adr-0014
  - adr-0017
  - adr-0020
  - adr-0024
  - design-domain-model
---

# SQLite persistence

SQLiteはキューの正本であり、プロセス間共有と再起動後の復旧に使う。stdoutは正本にしない。ステップ2で4テーブル、ステップ3で3テーブル、ステップ4で`task_runs.workspace_closed_at`列を実装し、[008](../journal/008-integration-confirm.md)で`task_runs`を作り直して`integrated`を加え（schema version 4）、[017](../journal/017-parallel-runs.md)でleaseをrun単位の`run_leases`に移してqueue全体の実行枠を外し（schema version 5、[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)）、[018](../journal/018-merge-queue.md)で`task_runs`を再び作り直して`integrating`と`needs_session`を加え、統合スロットの部分UNIQUE indexを足した（schema version 6、[ADR-0008](../adr/0008-merge-queue-squash-landing.md)）。`0007_supervisors.sql`は常駐supervisorの登録表`supervisors`を足した（schema version 7）。`0008_goals.sql`は`goals`、`tasks.goal_id` / `tasks.context`を足し、`run_events`を作り直してgoal単位のイベントを持てるようにした（schema version 8、[ADR-0009](../adr/0009-goal-groups-tasks.md)。ADRの「schema v6」は7がsupervisorsに使われたためv8と読み替える）。`0009_supervisor_mode.sql`は`supervisors`に`mode`と`workspace_id`を足した（schema version 9、[ADR-0011](../adr/0011-cmux-socket-password-and-in-cmux-fallback.md)）。`0012_queue_events.sql`は`run_events`を作り直し、`backend_call_failed`だけはtaskにもgoalにも紐づかない行を持てるようにした（schema version 12、task 109）。

```text
tasks
task_dependencies
task_runs
run_events
queue_repository     -- 0002: キューを束縛するGit common directory（1行）
run_processes        -- 0002: runごとのwrapper/agentのPID、heartbeat、終了コード
run_leases           -- 0005: runごとのsupervisor token、PID、heartbeat（0002のsupervisor_leasesを置き換え）
supervisors          -- 0007: 常駐superviseプロセスの登録（token主キー、PID、parallel、started_at、heartbeat_at）
                     -- 0009: mode（'launchd' | 'in_cmux' | null）とworkspace_id
goals                -- 0008: 複数taskが解く課題（title、description、acceptance、constraints、doc、closed_at、verdict）
                     -- 0013: status（'draft' | 'open'、既定'open'）
```

`task_dependencies(task_id, predecessor_id)`は依存関係を保存する。TaskRunは試行ごとに新しい行を作り、Taskに履歴を持たせる。workspace、worktree、receipt、log、repo、run directory、supervisor token、last errorの参照列をtask_runsに置き、claim時点ではnullにする。`result_commit`は検証で確認したcommitで、着地後は`main`に積んだsquash commitに置き換わる（rebase後のrun headは`refs/dagq/runs/<run-id>`と`run_integrated`イベントの`source_commit`が持つ）。`last_error`は検証の拒否理由、`needs_session`の理由、cleanup失敗、またはruntime errorを持ち、着地で消える。`workspace_closed_at`（0003）はcmuxがcloseを確認した時刻で、nullの間はworkspaceを開いているものとして扱う。成果物hashは未実装。

`goals`（0008）はgoalを1行で持つ。`title`は空でなく、`description` / `acceptance` / `constraints`は既定`''`、`doc`はnull可。`verdict`は`'achieved'` | `'abandoned'` | nullで、CHECK `(closed_at IS NULL) = (verdict IS NULL)`により閉じたgoalだけがverdictを持つ。`tasks.goal_id`は`goals(id)`への外部キー（null可、index `tasks_by_goal`）、`tasks.context`は`TEXT NOT NULL DEFAULT ''`で、v7以前のtaskは移行後に`goal_id = NULL`、`context = ''`になる。`run_events`は0008で作り直し、`task_id`をnull可にして`goal_id`（`goals(id)`への外部キー、index `events_by_goal`）を足した。CHECKで`task_id`と`goal_id`の少なくとも一方が非null、`run_id`があれば`task_id`も非nullとし、`(run_id, task_id) → task_runs(id, task_id)`の複合外部キーは残す。goal単位のイベント（`goal_created`、`goal_updated`、`goal_closed`）は`goal_id`だけを持ち、`task_goal_changed`はtaskのイベントとして`task_id`を持つ。goalの操作（`add_goal` / `edit_goal` / `close_goal` / `set_goal`と`add --goal`）は他の状態変更と同じく`BEGIN IMMEDIATE`で直列化し、closeの判定（status別件数）とverdictの書き込み、set-goalのtask statusとgoalの開閉の確認を同じトランザクションで行う。

`goals.status`（0013、[ADR-0024](../adr/0024-retire-maintainer-into-jobs-and-observer.md)の決定5）は`'draft'` | `'open'`（CHECK、`NOT NULL DEFAULT 'open'`）で、v12以前のgoalは移行後すべて`open`になる。`closed_at` / `verdict`とは独立で、閉じたdraftもありうる。`goal add --draft`が`draft`で挿入し、`goal ready`（`ready_goal`）が`BEGIN IMMEDIATE`の中でdraftかつ未closeを確かめて`open`に更新し、`goal_status_changed`を書く。候補の問い合わせ`READY_QUERY`（`candidates`、`graph_input`、`claim_task`が共有する）は、`goal_id`の指す`goals`の行が`status = 'draft'`のtaskを除く。

note（ADR-0024の決定4）は表を作らず`run_events`のkind `observation`として書く（`add_note`）。`--task`は`task_id`、`--run`は`task_runs`から引いた`task_id`と`run_id`、`--goal`は`goal_id`だけを持つ行になり、payloadは`{"text", "kind", "by"}`。`notes`は`kind = 'observation'`に、`--goal`なら`goal_id`一致か所属taskの`task_id`、`--task`なら`task_id`一致を足し、`--since`があれば`id > since`を昇順に、無ければ降順に`LIMIT`件引いて古い順に並べ直す。indexは足していない（件数が小さく、goal / taskで絞るときは既存の`events_by_task` / `events_by_goal`が効く）。

## Runtime ownership

- supervisorは実行中のrunごとに`run_leases`の行（`run_id`主キー、token、PID、heartbeat）を所有する。tokenはsupervisorプロセスに1つで、2秒ごとに同じtokenの全行のheartbeatを1文で更新する。claimはrun・`task_runs.supervisor_token`・lease行を1トランザクションで作り（`claim_for_supervisor`）、所有者のないclaimed runを作らない。run状態を変える操作はそのrunのtokenと30秒以内のheartbeatを要求する。runが`awaiting_integration`または`failed`になったらlease行を削除する（`lease_released`）。runtime errorでrunを手放すとき（abandon）は`last_error`と`runtime_error`（`lease_released`）を書いてlease行だけを削除し、statusとprocessは変えない。leaseを自動で奪うのは引き継ぎ（adopt、[ADR-0012](../adr/0012-adopt-stale-lease-of-live-wrapper.md)）だけ: `running` / `validating`のrunのleaseが他のtokenでstale（pidが死んでいるかheartbeatが30秒より古い）で、wrapperが生きているか`exited_at`記録済みなら、`adopt_run`が`BEGIN IMMEDIATE`の中でstatusとstaleを再検査し、lease行の`token` / `pid` / `heartbeat_at`と`task_runs.supervisor_token`を引き継ぐsupervisorのものに更新して`run_adopted`イベント（`previous_token`、`previous_pid`、`previous_heartbeat_age_secs`、観測した`wrapper: {pid, alive, exited_at}`、新しい`token`と`pid`）を書く。旧tokenのlease行が見つからなければ（先に別のsupervisorが引き継いだ、fresh、解放済み）0行更新で何もしない。`supervisor_token`は「いまそのrunを動かしているsupervisor」で、claimしたsupervisorのtokenは`run_adopted`の`previous_token`に残る。lease行のないrun、`claimed` / `starting`、`integrating`は引き継がない。`holds_lease(run, token)`でsupervisorは各tickに自分のleaseが残っているかを確認し、失っていればそのrunに書かない。それ以外にleaseを奪う経路はない。
- `supervisors`は常駐`supervise`プロセスの登録で、runを持たないsupervisorを`status`/`doctor`から見えるようにする（leaseはrunがあるときしか存在しないため）。`supervise`は起動時、heartbeat threadを始める前に自分のtokenをキーに`pid`、`parallel`（`--parallel`、CHECKで1以上）、`started_at`を1行書く（`register_supervisor`）。heartbeatは`heartbeat(token)`が`supervisors`と`run_leases`の同じtokenの行を1トランザクションで2秒ごとに更新する。ループを抜けるとき（drain完了、`--once`の完了、provisioning失敗後のdrain、claimやGitのエラーによる終了）に行を消す（`deregister_supervisor`）。heartbeat失敗で終わるときだけは消さず（DBに書けない可能性がある）、leaseと同じくstaleになる。killされたsupervisorの行は残り、`status`/`doctor`が`stale`（PIDが死んでいるかheartbeatが30秒より古い）として報告する。`status`/`doctor`/`recover`/`integrate`はこの表を消さない。消すのは`up`と`down --force`だけ: `up`は起動前にPIDの死んだ登録行を`deregister_supervisor`で消して`pruned_supervisors`に報告し（PIDが生きている行はheartbeatが古くても残す）、`down --force`はSIGKILLした登録の行と、生きた登録が無いときはPIDの死んだ行を消す。`supervise`が`--log-dir`を開けずに起動に失敗したときは自分の行をその場で消す（launchdの再起動のたびに行が溜まらないため）。どちらも`run_leases`には触れない（[supervisor-lifecycle](supervisor-lifecycle.md#up--down)）。`up`はさらに、生きていてheartbeatが30秒以内の登録があればsupervisorを起動せず`reused`にする。`mode`と`workspace_id`（0009）はその登録のプロセスをどう起動したかで、書くのは`up`だけ: 起動したsupervisorが登録に現れた直後に`set_supervisor_mode`で`'launchd'`（workspaceなし）か`'in_cmux'`（supervisor workspaceのUUID）を書く（[ADR-0011](../adr/0011-cmux-socket-password-and-in-cmux-fallback.md)）。手で起動したsupervisorは誰も書かないので`mode`はnullのままで、`up`が代わりに名乗ることもしない。modeはプロセスの性質なので登録行と同じ寿命を持ち、行が消えれば消える（queue dirのsidecar fileにしなかったのはこのため。fileならプロセスが死んだ後も残り、独自のstale判定と後始末が要る）。`status` / `doctor`は`mode`と`workspace_id`をそのまま出し、`down`は`'in_cmux'`の登録にSIGINTを送ってそのworkspaceを閉じる。`binary_version`（0010）はその登録のプロセスが動いているdagqのversion（`CARGO_PKG_VERSION`）で、`mode`とは逆に書くのは登録するプロセス自身だけ: `register_supervisor`が`INSERT`と同じ文で入れる（どのbuildかを知っているのはそのプロセスだけなので、`up`が後から名乗ることはしない）。列より古いbinaryが書いた行はNULLで、これは「`up`自身のversionではない」に含める。`up`はliveな登録の`binary_version`が1つでも自分と違えば、その登録のsupervisorをdrainして入れ替える（[ADR-0014](../adr/0014-up-replaces-a-supervisor-of-another-binary-version.md)、[supervisor-lifecycle](supervisor-lifecycle.md#up--down)）。`status` / `doctor`は`supervisors[].binary_version`として出す（登録のないlease holderはnull）。`run_leases.token`と`supervisors.token`が対応し、`status`/`doctor`はtokenでleaseを登録に結び付ける。`integrate`プロセスは登録せずleaseだけを持つので、外部キーは張らない。
- `queue_repository`はqueueを束縛するGit common directoryを持つ。cwdから解決したqueueは`init`が記録し（`bind_repository`）、以後の全コマンドがopen直後に一致を検査する（`assert_repository`）。`--db`のqueueは最初の`supervise`が記録し、`supervise`と`integrate`が検査する。`bind_repository`は別のrepositoryへの束縛を拒否し、暗黙の付け替えは行わない。付け替えは明示的な`rebind`だけが行う（`rebind_repository`。open直後の検査を通らない唯一のコマンドで、走行中のsupervisorか着地中の`integrate`がいれば拒否し、変更を`logs/rebind.jsonl`に追記する。queue単位のeventはschemaを変えないと`run_events`に入らないので書かない。[ADR-0020](../adr/0020-rebind-queue-to-a-moved-repository.md)）。
- `run_processes`は`(run_id, role)`を主キーとし、wrapperとagentの登録は1回限りにする。wrapperの操作は登録したPIDかつ未終了であることを要求する。
- ログ本体とreceiptはDBと同じdirの`runs/<run-id>/`のファイルに置く。provisionは`run_dir`・`worktree_path`・`receipt_path`・`log_path`をその時点の絶対pathでtask_runsへ記録するが、読み出しではこの値を使わず、開いたDBのdirの`runs/<run-id>/…`に置き換える（`run_row`が`TaskRun::relocated`を通し、配置は`RunPaths`の1か所。値がnullならnullのまま）。したがってqueueディレクトリを移しても、絶対pathが入った既存の行を含めて全コマンドが新しい場所を見る。schemaとmigrationは変えない（[ADR-0017](../adr/0017-resolve-run-paths-from-the-queue-directory.md)）。`repo_path`はrepositoryを指すので置き換えない。idle marker `idle.json`とClaudeのsettingsは`run_dir`から導出し、列は持たない。ただしreceiptの内容はruntimeが読んだ時点でイベントに写す（検証時は`validation_finished`、着地時は`integration_receipt` / `integration_failed`）ので、worktreeとrun dirが消えた後も`show`で辿れる。
- receipt受領後の終了要求は`session_idle_observed`（markerとreceiptのmtime、hookのフィールド）、`exit_requested`、`exit_request_timed_out`のイベントだけで表し、runのstatusは変えない。引き継いだsupervisorは`has_run_event(run, kind)`で`receipt_observed`・`exit_requested`・`exit_request_timed_out`の有無を読み、観測・終了要求・timeoutの記録を繰り返さない。
- receipt前のダイアログ待ちは`prompt_waiting`（`workspace_id`、`excerpt`、`screen_hash`、`prompt`）と`prompt_cleared`（`workspace_id`）のイベントだけで表し、runのstatusもschemaも変えない。引き継いだsupervisorは最後の`prompt_waiting`（その後に`prompt_cleared` / `receipt_observed`が無いもの）の`screen_hash`を読み、同じ画面を再記録しない（[supervisor-lifecycle](supervisor-lifecycle.md#ダイアログ待ちの検知)）。
- cmuxの呼び出しの失敗とtimeoutは`backend_call_failed`（`op`、`workspace_id`、`timeout_secs`、`error`（先頭300文字）、`load_avg`（1分値かnull）、`slots`、`parallel`）のイベントだけで表し、runのstatusは変えない。runのための呼び出しはそのrunの`task_id` / `run_id`を持ち、runに紐づかない呼び出しは`task_id` / `goal_id` / `run_id`のどれも持たない（0012）。`stats`が`backend_failures`として数える（[supervisor-lifecycle](supervisor-lifecycle.md#backendの呼び出しの失敗)）。
- receipt検証は`validating`かつ同じsupervisor tokenのrunだけを`awaiting_integration`または`failed`へ進める。検証コマンドの結果は`verification_command`イベント、判定とreceiptの内容は`validation_finished`イベントに置き、専用テーブルは持たない。
- workspaceのcloseは`awaiting_integration`かつ同じtokenで`workspace_closed_at`がnullのrunだけに記録でき、`workspace_closed`イベントと一緒に一度だけ書く。失敗は`cleanup_failed`イベントと`last_error`に残し、列はnullのままにする。イベントから導出せず列に持つのは、`doctor`/`recover`や統合確認が閉じていないworkspaceを1クエリで拾えるようにし、失敗後の再試行で`cleanup_failed`と`workspace_closed`の順序を追わずに済ませるため。
- `recover`だけがtokenなしで未完了runを`interrupted`（`integrating`なら`awaiting_integration`）にし、そのrunのlease行を削除する。同じトランザクションでそのrunのheartbeatが30秒以内のleaseがないことと`run_processes`の行数が事前確認と一致することを再検査する。確認したプロセス・leaseの状態は`run_recovered`イベントに残し、`run_processes`と他のrunのleaseは変更しない。
- 着地（`integrate`）は`integrate`プロセスのtokenでlease行を持つ。`begin_integration`は`awaiting_integration`または`needs_session`のrunを`integrating`にし、lease行と`integration_started`を1トランザクションで書く。`integrating`のrunはqueue全体で高々1件（`one_integrating_run_per_queue`）。`integrating`から出る遷移はすべてそのtokenの新鮮なleaseを要求し、lease行を消して`lease_released`を書く: `finish_integration`（`integrated`、`result_commit`を着地commit、`last_error`をnull、Taskを`completed`、`run_integrated`と`task_status_changed`）、`defer_integration`（`needs_session`、`last_error`に理由、`integration_deferred`）、`fail_integration`（`failed`、`integration_failed`。payloadの`receipt`に`failed`を報告したreceiptのJSON全体）、`abort_integration`（着地開始時のstatusへ戻す、`integration_error`）。Gitの操作はトランザクションの外で行い、mainを進める前のerrorではDBを元のstatusに戻す。着地後のworktree削除の失敗は`cleanup_failed`イベントと`last_error`だけに残す（`record_cleanup_failure`）。
- 着地で読んだreceiptは`Receipt::check`を通った直後に`integration_receipt`イベント（`main`、receiptの`commit`、`receipt`にJSON全体。`follow_ups`を含む）で記録する（`record_runtime_event`、lease外）。着地に至らなかった試行でも残るので、`needs_session`をセッションが解消した後のreceipt（解消後のcommit、evidence、`summary`、`follow_ups`）は`validation_finished`ではなく、そのrunの最後の`integration_receipt`が持つ。専用テーブルや列は持たない。

## Transactions and constraints

- `BEGIN IMMEDIATE`で状態変更、依存グラフ検証、claimを直列化する。ロック待機は最大5秒で、タイムアウトはエラーとして呼び出し元へ返す。
- claimは候補選択、Task更新、TaskRun作成、イベント保存（supervisorからはlease行も）を一つのトランザクションにまとめる。途中エラーでは全体をrollbackする。同時claimは`BEGIN IMMEDIATE`で直列化され、別々のtaskを取る。
- キュー全体の実行枠はない（0005で`one_executing_run_per_queue`を削除）。同時実行数は`supervise --parallel`が決める。
- 部分UNIQUE index `one_unfinished_run_per_task`で、Taskごとの未完了run（`awaiting_integration`、`integrating`、`needs_session`を含む）を1件に制限する。
- 部分UNIQUE index `one_integrated_run_per_task`で、Taskごとの`integrated` runを1件に制限する。
- 部分UNIQUE index `one_integrating_run_per_queue`で、queue全体の`integrating` runを1件に制限する（統合スロット）。
- foreign key、statusのCHECK制約、依存の複合主キーを設ける。自己依存はDBとapplicationの両方で拒否し、循環はトランザクション内の再帰CTEで検証する。
- Task詳細は一つのread transactionで読むため、Task・run・イベント間のスナップショットが揃う。
- `list`は`TaskQuery`（application層。`StatusFilter`は`Open`（既定、`completed` / `canceled`以外）/ `Any`（`--all`）/ `Only`（`--status a,b`、どれか）、`goal_id`、`limit`（既定20）、`before`、`full`）を受け、`SqliteQueue::list`がWHERE（status・goal・`id <= before`をAND）を組み立ててID降順に`limit + 1`件引く。余った1件があればそのIDを`next`にしてページから落とし、なければ`next`はnull。`total`は`before`とlimitを除いたフィルタだけの`count(*)`で、ページ・依存・最新runと同じread transactionで数える。各要素（`TaskListItem`）はid/status/title/goal_id、`dependencies`（先行task IDの昇順）、`latest_run`（rowidが最大のrunの`id`と`status`、なければnull）で、`full`のときだけdescription/acceptance/verification_commands/context/created_at/updated_atを同じ階層に足す。

SQLiteの書き込みトランザクションとIMMEDIATEの挙動は[公式仕様](https://www.sqlite.org/lang_transaction.html)に従う。

## Queue location

[ADR-0006](../adr/0006-queue-per-repository.md)。queueはrepositoryごとに1つで、`src/infrastructure/location.rs`の`QueueLocation`が場所を決める。

```text
$XDG_DATA_HOME/dagq/<hash>/              XDG_DATA_HOME が未設定・空・相対 path なら $HOME/.local/share
  queue.db                               SQLite（WAL の -wal / -shm も隣に置かれる）
  repository                             束縛先の Git common directory（人向けの逆引き）
  runs/<run-id>/                         prompt、runner、worktree/、claude-settings.json、idle.json、receipt.json、ログ
  logs/                                  supervisor-<started_at>-<pid>.log（supervise --log-dir）、launchd.log
~/Library/LaunchAgents/com.dagq.<hash>.plist   up が書く supervisor の LaunchAgent（down が消す）
```

- `<hash>`はcanonicalizeしたGit common directoryのUTF-8 bytesのSHA-256のhex先頭16文字（`repository_hash`）。symlink経由やworktreeからでも同じhashになる。
- `--db PATH`は明示override。run dirは`dirname PATH`/`runs/`、log dirは`dirname PATH`/`logs/`で、規則はcwd解決と同じ（`runs_dir`）。`git_common_dir`は持たず、`locate`の`source`は`db_flag`になる。LaunchAgentのlabelはrepository queueでは`com.dagq.<hash>`、`--db` queueではDBのpathを同じ関数でhashした`com.dagq.<hash of PATH>`（PATHは正規化した絶対path。ファイルがまだ無ければ存在する親までを正規化して残りを繋ぐ。`up --db ./q.db`と`down --db /abs/q.db`が同じlabelを指すため）。
- `locate`は解決結果（`db`、`queue_dir`、`runs_dir`、`log_dir`、`label`、`launch_agent`（`$HOME/Library/LaunchAgents/<label>.plist`。存在しなくても出す）、`source`、`git_common_dir`、`db_exists`）をDBを開かずに返す。
- repositoryを移動するとhashが変わり新しいqueueに解決される。旧queueは`repository`ファイルで特定し、`--db`で開く。新しいcheckoutから`--db <旧queue.db> rebind`で束縛を付け替え（`repository`ファイルも書き換わる）、出力の`move_to`（新しいhashのqueueディレクトリ）へqueueディレクトリを丸ごと移す。先に移してからフラグ無しで`rebind`してもよい。新しいcheckoutで`init`を先に打たない（[ADR-0020](../adr/0020-rebind-queue-to-a-moved-repository.md)）。
- queueディレクトリは中身（`queue.db`とWALファイル、`runs/`、`logs/`、`repository`）をまとめて移せる。runのpathは開いた場所から解決し直し、runのworktreeのGit管理情報（repository側の`.git/worktrees/<name>/gitdir`）は`integrate`が`git worktree repair`で直す。supervisorは`down --wait`で止めてから移し、移動後・着地前に`git worktree prune`を打たない（[ADR-0017](../adr/0017-resolve-run-paths-from-the-queue-directory.md)）。

## Database setup and migrations

`dagq init`（またはrepository外から`dagq --db PATH init`）で初期化する。`init`はDBのdirを`create_dir_all`で作り、cwdから解決したqueueなら`repository`ファイルを書いて`queue_repository`を束縛する。通常の操作で存在しないDBを暗黙に作成しない。

SQLiteはrusqliteのbundled機能で同梱する。初期化でWALを有効にし、各接続でforeign_keysを有効にする。migrationは`migrations/0001_queue.sql`から順に適用し、`PRAGMA user_version`とDDL更新を同じトランザクションでcommitする。

`application_id = 0x43545131`でdagqのDBを識別する。未知の新しいschemaや他アプリのDBは書き換えずに拒否する。`init`の再実行では登録済みデータを保持する。バイナリ更新時は既存DBをopenする際にも未適用migrationを確認する。

SQLiteはCHECK制約を変更できないため、statusの追加はtableの作り直し（`CREATE ... _vN` → `INSERT ... SELECT`（rowidも複写） → `DROP` → `RENAME` → index再作成）で行う。`DROP TABLE`はforeign keyが有効だと参照元の行があるとき失敗するので、migration実行中はトランザクションの外で`foreign_keys=OFF`にし、commit前に`pragma_foreign_key_check`が0件であることを確認してからONに戻す。`0004_integration.sql`がこの形の最初の例。`0005_run_leases.sql`は`run_leases`を作り、v4の`supervisor_leases`の行を`supervisor_token`が一致する実行中runへ移してから`supervisor_leases`と`one_executing_run_per_queue`を落とす（tableの作り直しは不要）。`0006_merge_queue.sql`は0004と同じ手順で`task_runs`を作り直して`integrating`と`needs_session`をCHECKに加え、`one_unfinished_run_per_task`を2状態込みで作り直し、`one_integrating_run_per_queue`を足す。`run_leases`の外部キーは`task_runs`を名前で参照しているので作り直し後もそのまま有効で、`foreign_key_check`で確認する。`0007_supervisors.sql`は`supervisors`を作るだけで、既存の行には触れない。`0009_supervisor_mode.sql`は`supervisors`に`mode`（CHECK `mode IN ('launchd','in_cmux')`、既定null）と`workspace_id`を`ALTER TABLE ADD COLUMN`で足す（既定がnullなので作り直し不要）。migration中に動いているsupervisorの行は`mode`がnullになり、次の`up`まで手で起動したものと同じ扱いになる。v6のsupervisorが動いている最中にmigrationが走っても、そのsupervisorは登録を持たないままleaseだけで`status`/`doctor`に並ぶ。`0010_supervisor_binary_version.sql`は`supervisors`に`binary_version`を`ALTER TABLE ADD COLUMN`で足す（既定がnullなので作り直し不要。CHECKも置かない——versionはbinaryが名乗る文字列で、runtimeが列挙できない）。migration中に動いているsupervisorの行は`binary_version`がnullになり、次の`up`が「自分のversionではない」として入れ替える（[ADR-0014](../adr/0014-up-replaces-a-supervisor-of-another-binary-version.md)）。`0008_goals.sql`は`goals`を作り、`tasks`に`goal_id`と`context`を`ALTER TABLE ADD COLUMN`で足し（既定がnull / `''`なので作り直し不要）、`run_events`を0004と同じ手順で作り直す（`id`（AUTOINCREMENT）も複写するので、イベントIDと順序、`sqlite_sequence`の続きが保たれる）。`0012_queue_events.sql`は`run_events`を0008と同じ手順で作り直し、CHECKを`task_id IS NOT NULL OR goal_id IS NOT NULL OR kind = 'backend_call_failed'`に緩める（`run_id`があれば`task_id`も非null、の CHECK と複合外部キーはそのまま）。runに紐づかないcmuxの呼び出し（`up`のworkspace、queueのworkspace group、`down`のclose）の失敗をqueue単位のイベントとして残すためで、ほかのkindの規則は変えない。`0013_goal_draft.sql`は`goals`に`status`を`ALTER TABLE ADD COLUMN`で足す（既定`'open'`があるので作り直し不要。CHECKは列の定義に置く）。`SqliteQueue::SCHEMA_VERSION`（`MIGRATIONS`の長さ）が最新の`user_version`で、testはこの定数と比較する。

## Planned runtime persistence

receipt検証結果はイベントとtask_runsの列で足りたため、`run_artifacts`テーブルは追加しなかった。成果物hashが必要になった時点で検討する。一定時間heartbeatが更新されないrunは自動再実行せず、`doctor`で確認して`recover`で明示的に閉じる。`interrupted`は最初からCHECK制約に含まれていたため、復旧のためのmigrationは不要だった。leaseの列を`task_runs`に足す案は、解放済みをnullで表すことになり実行の記録と揮発する所有権が混ざるため採らなかった（[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)）。

旧Python版の状態を読み込む移行コマンドは後続の配布段階で用意し、task ID、依存、run履歴、ログpathを保持する。
