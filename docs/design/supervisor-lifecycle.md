---
id: design-supervisor-lifecycle
type: design
title: Supervisor and workspace lifecycle
status: current
created: 2026-09-21
updated: 2026-09-23
last_verified: 2026-09-23
scope: runtime
related:
  - adr-0002
  - adr-0003
  - adr-0006
  - adr-0007
  - adr-0008
  - adr-0009
  - adr-0010
  - adr-0011
  - adr-0012
  - adr-0013
  - adr-0014
  - adr-0016
  - adr-0018
  - adr-0019
  - adr-0020
  - adr-0021
  - adr-0022
  - adr-0023
  - adr-0024
  - adr-0025
  - adr-0026
  - adr-0028
  - design-persistence
  - design-provider-lifecycle
  - design-plugin-integration
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
      → worktree and branch removed; history kept at refs/dagq/runs/<run-id>
    conflict / failed re-validation → needs_session
      → the supervisor resumes the session in the worktree (up to 3 attempts); it resolves,
        reruns verification, rewrites the receipt → the supervisor lands the run whose
        integrate was called, or returns it to awaiting_integration (failed receipt → run failed)
  → dependents become candidates; the resident loop claims them from the landed main
```

## Implementation status

ステップ3で`claim`から`running`、セッション終了検知までを、ステップ4の[005](../journal/005-receipt-validation.md)でreceiptの検証と`awaiting_integration`への遷移を、[006](../journal/006-workspace-close.md)で受理後のworkspace終了を、[007](../journal/007-session-exit-request.md)でreceipt受領後の終了要求を、[009](../journal/009-doctor-recover.md)で`doctor`/`recover`を、[008](../journal/008-integration-confirm.md)で統合確認`integrate`と`completed`への遷移を`src/runtime.rs`に実装した。ステップ6の[017](../journal/017-parallel-runs.md)（[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)）でleaseをrun単位にし、`supervise`を上限付き並列の常駐ループにした。ステップ7の[018](../journal/018-merge-queue.md)（[ADR-0008](../adr/0008-merge-queue-squash-landing.md)）で`integrate`を手動mergeの確認からruntimeによる着地（rebase → 再検証 → squash）に置き換えた。[021](../journal/021-maintainer-up-down.md)で`up` / `down`（`src/lifecycle.rs`）、launchdによる常駐、`supervise --log-dir`、maintainer promptを足した。task 24（[ADR-0012](../adr/0012-adopt-stale-lease-of-live-wrapper.md)）で、supervisorが死んだ後もwrapperが生きているrunを次のsupervisorが引き継ぐ（adopt）ようにした。[ADR-0011](../adr/0011-cmux-socket-password-and-in-cmux-fallback.md)のtask 21で`up`にcmux外接続のpreflightを、task 22で`up --in-cmux`（launchdなしのfallback）と`supervisors.mode`を足した。task 30（[ADR-0014](../adr/0014-up-replaces-a-supervisor-of-another-binary-version.md)）で`supervisors.binary_version`を足し、`up`がversionの違うliveなsupervisorをdrainして入れ替えるようにした（`--no-wait`は走行中のrunがあれば入れ替えない）。task 57（[ADR-0016](../adr/0016-maintainer-notification-and-compact-output.md)）で`status`に`attention`と`cursor`を足し、`events --after`と`watch`（`src/watch.rs`、判定は`src/domain.rs`）を足した。task 75（task 58の登録し直し）で`WorkspaceBackend::notify`とcmux adapterの`cmux notify`を足し、task 87で`dagq ask`が新しいaskのときにinboxへ通知するようにした。supervisorは通知を送らない（[人への通知](#人への通知cmux-notify)）。task 70（[ADR-0019](../adr/0019-move-routine-maintainer-work-into-the-runtime.md)の決定3）で`integrate`が着地後に`main`を`origin`へpushするようにし（`--no-push`、`push_finished` / `push_skipped` / `push_failed`）、`push_failed`をattention（`push main`）にした。task 79（[ADR-0025](../adr/0025-leaseless-unfinished-run-is-a-recover-run-attention.md)）で、supervisorが手放した（leaseの無い）未完了runを`recover run`のattentionにした。task 94（[ADR-0023](../adr/0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)の決定5）でrun_eventsから時間と閾値超えを導出する`stats`（集計は`src/domain/stats.rs`）を足した。task 109でcmuxの呼び出しの失敗とtimeoutを`backend_call_failed`（load averageとslot数つき）として記録し、`stats`に`backend_failures`とalert `backend_failures`を足した（[backendの呼び出しの失敗](#backendの呼び出しの失敗)）。task 71（[ADR-0019](../adr/0019-move-routine-maintainer-work-into-the-runtime.md)の決定1）で、supervisorが`needs_session`のrunをresumeして解消させ、`integrate`が呼ばれていたrunを着地させるようにした（[`needs_session`](#needs_session)）。task 99（[ADR-0024](../adr/0024-retire-maintainer-into-jobs-and-observer.md)の決定4）で、headlessのClaudeで動くobserver job（`observe`、`src/observer.rs`）と、supervisorのtimerによる起動（`--observe-interval` / `--observe-daily`）、askの`kind: blocked`を足した（[Observer](#observer)）。

## Roles

- **supervisor**: runtimeの`supervise`プロセス。taskをclaimし、runごとにworktreeとworkspaceを作って監視し、receiptを検証する。`up`がlaunchdのLaunchAgentとして常駐させるか（既定）、`--in-cmux`なら`[<repo>]supervisor` workspaceの中で動かす。
- **maintainer**: 常駐のClaude Code session（旧称SV / operator）。登録・監視・レビュー・着地を行う。`up`が`[<repo>]maintainer`のcmux workspaceで、runtimeが生成した初期prompt付きで起動する。CLIの使い方はpluginの`dagq-maintain` skillが持つ。
- **worker**: run session。runごとのcmux workspace `[<repo>]worker#<task-id> - <task title>`（descriptionは`dagq role=worker queue=<queue hash> run=<run-id> task=<id>`）で動くClaude session。`needs_session`のrunをresumeするworkspaceも同じtitleで、descriptionは`run <run-id> resume`。
- **planner**: 人と対話してgoal / taskを登録するsession。`up`がmaintainerと同じ手順で`[<repo>]planner`のworkspace（`DAGQ_ROLE=planner`）に、`planner_prompt`付きで起動する（[Session prompts](#session-prompts)）。
- **inbox**: 人がqueueのask（maintainerとworkerの質問）に答えるsession。`up`がmaintainerと同じ手順で`[<repo>]inbox`のworkspace（`DAGQ_ROLE=inbox`）に、`inbox_prompt`付きで起動する（[Session prompts](#session-prompts)）。
- **observer**: supervisorのtimerが起動するheadlessのjob（`DAGQ_ROLE=observer`、workspaceは持たない）。stats・note・openなask・graphを読み、note・`blocked`のask・draftのgoalだけを書く（[Observer](#observer)）。

runtimeの中で人が打つ`/exit`や復旧を指す語はすべてmaintainerに寄せた（`src/`に`operator`は残らない）。

## `up` / `down`

`dagq up [--parallel N] [--in-cmux] [--plugin-dir PATH] [--repo PATH] [--cmux EXE] [--claude EXE]`はqueueのruntimeをcold startする1コマンドで、`src/lifecycle.rs`の`up`が行う。冪等で、続けて2回叩けば2回目は全部`reused` / `skipped`になる。外部（launchctl、cmux、PIDの生存とsignal）は`LaunchAgent`、`WorkspaceBackend`、`ProcessControl`のtrait越しに呼び、`tests/lifecycle.rs`はfakeで判定を、`tests/e2e.rs`は実launchdと実cmuxで`up → status → down --wait`を両方のmodeで確認する（in-cmux modeのe2eはsocket passwordを要らないので、`cmuxOnly`のままでも通る）。ただしlaunchd modeのe2eは2026-09-23のユーザー判断で一時的に既定の`--ignored`実行から外している: launchd modeで運用しているプロジェクトが今は無く、socket passwordの無い環境ではpreflightで必ず止まるため。`DAGQ_E2E_LAUNCHD=1`を付けたときだけ本文を実行し、付けなければ理由を出してそのままpassする（testと補助関数は残してあり、戻すときはこの条件を外す）。launchd mode自体と`tests/lifecycle.rs`のfakeによるtestはそのまま。

1. **preflight**: queueが`init`済み（DBが存在する。`--db`がなければcwdのrepositoryから解決）、repositoryのroot、cmux（`ping`）、Claude（`--version`）、Claude Codeがrepository rootを信頼済みであること（`$CLAUDE_CONFIG_DIR/.claude.json`、未設定なら`~/.claude.json`の`projects[<root>].hasTrustDialogAccepted`が`true`。run worktreeの信頼はrepository rootから決まるので、未信頼のまま流すと全runがtrust dialogで止まる。未信頼・configが無い・HOMEが無いときは、rootで`claude`を一度起動して承認する案内のerrorで、何も起動せずに止まる。configを書き換えて信頼を代行することはしない。[provider-lifecycle](provider-lifecycle.md#trust-prompt)）。`--plugin-dir`は絶対pathに正規化する。
2. **stale登録の削除**: `supervisors`表のうちPIDが死んでいる行を`deregister_supervisor`で消し、消したtokenとpidを結果の`pruned_supervisors`に出す。`run_leases`は触らない（そのrunの復旧は`doctor` / `recover`の仕事）。PIDが生きていてheartbeatが30秒より古い登録（hang）は消さず、reuseもしない。
3. **supervisor**: 生きていてheartbeatが新しい登録があり、その`binary_version`が全部`up`自身のversion（`dagq::VERSION` = `CARGO_PKG_VERSION`）と同じなら`{"outcome":"reused","mode":…,"version":…,"pid":…}`で、plistにもlaunchctlにもcmuxにも触らない（下記のcmux外接続のpreflightもしない。`mode`はその登録に記録されているものをそのまま返し、手で起動したsupervisorはnullのまま）。1つでもversionが違えば**入れ替える**（下記）。liveな登録が無ければ`--in-cmux`の有無でmodeが決まる。
   - **launchd mode（既定）**: まずcmux外接続のpreflight（下記）を通し、それからLaunchAgentを書いて起動し、登録が現れるまで（30秒）待って`{"outcome":"started","mode":"launchd","pid":…,"workspace_id":null,"plist":…}`。
   - **in-cmux mode（`--in-cmux`）**: launchdには一切触らず（plistを書かず、launchctlも呼ばない）、cmux外接続のpreflightもしない（supervisorはcmuxのterminalの子になるので、socket passwordが要らないのがこのmodeの目的）。`session_workspaces`の`supervisor`行に記録したworkspaceが`cmux --json --id-format uuids workspace list`にまだ居れば、そのIDと`cmux workspace close <id>`を挙げたerrorで止まる（titleは見ない。居なければ行を消して進む。cmuxはcommandが終わってもworkspaceを閉じないので、crashしたsupervisorのworkspaceや、生きているがheartbeatの止まったsupervisor——`up`はreuseもkillもしない——のworkspaceが残る。どちらも人が中を見て閉じる。[ADR-0011](../adr/0011-cmux-socket-password-and-in-cmux-fallback.md)のConsequences）。開いていなければ`cmux workspace create --name "[<repo>]supervisor" --description "dagq role=supervisor queue=<queue hash>" --env DAGQ_ROLE=supervisor --env DAGQ_QUEUE=<db> [--group <queueのgroup>] --command "<up自身の絶対path> --db <db> supervise --parallel N --log-dir <queue dir>/logs --cmux <resolved> --claude <resolved>" --focus false --cwd <repository root>`で作り（`shell_join`で1引数ずつquoteする。argvはlaunchd modeの`ProgramArguments`と同じ`supervise_arguments`。tagsは下記[Naming](#naming)）、`identify`でUUIDを得て`session_workspaces`の`supervisor`行に書き（登録を待つ前に書くので、登録が現れなかったworkspaceも次の`up`が見つける）、登録が現れるまで（30秒）待って`{"outcome":"started","mode":"in_cmux","pid":…,"workspace_id":…,"name":"[<repo>]supervisor","plist":null}`。登録が現れなければerrorで止まり、文面はそのworkspace IDを挙げる（画面を読んでから閉じる）。launchdの`KeepAlive`に相当するものはないので、このmodeのsupervisorが死んでも何も再起動しない。人が`up --in-cmux`を打ち直す。
   - **待つ相手**: `wait_for_registration`は`up`が起動したsupervisorの登録だけを受け取る。2で見た時点で既に表にあったtoken（PIDが生きていてheartbeatの古い「生きているが黙っている」登録を含む。`up`はこれをpruneもreuseもしない）は除外する。除外しないと、待っている間にその古いsupervisorがheartbeatを再開したときにそれを自分が起動したものと取り違え、modeと新しいworkspace IDを別プロセスの行に書いてしまう（`supervisors`は`started_at`順なので古い行が先に出る）。その後の`down`は古いsupervisorにSIGINTを送りながら、新しいsupervisorがまだ動いているworkspaceを閉じることになる。
   - **modeの記録**: 起動したsupervisorが登録に現れた直後に、`up`が`supervisors`の行へ`mode`（`launchd` / `in_cmux`）と、in-cmux modeなら`workspace_id`を書く（`set_supervisor_mode`。schema v9）。書くのは`up`だけで、`supervise`自身は書かない。modeはプロセスの性質なので登録行と寿命を共にし、supervisorがgracefulに終われば行ごと消える（queue dirのsidecar fileにしなかった理由は[persistence](persistence.md)のRuntime ownership）。`status` / `doctor`は`supervisors[].mode`と`workspace_id`として出し、`down`はこれを見てSIGTERM（launchd mode / 手起動）とSIGINT＋workspace close（in-cmux mode）を使い分ける。
   - **versionの違うsupervisorの入れ替え**（[ADR-0014](../adr/0014-up-replaces-a-supervisor-of-another-binary-version.md)）: 登録の`binary_version`は`supervise`プロセス自身が`register_supervisor`で書いた自分のversionで、列（schema v10）より古いbinaryの行はnull。`up`はliveな登録のどれか1つでも自分のversionと違えば（nullを含む）、live全部をdrainしてから1つ起動し直す。停止は`down --wait`と同じ順序: LaunchAgentを必ず外し（bootoutがSIGTERMを運び、同時に`KeepAlive`が古いbinaryを即座に立て直すのも止める）、`mode`が`in_cmux`の登録にはSIGINT、launchdがsignalしなかったプロセス（手起動、またはagentのPIDでないもの）にはSIGTERMを送り、その登録が全部消えるかPIDが死ぬまで`poll`ごとに待ち、`in_cmux`のworkspaceを閉じる（`down`と同じ`close_supervisor_workspaces`を`SeenThrough`で呼ぶ。閉じておかないと記録したworkspaceが開いたままで、新しいin-cmux supervisorが起動を拒む）。それから通常の起動経路に入る（起動し直すmodeは入れ替えられる側のmodeではなく、その`up`が指定されたmode）。launchd modeで起動し直すときのcmux外接続のpreflightは、drainより**前**に1回だけ通す（`prove_detached_cmux`）: 動いているsupervisorをdrainした後で新しいsupervisorが起動できないと分かるのでは、queueを serve するものが何も無くなる。拒まれたら何もsignalせず、plistも触らず、いつもの文面（`DETACHED_CMUX_HINT`）で止まる。待つ相手の除外集合はdrainの後に取り直すので、生き残った「生きているが黙っている」登録を自分の起動したものと取り違えない。結果は`{"outcome":"restarted","version":…,"previous_version":…,"replaced":[{"token","pid","mode","workspace_id","version"}],"supervisor_workspaces":[…]}`に、起動したsupervisorの`mode` / `pid` / `token` / `workspace_id` / `plist` / `log_dir`が並ぶ。drainに上限は置かない（runはClaude sessionなので待ち時間はrunの長さそのもの）。`up --no-wait`は待たないための逃げ道で、`active_runs`（`claimed` / `starting` / `running` / `validating` / `integrating`）が1件でもあれば件数とrun idを挙げたerrorで止まる。判定はsignalもuninstallも何もする前に行うので、止まったときの状態は`up`を打つ前と同じ（PIDの死んだ登録のprune（2）だけは済んでいる）。走行中のrunが無ければそのまま入れ替えるが、`--no-wait`のときはdrainの待ちにも上限（`startup_timeout`、既定30秒）が付く: runが無くても止まらないsupervisorはありうる（loopがcmuxやgitでhangしていてもheartbeat threadは別なので登録は新しいまま、判定とsignalの間にrunがclaimされることもある）ため、上限を超えたら残っているtokenとpidを挙げたerrorで止める（停止は既に頼んであり、agentも外れているので、`status`から消えたら`up`を打ち直す）。
   - **入れ替えの対象はliveな登録だけ**: PIDが生きていてheartbeatの止まった「生きているが黙っている」supervisorは、従来どおりreuseもpruneもkillもしないので、それが古いbinaryでも入れ替えられず、`up`はその隣に新しいversionのsupervisorを立てる。`status`がstaleとして報告するので、人が`down --force`で止めてから`up`をやり直す（[ADR-0014](../adr/0014-up-replaces-a-supervisor-of-another-binary-version.md)のConsequences）。
   - **起動できないと分かる条件はdrainより前に確かめる**: launchd modeならcmux外接続のpreflight（上記）、in-cmux modeなら`session_workspaces`の`supervisor`行のworkspaceがまだ開いているかどうか（`ensure_supervisor_workspace_free`）。開いているworkspaceがこの入れ替えで閉じる登録（`live`の`in_cmux`の`workspace_id`）のものでなければ、そのIDと`cmux workspace close <id>`を挙げてdrainの前に止まる。`up`はpruneのときworkspaceを閉じないし、cmuxはcommandが終わってもworkspaceを閉じないので、登録の無いcrash済みsupervisorのworkspaceが開いたまま記録に残っていることがある。
   - **drainがPIDの死で終わったとき**: 登録が消えるのではなくPIDが死んで待ちが終わることがある（launchdの`ExitTimeOut`によるSIGKILL、heartbeat失敗であえて行を残す経路）。その登録行は`down --force`と同じようにここで消す。残すと、直後に閉じたworkspaceを指す行が生き残り、次の`down`が同じcloseをやり直して`close_failed`を報告する。
   - cmux外接続のpreflight: launchdが起動するsupervisorはcmuxのterminalの子プロセスではなく、cmuxはそういうプロセスをsocket passwordでしか受け入れない。`up`はplistを書く前に`WorkspaceBackend::preflight_detached(SupervisorEnvironment)`で、plistが持たせるのと同じ環境（`SupervisorEnvironment`: `PATH`と、`up`を叩いたshellが`CMUX_SOCKET_PASSWORD`をexportしていればそれ）で`cmux ping`を実行する。その際、`up`自身が今いるcmux sessionから継承した`CMUX_*`の環境変数（`CMUX_SOCKET_CAPABILITY`、`CMUX_WORKSPACE_ID`、`CMUX_SOCKET_PATH`など）はすべて外す（`adapters::detach`）。継承した`CMUX_SOCKET_PASSWORD`も一度外し、shellがexportしていた値だけを載せ直す。環境を外すだけでは足りない: cmux 0.64.25は接続元をプロセスの系譜で判定していて、cmuxのterminalの子孫なら環境変数がどうであれ通し、launchd配下なら拒む（`CMUX_*`を全部外した`cmux ping`はcmux内の子プロセスとしてはPONGを返し、同じ環境でlaunchdから起動したsupervisorは拒まれた。実機確認）。そこで`Cmux::preflight_detached_within`はpingをcmuxのプロセスツリーの外で走らせる: 外側の`/bin/sh`（`setsid`で`up`のsessionと端末からも切り離す）が内側の`/bin/sh`をbackgroundで起動して即終了し、launchd（pid 1）が内側を引き取る。内側は外側のpidが消えるまで（`kill -0`）待ってから（cmuxが系譜を見る時点で親がlaunchdになっているように）、自分のpidを`pid=N`として出力し、`exec cmux ping`になる。runtimeはpid行の後の出力がPONGであれば通し、そうでなければstderr（cmuxの拒否文）を`DetachedRefusal`のerrorにする。pingを起動できない・pidが読めない・期限内に終わらない場合は`DetachedRefusal`ではない普通のerrorで、`up`はこれをpasswordの案内にはつなげず「cmux could not be asked …」として止める。cmuxのCLIは`CMUX_SOCKET_PATH`なしだとsocketを自分で探し、0〜11秒かかった（見つかると`last-socket-path`に覚えて次は即答）ので、期限は60秒（`DETACHED_PING_TIMEOUT`）で、過ぎたらそのpidにSIGKILLを送る。この孤児のpingが実機（cmux 0.64.25、`cmuxOnly`）で拒まれることは`tests/e2e.rs`のlaunchdの`up`/`down`で確認した（`up`がplistを書く前に上記の文面で止まる。今はこのtestは`DAGQ_E2E_LAUNCHD=1`のときだけ走る）。`tests/lifecycle.rs`はstubのcmuxで、届いた環境に`CMUX_SOCKET_PASSWORD`以外の`CMUX_*`がないこと、`PATH`がplistのものであること、stubの親がpid 1であること、拒否・変な応答（`DetachedRefusal`）・hang（そうでないerror、pidはkillされる）を確認する。cmuxが拒めば`up`はerrorで止まり、plistは書かず、launchctlも呼ばず、maintainer workspaceも作らない。errorの文面（`lifecycle::DETACHED_CMUX_HINT`）は拒否の事実と3つの対処（cmuxのSettingsにsocket passwordを保存する。cmuxのCLIはそれを自分で使う / `up`を叩くshellで`CMUX_SOCKET_PASSWORD`をexportする / `up --in-cmux`を使う。自動再起動がないことも書く）を載せ、末尾にcmuxのerrorをそのまま付ける。
   - plist: `~/Library/LaunchAgents/com.dagq.<queue hash>.plist`。`Label`は同名、`ProgramArguments`は`[up自身の絶対path, --db <db>, supervise, --parallel N, --log-dir <queue dir>/logs, --cmux <resolved>, --claude <resolved>]`（cmuxとclaudeは`up`がpreflightした実行ファイルの絶対path。launchdのPATHに頼らないため）、`WorkingDirectory`はrepository root、`EnvironmentVariables`は`PATH`（`up`を叩いたshellのPATH）と、そのshellが`CMUX_SOCKET_PASSWORD`をexportしていたとき（空でないとき）だけ`CMUX_SOCKET_PASSWORD`（Settingsに保存したpasswordは読まないし書かない）、`KeepAlive` true、`RunAtLoad` true、`StandardOutPath` / `StandardErrorPath`は`<queue dir>/logs/launchd.log`、`ExitTimeOut` 86400（launchdの既定20秒ではdrainが待てない）。生成は`src/infrastructure/launchd.rs`の`LaunchAgentSpec::xml`。passwordを持ちうるので、plistは0600で書く。
   - 起動: `launchctl bootout gui/<uid>/<label>`（未loadなら無視）の後に`launchctl bootstrap gui/<uid> <plist>`。既にbootstrap済みでも定義を差し替えるためbootoutしてからbootstrapし直す。bootoutは即返り、serviceは旧プロセスが終わるまで`launchctl print`に残り、その間のbootstrapはexit 5で失敗する（実機確認）ので、`print`が消えるまで0.5秒ごとに待つ。60秒で消えなければ`launchctl kill SIGKILL`を送り、さらに60秒待って諦める（hangしたsupervisorがlabelを`ExitTimeOut`の間占有しないため）。
   - 登録が30秒以内に現れなければerrorで止め、agentはそのまま残す（`launchd.log`を見る）。preflightに通らない環境ではKeepAliveで再起動を繰り返すので、`down`で外す。
4. **maintainer / inbox / planner workspace**: この順に同じ手順（`lifecycle`の`Sessions::open`）で開く。以下はmaintainerで書き、inboxとplannerはroleとtitle（`[<repo>]inbox` / `[<repo>]planner`）とprompt（`inbox_prompt` / `planner_prompt`）だけが違う。maintainerは`up`自身の環境に`DAGQ_ROLE=maintainer`があり`DAGQ_QUEUE`が同じqueue DB（正規化して比較）なら`skipped`（maintainer sessionのskillから`up`を呼んでも二重にならない。cmuxには問い合わせない）。そうでなければ`session_workspaces`の`maintainer`行のUUIDが`cmux --json --id-format uuids workspace list`に居れば`reused`（titleは見ないので、人がrenameしても、同じtitleのworkspaceを別に開いても判定は変わらない）。行が無いか、UUIDがlistに居なければ（閉じられた、cmuxが再起動した）行を消して`cmux workspace create --name "[<repo>]maintainer" --description "dagq role=maintainer queue=<queue hash>" --env DAGQ_ROLE=maintainer --env DAGQ_QUEUE=<db> [--group <queueのgroup>] --command "<claude> [--plugin-dir PATH] -- '<maintainer prompt>'" --focus false --cwd <repository root>`で作り、`identify`でUUIDを得て`maintainer`行に書き`created`。`DAGQ_ROLE` / `DAGQ_QUEUE`はcommandの前置きではなくworkspaceの`--env`なので、そのworkspaceで`claude`を打ち直しても引き継がれる。引数は`shell_join`で個別にquoteする（promptの改行はquoteの中に収まり、cmuxがログインシェルに打ち込んでも1コマンドになる。[010](../journal/010-failure-path-smoke.md)）。inbox / plannerのsessionの中から`up`を呼べば、そのroleだけが`skipped`で、他の2つは開くかreuseする（maintainer sessionの中の`up`もinboxとplannerを開く）。
5. **結果**: `{"supervisor": {"outcome": "started"|"reused"|"restarted", "mode": "launchd"|"in_cmux"|null, "version", "pid", "token", "workspace_id", "plist", "log_dir"}（in-cmux modeで起動したときは`name`も、`restarted`のときは`previous_version` / `replaced` / `supervisor_workspaces`も）, "maintainer": {"outcome": "created"|"reused"|"skipped", "workspace_id", "name"}, "inbox": {…同じ形}, "planner": {…同じ形}, "pruned_supervisors": [{"token","pid"}], "warnings": [<workspace groupを作れなかった理由>], "doctor": {"unfinished_runs": [{"run_id","task_id","status","lease_stale"}], "awaiting_integration": [{"run_id","task_id","last_error"}], "needs_session": [...]}}`。`lease_stale`はleaseのPIDが死んでいるかheartbeatが30秒より古いとき`true`、leaseがなければnull。

前提: launchdが起動したsupervisorはcmuxのterminalの外で動くので、cmuxのsocketがcmux外のプロセスからの接続を受け付ける必要がある。開発環境のcmux 0.64.25は既定では「アクセスが拒否されました。cmux内で起動されたプロセスのみ接続できます」で`cmux ping`を拒み、supervisorはpreflightで落ちて登録に現れなかった（`launchd.log`にそのerrorが残り、`up`は30秒でerrorになり、KeepAliveで再起動が続いた。journal 021）。`cmux --help`のSocket Authはpasswordによる認証（`--password`、`CMUX_SOCKET_PASSWORD`、Settingsに保存したpassword）を載せている。cmuxは接続元をプロセスの系譜で判定するので（上記）、`up`はplistを書く前にcmuxのプロセスツリーの外からのpreflightを通し、拒まれたらplistを書かずに止まる。ユーザーの用意はcmuxのSettingsにsocket passwordを保存する（`~/.config/cmux/cmux.json`の`automation.socketControlMode: "password"`と`automation.socketPassword`。既定は`cmuxOnly`。推奨。plistに何も残らない）か、`up`を叩くshellで`CMUX_SOCKET_PASSWORD`をexportする（そのときだけplistに書かれる）かのどちらか。どちらもなければ`up --in-cmux`を使う（launchdを使わず、supervisorをcmux workspaceの中で動かす。自動再起動はない）。cmuxのterminalで`supervise`を手で起動して`up`にreuseさせてもよい。

前提とfallbackは[ADR-0011](../adr/0011-cmux-socket-password-and-in-cmux-fallback.md)で決めた: launchd modeはcmuxのsocket password（Settings保存、または`up`を打ったshellがexportした`CMUX_SOCKET_PASSWORD`）を前提にして`up`がcmux外からの`ping`をpreflightで確かめ（task 21）、`up --in-cmux`がlaunchdなし・自動再起動なしで`[<repo>]supervisor` workspaceにsupervisorを起動する（task 22）。どちらも実装済み。

`dagq down [--wait] [--force] [--cmux EXE]`はsupervisorを止める。どちらのmodeで起動したものも、手で起動したものも同じ1コマンドで止まる。まずPIDの生死で登録を分け、次にLaunchAgentを必ず外す（`launchctl print`でagentの有無とPIDを読み、`launchctl bootout gui/<uid>/<label>`してplistも消す。`RunAtLoad`のため残すと次のloginで復活する。in-cmux modeに切り替えたqueueに前のlaunchd modeのagentが残っていても、これで外れる）。PIDの生きている登録が1件もなければ`{"outcome":"not_running","launch_agent_unloaded":…}`（`--force`ならPIDの死んだ登録行も消して`pruned_supervisors`に出す）。あれば登録ごとにsignalを選ぶ: `mode` が`in_cmux`ならSIGINT（cmuxのterminalでCtrl-Cを押したのと同じ。runtimeはSIGTERMと同じくdrainに入る。このmodeにはsignalを届けてくれるservice managerがない）、そうでなければ従来どおりSIGTERM——ただしbootoutでlaunchdがagentのプロセスにSIGTERMを届けるので、agentのPIDには送らない（runtimeは1回目のsignalでdispositionを既定に戻すので、2回目は即死になる）。agentがloadされているのにPIDが読めないときは誰にも送らない。既定は`{"outcome":"draining","pid":…,"pids":[…]}`で即返り、`--wait`はその登録が消えるかPIDが死ぬまで2秒ごとに待って`stopped`、`--force`はSIGKILLを送って登録行を消し`killed`（leaseは30秒でstaleになり、runは`doctor` / `recover`で扱う）。

in-cmux modeのsupervisor workspaceは`down`が閉じる。閉じる相手は登録の`workspace_id`で決め、titleは見ない（閉じたworkspaceが`session_workspaces`の`supervisor`行と一致すれば行も消す）。判定はどの経路でも同じで、`supervisors`の全登録（`not_running`のときの死んだ登録も含む）が対象になる。`down`がその停止を見届けたとき——`--wait`はdrainの完了を待った後、`--force`はkillした後、`not_running`は最初から誰も生きていない——は無条件に閉じる。PIDの生死は見ない: `kill(2)`は対象が回収される前に返るのでSIGKILLの直後の`kill(pid,0)`はまだ成功し（実測20/20）、`--wait`もsupervisorが登録を消してから終了するので戻った時点ではまだPIDが見えている。ここで生死を条件にすると、止めたはずのworkspaceが開いたまま`left_open`として報告される。既定の`down`だけは別で、supervisorはまだdrain中かもしれない——cmuxのworkspaceを閉じるとその中のsupervisorも終わってdrainが途切れる——ので、`down`が何もsignalする前に読んだ生死だけを見て、既に終わっていたものだけを閉じる。結果の`supervisor_workspaces`は1件ずつ`{"workspace_id","outcome":"closed"}`、`{"workspace_id","outcome":"left_open","reason":"supervisor pid N is still draining; `down --wait` closes it"}`、`{"workspace_id","outcome":"close_failed","reason":…}`のいずれか。cmuxがcloseを拒んでも`down`はerrorにしない（止めるという仕事はもう終わっている）。既定の`down`でdrain中に残ったworkspaceは、人が閉じるか`down --wait`を打ち直す（閉じないまま`up --in-cmux`を打つと、上記のとおり止まる）。`--force`は生きた登録に加えて死んだ登録も消す。閉じたworkspaceを指す行が残ると、次の`down`が同じcloseをやり直して`close_failed`を報告するため。`--cmux`はこのcloseに使う実行ファイルで、解決できなくても`down`自体は進む（閉じる相手がいなければ使わない）。maintainer、inbox、plannerのworkspaceは閉じない（`session_workspaces`の行も残り、次の`up`がreuseする）。

### Logs

`supervise --log-dir DIR`はDIRを作り、`supervisor-<started_at unix>-<pid>.log`に起動時のtoken / pid / parallel / db / repositoryと、従来stderrに出していた進行メッセージ（claim、workspace、receipt受領、終了要求、run終了、abandon、rejection）と最後の結果JSON（またはerror）を`[unix time] message`の形で追記する。stderrにも従来どおり出す（`SupervisorLog`）。`up`が作るagentは`--log-dir <queue dir>/logs`で起動し、launchdが拾うstdout / stderrは同じdirの`launchd.log`に溜まる（起動ごとのファイルはruntimeが分け、`launchd.log`は分けない。ローテーションはしない）。`locate`は`log_dir`、`label`、`launch_agent`（plistのpath。存在しなくても出す）を返す。

### Naming

cmux workspaceの名前は複数repositoryで同じcmuxを使うためrepository名を含み、`[<repo>]<role>`の形をとる（[ADR-0028](../adr/0028-workspace-titles-are-repo-and-role.md)。[ADR-0018](../adr/0018-run-workspace-named-after-the-task.md)と[ADR-0021](../adr/0021-maintainer-and-supervisor-workspace-names-follow-the-run-style.md)の`dagq`入りの書式を上書き）。`<repo>`はrepository rootのbasename（basenameが空ならpath自体）で、`]`の直後に空白を入れない。名前の組み立ては`infrastructure::adapters`の純粋関数:

- worker: `[<repo>]worker#<task-id> - <task title>`（`run_workspace_name`。`<repo>`はrunの`repo_path`のbasename、titleはtaskのtitleを切り詰めずにそのまま）
- resume: `needs_session`のrunを`claude --resume <run-id>`で開き直すworkspaceは、workerと同じ`run_workspace_name`の名前でdescriptionを`run <run-id> resume`にする（supervisorの自動resumeが`create_resume`で開く。[`needs_session`](#needs_session)。maintainerは開かない）
- maintainer: `[<repo>]maintainer`（`maintainer_workspace_name`）
- supervisor（in-cmux mode）: `[<repo>]supervisor`（`supervisor_workspace_name`。[ADR-0011](../adr/0011-cmux-socket-password-and-in-cmux-fallback.md)の決定3）
- planner: `[<repo>]planner`（`planner_workspace_name`）、inbox: `[<repo>]inbox`（`inbox_workspace_name`）。`up`が開く

`DAGQ_ROLE`の値は`lifecycle`の`MAINTAINER_ROLE` / `WORKER_ROLE` / `PLANNER_ROLE` / `INBOX_ROLE`（`SessionRole`の文字列）。

titleは表示専用で、runtimeはどのworkspaceもtitleで探さない（[ADR-0026](../adr/0026-identify-workspaces-by-uuid-env-and-queue-group.md)）。識別の正はqueue DBのUUID: runは`runs.workspace_id`、in-cmux supervisorの登録は`supervisors.workspace_id`、maintainer・inbox・plannerとin-cmux supervisorのworkspace（登録が消えても残るもの）は`session_workspaces(role, workspace_id, created_at)`（schema v11）。存在判定は`WorkspaceBackend::exists`（cmux adapterは`cmux --json --id-format uuids workspace list`のidに大文字小文字を問わず一致するか）で、listに居ないUUIDの行は消して作り直す。`workspace list`は呼び出し元のwindowのworkspaceしか返さない。

workspaceは作成時にtitleとcommandのほかに`WorkspaceTags`を持つ（`workspace_create_arguments`が`--name`、`--description`、`--env`、`--group`、`--command`、`--focus false`の順に並べ、`--cwd`を足す）:

- **env**: `DAGQ_ROLE=<role>`と`DAGQ_QUEUE=<canonical db path>`（`lifecycle::session_env`）。roleは`SessionRole`（`maintainer` / `supervisor` / `worker` / `planner` / `inbox`）。`cmux workspace env <id> --json`で読め、workspaceの全shellに継承される。maintainer / inbox / planner session内の`up`の`skipped`判定とpluginのhookはこれを読む。
- **description**: `dagq role=<role> queue=<queue hash>[ run=<run-id>][ task=<id>]`（`workspace_description`）の1行。人向けの補助で、判定には使わない。
- **group**: queueのworkspace group。`WorkspaceBackend::ensure_group(queue hash, "[<repo>]")`（cmux adapterは`cmux --json --id-format uuids workspace-group create --name "[<repo>]" --external-id <queue hash>`。既にあれば同じgroupが返る）のUUIDを`--group`で渡す。`up`は最初にworkspaceを作るときに1回だけ求め、supervisorはrunのworkspaceを作るたびに求める（cmuxは最後のworkspaceが閉じたgroupを消すので、前のrunのgroup UUIDを持ち越さない）。作れなければwarning（`up`はJSONの`warnings`、supervisorはlogの`warning: cmux workspace group …`）にしてgroupなしでworkspaceを作る。cmuxは`--from`なしの`workspace-group create`でgroupのanchor workspaceを生成するので、queueごとに1つ見出しのworkspaceが増える。

queue hashは`QueueLocation::hash()`: repositoryのqueueはqueueディレクトリ名（LaunchAgentのlabel`com.dagq.<hash>`と同じ）。`up`が`supervise`を`--db`で起動しても同じ値になるよう、`--db`のqueueでもファイル名が`queue.db`で隣に`repository`ファイルがあればディレクトリ名を使い、それ以外の`--db` queueはlabelのhash。

### Maintainer prompt

`src/runtime.rs`の`maintainer_prompt(db, log_dir)`がworker promptの隣で生成する（[ADR-0016](../adr/0016-maintainer-notification-and-compact-output.md)の決定8で5行以内に縮めた）。内容: この queue（db path）のmaintainerであることとsupervisorのlog dir、`dagq status`から始めてdagq pluginの`dagq-maintain` skillに従い`dagq watch --after <cursor>`をbackgroundで走らせ終了で起きること、subagentレビューが通ったrunは着地し、疑義（受け入れ条件との食い違い・指示外の変更・レビューの指摘）のときだけユーザーに聞き、`watch`が返っただけでは`integrate`しないこと（[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)決定3）、queue DBを直接開かずCLIだけを使うこと、skillが無ければそう報告して待つこと。CLIの手順はpromptに書かずskillに置くので、skillを変えてもpromptは変わらない。compactionと`/clear`からの起き直しはpromptではなくpluginの`SessionStart` hookが`status`を出して担う（[plugin-integration](plugin-integration.md#起き直しhookadr-0016)）。

### Session prompts

inboxとplannerの初期promptは`src/runtime.rs`の`inbox_prompt(db)` / `planner_prompt(db)`が`maintainer_prompt`の隣で生成し、どちらも5行以内（[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)）。workspaceのcommandは`lifecycle`の`inbox_command` / `planner_command`で、maintainerと同じく`<claude> [--plugin-dir PATH] -- '<prompt>'`。

- **inbox**: このqueue（db path）のinboxで、askを人に取り次ぎ自分では判断しないこと。`dagq status --role inbox`から始め、`dagq watch --role inbox --after <cursor>`をbackgroundで回して終了で起き、返ったcursorからwatchし直すこと。`ask_opened`が来たら`dagq asks --open --role inbox`でaskを読み、questionとoptionsを人に見せ（AskUserQuestionが使えるなら使う）、人の答えを`dagq answer ID --text '<answer>'`で書くこと。queue DBを直接開かずCLIだけを使うこと。
- **planner**: このqueueのplannerで、人の課題を聞いてgoalとtaskにすること。dagq pluginの`dagq` skillで登録してtaskを`ready`にすること。goalの全taskが完了したらreceiptをgoalのacceptanceと照合して`dagq goal close ID --verdict achieved`で閉じること。queue DBを直接開かずCLIだけを使うこと。observerのnoteとdraft goalを見せる一文は後続goalで足す（今は書かない）。

### Maintainerへの通知（ADR-0016で決定、一部実装済み）

[ADR-0016](../adr/0016-maintainer-notification-and-compact-output.md)で次を決めた。`status`のattentionとcursor、`events --after`、`watch`は実装済み（[`status`](#status)、[`events` / `watch`](#events--watch)）。`review`は実装済み（下記「`review`」）。`cmux notify`は[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)で送る条件と宛先が改められ、`dagq ask`が新しいaskのときにinboxへ送る形で実装済み（[人への通知](#人への通知cmux-notify)）。圧縮出力と`--full`はtask 59、短い`maintainer_prompt`とpluginの`SessionStart` hookはtask 65で実装済み。

- maintainerは状態を持たない使い捨てのsessionで、compaction・`/clear`・再起動からの起き直しは`status`の1コマンドで行う。`status`はsupervisorの健全性、未完了run、attention、次のcursorを上限のある大きさで返す。
- `watch --after <cursor>`はcursorより後のattentionイベントかsupervisor健全性の変化までblockし、attentionと新しいcursorを返して終わる。maintainerはこれをbackgroundで走らせて終了で起きる。`doctor`は診断専用で、pollingには使わない。
- attentionはrun_eventsのkind（公開契約。既存のkind名とpayloadは変えず追加だけ）からdomainが判定する: runの`awaiting_integration`・`needs_session`・`failed`、`exit_request_timed_out`、`prompt_waiting`（[ADR-0019](../adr/0019-move-routine-maintainer-work-into-the-runtime.md)）、supervisorの停止/stale（のちに[ADR-0025](../adr/0025-leaseless-unfinished-run-is-a-recover-run-attention.md)でsupervisorが手放した未完了runの`recover run`が加わった）。supervisorの状態はrun_eventsに載せず`supervisors`表から導出し、schemaは変えない。
- supervisorはattentionのたびにmaintainer workspaceへ`cmux notify`を送る（人向け。のちに[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定5で`ask_opened`のときだけinbox宛てに改まった）。runtimeはmaintainerのterminalに`cmux send`で打ち込まない（workerへの`/exit`は従来どおり）。`integrate`は`watch`からもイベントの副作用としても呼ばない。
- maintainer経路のコマンドは既定で圧縮し（既存キー名を変えずに省く・切り詰める）、全文は`--full`。`show`・`goal show`・`doctor`は実装済み（`doctor`は上、`show`と`goal show`は[domain-model](domain-model.md)）。レビューは`review ID`が`<run_dir>/review.md`を書き、maintainerはsubagentにpathを渡す（`review`は実装済み。下記「`review`」）。
- `maintainer_prompt`は「`status`から始め、`watch`をbackgroundで回し、attentionを報告して承認を待つ」に縮める（着地の承認は[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定3で「疑義のあるときだけ聞く」に改まった）。

## `supervise`

`dagq supervise [--parallel N] [--once] [--log-dir DIR] [--observe-interval SECS] [--observe-daily BOOL]`はrepository内で実行する常駐ループ（通常は`up`がlaunchdで起動する。手で専用ターミナルから起動してもよい）で、依存が解けたtaskを上限N（既定4）まで同時に実行する。queueはcwdから解決し（[persistence](persistence.md)のQueue location）、repositoryのcheckoutもcwdを使う。`--db PATH`と`--repo REPO`はそれぞれの明示override（[016](../journal/016-queue-per-repository.md)）。

起動時:

1. DBのpathを正規化し、checkoutのroot、Git common directoryを取得する。DBはworktree外か、common directory配下に置く（ユーザーDIRのqueueは常に満たす）。worktreeの作成元は`repo_path`に記録したcheckout。
2. cmux（`ping`）とClaude（`--version`）のpreflightを行う。
3. queueをrepositoryに束縛する（`bind_repository`）。別repositoryに束縛済みなら開始しない。queue全体の排他はなく、同じqueueに別のsupervisorがいても構わない。
4. supervisorプロセスのtoken（UUID）を作り、`supervisors`表に自分を登録する（`register_supervisor`: token、PID、`--parallel`、`started_at`）。runを1つも持たない常駐supervisorも、この登録で`status`/`doctor`に並ぶ。続けて別スレッドで2秒ごとにそのtokenの登録と全leaseのheartbeatを1トランザクションで更新する（`heartbeat(token)`）。heartbeatの失敗はループで検知し、全runに`runtime_error`を記録してleaseと登録を残したまま終了する（プロセス終了後にstaleになる）。

ループ（1秒ごと）:

5. **adopt**（[ADR-0012](../adr/0012-adopt-stale-lease-of-live-wrapper.md)）: active runが上限未満なら、claimの前に、他のtokenのleaseを持つ`running` / `validating`のrunを`runs_leased_by_others`で読み、leaseがstale（pidが死んでいるかheartbeatが30秒より古い）で、wrapperが生きていてheartbeatが30秒以内か`exited_at`が記録済みのものを`adopt_run`で引き継ぐ。`adopt_run`は`BEGIN IMMEDIATE`の中でstatusとstaleを再検査し、lease行の`token` / `pid` / `heartbeat_at`と`task_runs.supervisor_token`を自分のものにして`run_adopted`を書く（同じrunを2つのsupervisorが取ろうとしても1つしか通らない。負けた方は何もしない）。引き継いだrunのslotはDBから組み立てる: pathは`run_planned`のもの、`receipt_seen`はreceiptファイルと`receipt_observed`イベントの有無、`exit_requested`イベントがあれば`/exit`を再送せずtimeoutをいまから数え直し（`exit_request_timed_out`が記録済みなら再記録しない）、wrapperの登録待ちは持たない。`validating`のrunは9の検証をはじめから行う。`claimed` / `starting`（wrapperの登録にclaimしたtokenが要る）、leaseのないrun（abandon済み・`recover`済み）、`integrating`、wrapperが死んでいるか黙っているrunは引き継がず、[`recover`](#recover-run_id)に残す。引き継ぎはlogに1行で残す。
   **claim**: 続けて、`graph_input`から`dependency_graph`でcandidatesをclaim順（解放数`unblocks`の多い順、同数ならID昇順。[ADR-0023](../adr/0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)の決定4、`graph`と同じ順）に並べ、空でなければ`refs/heads/main`を読み直してbase commitにし、`claim_for_supervisor_in_order`でその順の先頭からまだclaim可能なtaskを取り、run・`supervisor_token`・lease行を1トランザクションで作る。順序は`graph`で再現できるので`claim_reordered`は記録しない。`integrate`で依存が解けたtaskは次のループで、先行taskを含む`main`から始まる。
6. **provision**: run管理領域（DBと同じdirの`runs/<run-id>/`）のpath、branch `dagq/<run-id>`、worktree（`runs/<run-id>/worktree`）、receipt、logのpathを`run_planned`として先にDBへ保存し、ディレクトリ、`prompt.txt`、runtimeバイナリのスナップショット`runner`、worktreeを作り、cmux workspaceを`--name "[<repo>]worker#<task-id> - <task title>" --description "dagq role=worker queue=<queue hash> run=<run-id> task=<id>" --env DAGQ_ROLE=worker --env DAGQ_QUEUE=<db> [--group <queueのgroup>] --command '<runner> --db ... session --run ... --lease <token> --claude ...' --focus false --cwd worktree`で作成して、`identify`で解決したUUIDを`workspace_created`として保存する。wrapperにはDBのpathを`--db`で明示的に渡す。provisioningの失敗は環境要因とみなし、そのrunをabandon（下記）した上で以後のclaimを止め、active runをdrainしてから非0で終了する。`prompt.txt`の内容は下記[Prompt](#prompt)。
7. **監視**: 各tickの先頭で、そのrunのlease行がまだ自分のtokenであることを確認する。なければ（別のsupervisorが引き継いだ、または`recover`された）そのrunをslotから外し、DBには何も書かず結果の`errors`に載せる。tickの途中でleaseを失ってlease付きの書き込みが失敗した場合も同じで、abandonしない（`last_error`を書かない）。検証threadが動いていればそのまま終わらせる（結果は記録されない。引き継いだ側が検証をやり直す）。続けてrunごとの`SessionWatch`が、wrapperの登録（45秒以内）、wrapper heartbeat（30秒以内）、receiptファイルの出現、idle marker、wrapperの終了を確認する。receiptの出現は`receipt_observed`（`validated: false`）として記録するだけで、セッション終了とは別に扱う。receipt観測後にidle markerがreceiptより新しければ`session_idle_observed`を記録し、`exit_requested`を記録してから`WorkspaceBackend::send_exit`で一度だけ終了を要求する（下記）。wrapperが`exited_at`を記録済みのsession（自分で終わった、maintainerが`/exit`を打った、引き継ぐ前に終わっていた）には終了を要求しない。
8. wrapper終了後に画面を`terminal-final.txt`へ保存し、`supervision_finished`でrunを終了コード0なら`validating`、それ以外なら`failed`にする。非0のときは同じトランザクションで`last_error`に`session exited with code N`を書き、`show`だけで理由が分かるようにする。Taskは`in_progress`のまま残す。
9. `validating`のrunはreceipt検証（下記）をrunごとのthread（専用SQLite接続）で行い、ループは完了を待ちながら他のrunを監視し続ける。完了したら`validation_finished`でrunを`awaiting_integration`または`failed`にする。
10. `awaiting_integration`になったrunだけ`cmux workspace close <workspace_id>`でworkspaceを閉じ、`OK workspace:N`の応答を確認して`workspace_closed`（`task_runs.workspace_closed_at`）を記録する。worktreeとbranchは統合まで残す。closeが失敗したら`cleanup_failed`イベントと`last_error`に記録し、runは`awaiting_integration`、`workspace_closed_at`はnullのままにする。
11. `awaiting_integration`または`failed`になったrunのleaseを解放する（`lease_released`）。
12. **observer**: 停止要求が無くclaimを止めていなければ、observationの期日が来ていて走っているobserverが無いときに`dagq observe`を子プロセスで起動する（[Observer](#observer)）。run slotは使わない。
13. active runも走っているobserverもなく、`--once`か停止要求（下記）か、provisioning失敗でclaimを止めていればループを抜ける（走っているobserverは自分のtimeoutまでで終わるので、runと同じく待つ）。それ以外はactive runがない間2秒ごとに`candidates`を見る。ループを抜けたら（claimやGitのエラーで抜ける場合も含む）自分の登録を消す（`deregister_supervisor`）。heartbeat失敗で終わるときだけは消さない。

結果は`{"outcome": "finished" | "stopped", "runs": [休止したrun], "errors": [{run_id, task_id, message}]}`。SIGINT/SIGTERMは1回目でclaimを止めてactive runの終了を待ち（graceful drain）、2回目で既定の動作（即終了）になる。即終了した（killされた）supervisorのleaseはPIDが死んだ時点で（遅くともheartbeatの30秒で）staleになり、wrapperが生きているrunは次のfill passで別のsupervisorが引き継ぐ（5）。登録はPIDが死んだ時点から`stale`として`status`/`doctor`に残る。`status` / `doctor` / `recover` / `integrate`は登録を消さず、次の`up`がPIDの死んだ登録だけを消す（[`up` / `down`](#up--down)）。

### 人への通知（`cmux notify`）

`WorkspaceBackend::notify(title, body, workspace)`は人への通知の操作で、cmux adapterは`cmux notify --title <title> --body <body> [--workspace <id>]`を実行する（`workspace`が`None`なら`--workspace`を付けない。失敗はcmuxの非0終了をエラーにして返す）。terminalへの打ち込みではないのでmaintainerやworkerのUI状態に干渉しない。

`cmux notify`を送るのは`dagq ask`だけで、新しいaskを登録したとき（`ask_opened`を書いたとき）に1回送る（[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定5、task 87）。askを作るのはmaintainerやworker、observer（`blocked`）のプロセスでsupervisorではないので、通知はaskコマンド自身が`runtime::ask`で送る。宛先は`session_workspaces`に記録されたinboxのworkspace UUID（`up`が記録する）で、記録が無ければ`--workspace`なしで送る。titleは`[<repo>] ask #<id> <kind>`（`<repo>`はqueueが束縛されたrepositoryのmain checkoutのディレクトリ名。束縛の無い`--db` queueは作業ディレクトリの名前）、bodyはquestionの先頭200文字（超えれば`…`）と、改行の後の`task <id>`（runのaskなら` run <run-id>`を続ける。observerの`blocked`でtaskの無いaskはこの行を省きquestionだけ）。同じ（task、run、kind）のopenなaskを返しただけ（`created: false`）なら送らない。`answer`と`ask close`も送らない。cmuxは`ask --cmux <path>`（既定は`cmux`をPATHで解決）。通知の失敗でaskは失敗しない: askは登録済みのまま、出力の`notified`をfalseにして`notify_error`に理由を書く（成功なら`notified: true`）。`backend_call_failed`には記録しない（runにもsupervisorにも属さない呼び出しで、inboxはaskを`watch --role inbox`で受けるので通知は補助）。

supervisorは通知を送らない。[ADR-0016](../adr/0016-maintainer-notification-and-compact-output.md)の契約(3)はattentionのたびにmaintainer workspaceへ送るとしていたが、ADR-0022の決定5でrunの遷移（`awaiting_integration`・`needs_session`・`failed`・`exit_request_timed_out`など）は通知しないことになった。`integrate`の`push_failed`（task 70）も通知しない。maintainerはattentionを`watch`で受ける。

### Run environment

repository rootの`dagq.toml`の`[run.env]`（[ADR-0023](../adr/0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)の決定3）が、runごとの環境変数になる。読み込みは`src/infrastructure/run_env.rs`の純粋関数（`parse_run_env`と`expand`、fileを読む`load_run_env`）で、ファイルが無ければ空。

- 書式はTOMLの部分集合: `[run.env]`の表だけを持ち、各行は`KEY = 'literal'`か`KEY = "basic"`（`\\` `\"` `\n` `\t`のescape）。`#`以降はcomment。ほかの表、表の外のkey、環境変数名でないkey、重複したkey、`DAGQ_`で始まるkey（runtimeが`DAGQ_ROLE` / `DAGQ_QUEUE`に使う）はエラーにする。
- 値の`${DAGQ_QUEUE_DIR}`はqueue directory（DBのある directory）、`${DAGQ_RUN_DIR}`はそのrunのrun directoryに展開する。ほかの`$`は書いたまま残す（shellの展開はしない）。ADR-0023は`${DAGQ_QUEUE_DIR}`だけを挙げるが、task 91で`${DAGQ_RUN_DIR}`も加えた。
- 読むのはrepositoryのmain checkout（Git common directoryが`.git`ならその親、bareなら`supervise` / `integrate`を実行したcheckout）の作業ファイルで、run worktreeのものではない。`integrate`をどのworktreeから呼んでも同じファイルを読む（common directoryが`.git`という名前でない構成だけは、実行したcheckoutのものを読む）。検証コマンドが1件も無ければ読まない。
- 渡し先: (a) `provision`がworkerのworkspaceを作るとき、`DAGQ_ROLE` / `DAGQ_QUEUE`の後ろに`--env KEY=VALUE`で並べる（ADR-0026の仕組み）。worktreeを作る前に読むので、壊れた`dagq.toml`はprovisioningの失敗になり、workspaceは開かずsupervisorはclaimを止める。(b) `integrate`の`verification_commands`を`Command`のenvに足す（validatingは検証コマンドを実行しない）。読めないファイルは着地処理のエラーで、runは元の状態に戻る。(c) needs_sessionのresume（task 71）とreviewのheadless実行（ADR-0023の決定2）はまだruntimeに無く、それぞれの実装で同じenvを渡す。
- `dagq.toml`はrepositoryにcommitされ、値はworkspaceを開く`cmux`のargvに出るので、secretは入れない。
- この repositoryではtargetを共有せず、`dagq.toml`も置かない（ADR-0023の決定3は`CARGO_TARGET_DIR = "${DAGQ_QUEUE_DIR}/target"`を置くとしたが、task 91の着地前のreviewの指摘を受けて2026-09-23にユーザーが決めた）。理由: (a) cargoのlockはbuildだけを直列化し、その後のtest実行は分離されないので、`CARGO_BIN_EXE_dagq`をexecするtest（`tests/cli.rs`・`runtime.rs`・`location.rs`・`plugin.rs`・`e2e.rs`）が、並行する別のrunのbuildが上書きした`target/debug/dagq`を実行しうる。(b) 同時の`cargo llvm-cov`が共有の`llvm-cov-target`のprofrawを消し合い・混ぜ合い、coverageの関門が誤る。buildの共有はsccacheなど安全な方法を別途検討する。

### Prompt

`prompt.txt`は`src/runtime.rs`の`prompt(task, run, goal, predecessors, siblings)`が生成するclaim時点のスナップショットで、run中にqueueが変わっても書き換えない。goalの`goal edit`も、兄弟taskの状態変化も、走行中のrunには届かず、次のclaimのpromptから反映される。task単体の情報（ID、run ID、title、description、acceptance、verification_commands）、receiptの契約（pathとJSONの形）に加えて、[ADR-0009](../adr/0009-goal-groups-tasks.md)の次の4節をこの順で検証コマンドとreceiptの契約の間に載せる。どれも常に書き、該当がなければ`none`にして、promptの節構成をgoal・context・依存・並列の有無で変えない。

- **Goal**: taskに`goal_id`があれば、claim時点の`TaskStore::show_goal()`のgoalを`Goal ID` / `Goal title` / `Goal description` / `Goal acceptance` / `Goal constraints` / `Goal doc`の行で載せる。`doc`はrepository内のpathをそのまま書き、内容は読まない（なければ`Goal doc: none`）。goalのないtaskは`Goal: none, this task stands alone`。
- **Context**: taskの`context`が空白でなければ本文をそのまま載せ、空なら`Context: none`。
- **Predecessor tasks**: taskの直接の依存元（`task_dependencies`のpredecessor）ごとに1行、`- task <ID>: <title>; result commit <sha>; summary: <text>`。`result commit`は依存元の`integrated` runの`result_commit`（`integrate`がmainに積んだsquash commit）、`summary`はそのrunのreceipt（`receipt_path`。なければ`<run-dir>/receipt.json`）の`summary`（空白を1つに畳む。空なら`(no summary)`）。receiptが読めない・parseできない・pathが不明なら`(receipt unavailable)`、integrated runがなければ（手で`completed`にしたなど）`result commit (not landed)`と書き、いずれもprovisionを止めない。取得はqueueの読み取り専用操作`TaskStore::predecessors(task_id)`（ID順。依存元の`Task`と`integrated` runの`Option<TaskRun>`）で、summaryの読み取りはruntime側（`PredecessorSummary::from_predecessor`）が行う。依存元がなければ`Predecessor tasks: none`。
- **Sibling tasks in progress**: `TaskStore::tasks_in_progress()`が返す`in_progress`のtask（ID順）から自分のtaskを除き、taskにgoalがあれば同じ`goal_id`のtaskに限定したものを`- task <ID>: <title>`で並べる（`siblings_in_progress`）。goalのないtaskはgoalの有無を問わず全`in_progress` taskを見る。claimは`fill_slots`で1件ずつ順に行うので、同じpassで後にclaimされたtaskのpromptには先にclaimされたtaskが載り、その逆は載らない。`awaiting_integration`や`needs_session`のrunを持つtaskも`in_progress`なので載る。なければ`Sibling tasks in progress: none`。

冒頭（worktreeだけで作業する指示の直後）に、最初に読むものを`WORKER_READING`の一文に限定する: repository instructions（AGENTS.md）のworker節、この下のtask context（とそれが名指す文書）、goal doc、依存元のsummaryだけを読み、`dagq list` / `dagq show`は打たず、docs全体は読まず、他のファイルはtaskが必要とするときだけ開く（goal 11の決定4。runに要る情報はpromptに載っていて、queueの一覧やdocs全体を読むのは最初のcommitを遅らせるだけ）。

判断が要るときの手順も載せる: terminalに質問を書いて待つのではなく、worktreeで`dagq ask --run <run-id> --kind worker_question --question '...'`を打ち、短く報告して止まる。回答は`answer to ask <id>: ...`としてterminalに届く（[workerの質問への回答の送信](#workerの質問への回答の送信)）。

末尾の「receiptを書いたら短く報告して止まる」の直前に`STOP_BACKGROUND`の一文を置く: receiptを書く前に、自分が起動したbackgroundの処理（`run_in_background`のshell、待ちループ、watchなど）をすべて止める。残っているとClaude Codeが`/exit`に「Background work is running — Exit and stop tasks / Move to background and exit / Stay」の確認画面を出して止まり、`exit_request_timed_out`になる（2026-09-23にtask 49・75・74・118で起きた）。同じ一文をresumeの定型の解消依頼（[`needs_session`](#needs_session)）にも手順4として載せる。runtimeはこの確認画面を検知も応答もしない（[ADR-0016](../adr/0016-maintainer-notification-and-compact-output.md)のTUI非結合と[ADR-0019](../adr/0019-move-routine-maintainer-work-into-the-runtime.md)の決定6の「記録だけでキーは送らない」。ダイアログ待ちの検知はreceipt後の画面を読まない）。

taskに`required_evidence`があれば、verification commandsの直後（4節の前）に`Required evidence: e2e, tests (each must be passed with evidence in the receipt, or the run waits for a session to add it)`の1行を載せ、workerに事前に知らせる（無ければ行ごと出さない）。

4節の後に「担当はこのtaskだけ。兄弟taskの範囲を変えず、範囲外の仕事を見つけたら受け持たずにreceiptの`follow_ups`に書く」の一文を置き、receipt JSONの例に任意の`follow_ups`（`{title, description}`の配列。`Receipt::check`は配列であることだけを見る）を含める。

schemaとCLIは変えない。`tests/e2e.rs`のstubはpromptの1行目とreceipt pathの行だけを読み、`follow_ups`のないreceiptを書くので、節の追加に影響されない。

### 1 runの異常（abandon）

wrapper heartbeat切れ、検証処理そのもの（Git呼び出しやDB）の失敗、closeの記録失敗など、監視中のruntime errorは**そのrunだけ**を手放す: `last_error`と`runtime_error`イベント（`lease_released: true`）を書き、そのrunのlease行を削除し、status・`run_processes`・workspace・worktreeは変えない。supervisorは他のrunを続け、結果の`errors`にそのrunを載せる。leaseを消すのは、常駐supervisorが生きている間も`recover`がrunのprocessだけで判定できるようにするため。taskは未完了runで占有されたままなので二重実行にはならず、未登録のwrapperはleaseがなければ登録できずClaudeを起動しない。leaseがないので他のsupervisorも引き継がない。`status` / `watch`はこのrunを`recover run`のattentionとして出す（[ADR-0025](../adr/0025-leaseless-unfinished-run-is-a-recover-run-attention.md)。`kind`は`runtime_error`）。maintainerは`dagq-recover` skillに従い、`show`と`doctor`で確認して[recover](#recover-run_id)で扱う。recoverすればrunは`interrupted`（`integrating`だったものは`awaiting_integration`）になりattentionから消える。wrapperの登録を待つ時間は`WorkspaceBackend::registration_timeout`（既定45秒）。終了要求のtimeout（`exit_request_timed_out`）ではabandonせず、leaseを持ったままsessionの終了を待つ（[Receipt and session exit](#receipt-and-session-exit)）。

## Observer

[ADR-0024](../adr/0024-retire-maintainer-into-jobs-and-observer.md)の決定4。dagqが回っているかを観察して継続的改善の材料を残すjobで、個々の詰まりは解消しない。cmux workspaceを持たず、`AgentProvider::headless_command`（Claudeでは`claude -p --allowedTools 'Bash(dagq:*)' -- <prompt>`）で起動する。実装は`src/observer.rs`。

- **`dagq observe [--since CURSOR] [--daily] [--dry-run] [--timeout SECS] [--claude PATH]`**: 1回のobservation。
  1. 入力を集める: `stats --since <cursor>`（`--since`が無ければ前回のobserveが保存した`<queue dir>/observer/cursor`、それも無ければ`stats`の既定の直近50件。`--daily`は24時間前より前の最後のevent id）、直近20件のnote（`notes`）、openなask（`asks --open`）、`graph`の`candidates`と`critical`。
  2. promptを作る。役割は「dagqが回っているかを観察し、うまくいっていないことをnoteに記録し、`stats.alerts`を`kind: blocked`のaskでinboxに上げ（同じalertは1回だけ。openなaskかnoteで上げ済みなら作らない）、繰り返し起きている問題は証拠のobservationのidを引いて`goal add --draft`で改善提案を登録する。個々の詰まりは解消しない。run / task / goalの状態は変えない」。queueのコマンドは`dagq --db <db>`の形で渡す。`--daily`は24時間の傾向を見る別の文面にする。`--dry-run`はpromptを`{dry_run, mode, since, cursor, prompt}`で返し、何も起動・記録しない。
  3. `<queue dir>/observer/<started_at>/`（同じ秒に既にあれば`-1`以降の接尾辞）を作り、`prompt.md`と`input.json`を書き、`observe_started`（`mode`: `hourly` / `daily`、`since`、`dir`）をtaskの無いrun_eventsに記録する。
  4. そのdirをcwdに、env `DAGQ_ROLE=observer`、`DAGQ_QUEUE=<db>`、PATHの先頭に`dagq`のdirを置いてagentを起動し、stdout / stderrを`output.log`に書く。`--timeout`（既定1800秒）を過ぎたらkillする。
  5. 終了後、`observe_started`より後にobserverが書いたnote（`observation`で`by: observer`）、observerのask、新しいdraftのgoal（goalは登録者を持たないので、同じ間にplannerが登録したdraftも数える）を数え、`observe_finished`（`mode`、`outcome`: `succeeded` / `failed`（非0終了） / `error`（起動できない・timeout）、`exit_code`、`error`、`since`、`cursor`（`stats`の`next_cursor`）、`cursor_saved`、`notes`、`asks`、`goals`、`duration_secs`、`dir`）を記録して返す。`succeeded`のhourlyのときだけ`cursor`を`<queue dir>/observer/cursor`に保存する（一時ファイルからrename）。失敗したobservationのwindowは次のobservationが読み直す。dailyはcursorを動かさない。
- **権限**: observerのenvからのCLIは許可一覧で判定する（task 97の`observer_access`）。読み取り、`note` / `notes`、`ask --kind blocked`、`goal add --draft`とdraftのgoalへの`add`だけが通り、`ready` / `integrate` / `recover` / `goal ready` / `answer` / `observe` / `supervise`などは`{"error":"observer may not change queue state"}`で拒否される。
- **timer**: `supervise --observe-interval SECS`（既定3600、`--once`のときは既定0。0でobserverを起動しない。dailyも含む）と`--observe-daily BOOL`（既定true）。ループの各passで、走っているobserverが無ければ、dailyが有効で最後のdailyの`observe_started` / `observe_finished`から24時間経っていればdailyを、そうでなく最後のhourlyから`--observe-interval`秒経っていればhourlyを、`<runner> --db <db> observe --claude <claude> [--daily]`の子プロセスで起動する（cwdはcheckout、`DAGQ_ROLE`は外す）。一度も記録の無いmodeは期日が来ている。期日はqueueのrun_eventsで判定するので、別のsupervisorや手の`observe`も数える。加えて同じプロセスが同じmodeを起動してから間隔が経つまでは再起動しない（記録を書く前に落ちたobserverを毎passで起動しないため）。同時に走るobserverは1つで、run slotを使わない。子プロセスの終了はlogに1行残し、記録は`observe_finished`が持つ。launchd modeでもin-cmux modeでも`supervise`の既定値で同じに動く（`up`はこのflagを渡さない）。
- **出力の扱い**: noteはplanner（`dagq` skillの`reference/observer.md`）が人と読み、draftのgoalは人が採否を決めて`goal ready`か`goal close --verdict abandoned`にする。`blocked`のaskは`status --role inbox`に`ask_opened`として出る。

## `session` wrapper

cmux workspaceが起動する隠しコマンド。TTYが必要で、パイプからは起動しない。

1. `workspace_id`が保存されるまで待ち（45秒以内）、wrapperのPIDを一度だけ登録する。leaseが無効なら登録できない。
2. `prompt.txt`を読み、providerのコマンドでagentを起動して`agent_started`を記録し、runを`running`にする。
3. 1秒ごとにheartbeatを更新しながら子プロセスをwaitする。DB障害中も子プロセスの所有を手放さない。
4. 終了コードを`session_exited`として記録する。agent起動後のエラーでは子プロセスが生きている可能性を考慮し、終了を記録しない。

## Receipt and session exit

receiptの受領とセッション終了は別の事象である。agentはreceiptを`<run-dir>/receipt.json`へ一時ファイルからrenameして公開し、応答完了後もセッションを維持する。セッション終了はwrapperが記録する終了コード（`session_exited`）だけで確認し、画面文言は使わない。

receipt受領後の終了要求は次の順で自動化している。

1. **idle判定**: Claude adapterがrunごとの`<run-dir>/claude-settings.json`に`Stop` hookを書き、`--settings`で渡す。hookはClaudeが応答を終えるたびにstdinのイベントJSON（`session_id`、`hook_event_name`など）を`<run-dir>/idle.json`へ一時ファイル + renameで書く。supervisorはreceiptを観測した後、`idle.json`のmtimeが`receipt.json`のmtime以上なら「receipt提出後に応答が完了した」と判定する。receiptより古いmarker（maintainerへの質問で止まった以前のturnなど）は無視する。判定の根拠（両ファイルのmtime、hookのフィールド）は`session_idle_observed`に記録する。権限確認や質問で止まっているturnでは`Stop`が発火しないため、その間は終了要求を送らない。
2. **終了要求**: `WorkspaceBackend::send_exit(workspace_id)`で、cmuxでは`cmux send --workspace <uuid> -- /exit`の後に`cmux send-key --workspace <uuid> -- enter`を送る。maintainerが打つのと同じ経路で、1回だけ送り、再送やプロセスのkillはしない。`exit_requested`（`timeout_secs`付き）は送る前に記録する。sessionは`enter`で即座に終わり得て、wrapperの`session_exited`が`send_exit`の戻りより先にcommitされることがあるので、送った後に記録するとイベントの順序が因果と逆になる。送信の失敗はrunをabandon（leaseを外す）するので、引き継ぎの対象にならない。記録と送信の間でsupervisorが死んだ場合だけ、引き継いだsupervisorは`exit_requested`を見て`/exit`を送らず、runは`exit_request_timed_out`まで待って人の`/exit`を待つ（送った後に記録していたときは逆に`/exit`を2回送り得た。2回送らないことを優先する）。
3. **終了確認**: wrapperの`session_exited`を待ち、通常どおり`supervision_finished`へ進む。要求から`WorkspaceBackend::exit_timeout`（cmuxは120秒）以内に終了しなければ、`exit_request_timed_out`（`workspace_id`、`timeout_secs`）を1回だけ記録してlogに書き、そのrunを手放さずに監視を続ける（[ADR-0019](../adr/0019-move-routine-maintainer-work-into-the-runtime.md)の決定2）。runは`running`のままleaseも保持し、`last_error`と`runtime_error`は書かない。Claude Code自身のダイアログなどが`/exit`を止めた状態を想定しており（典型は、workerが残したbackgroundのshellや待ちループに対する「Background work is running」の確認画面。promptとresumeの依頼が`STOP_BACKGROUND`で予防する。[Prompt](#prompt)）、`/exit`は再送しない（開いたダイアログの別の選択肢を押しかねないため）。`status` / `watch`は`send /exit`のattentionを出し、人かmaintainerがダイアログを片付けて`/exit`を送れば、wrapperの`session_exited`を観測した時点で通常どおり`supervision_finished`→`validating`へ進む。wrapperのheartbeatが切れた場合は従来どおりabandon（下記）になる。supervisorがleaseを持っている間は`recover`が拒否されるので、人が止めるときは`down --force`でsupervisorを止めてから`recover`する経路も変わらない。引き継いだrunは`exit_request_timed_out`が記録済みなら再記録しない。このrunはsessionが終わるまでslotを占め、drain（`down --wait`、SIGINT / SIGTERM、`up`によるversionの入れ替え、`--once`）もそのrunを待ち続けるので、drainの前に`/exit`を送って終わらせる（待てなければ`down --force`）。

maintainerの手動`/exit`はいつでも有効で、markerがない（hookが無効化されているなど）場合は従来どおり手動終了を待つ。

### ダイアログ待ちの検知

receiptより前にClaude Code自身のダイアログ（folder trust、LSP pluginの推奨、auto modeの案内など）で止まったsessionは、`Stop`が発火せず画面も変わらないまま待ち続ける。supervisorは画面を読んでこれを検知し、記録するだけでキーは送らない（[ADR-0019](../adr/0019-move-routine-maintainer-work-into-the-runtime.md)の決定6。応答は人かmaintainer）。

- **条件**: agentの登録（`agent_started`）をそのsupervisorが最初に見てから`WorkspaceBackend::prompt_wait`（cmuxは90秒）以上経ち、receiptも`idle.json`もなく、closeされていない`worker_question`もなく（askで止まったworkerは回答を待っているのでダイアログ待ちではない）、wrapperが生きて（未終了でheartbeatが有効）agentのPIDも生きているrun。画面の読み取りは`WorkspaceBackend::capture`で、多くとも10秒に1回（`prompt_wait`がそれより短ければその間隔）。引き継いだrunは引き継いだ時点から数える。読み取りの失敗はsupervisor logに書くだけでrunには影響させない。
- **判定**: `runtime::detect_prompt(screen) -> Option<PromptKind>`（純粋関数）が、画面の空行を除いた末尾30行について、枠線（`│`など）と前後の空白を除いた行で見る。`Do you trust`で始まる行か`trust this folder`を含む番号付き選択肢があれば`trust`、`❯`で始まる番号付き選択肢（`❯ 1. …`）の前後3行以内にも番号付き選択肢があれば（選択肢の文が折り返しても）`choice`、`Enter to confirm`か`Esc to cancel`で始まる行があれば`confirm`。文中や引用の中の同じ文言（作業中の出力やコード）は行頭にないので数えない。
- **記録**: 兆候があれば`prompt_waiting`（`workspace_id`、`excerpt`=空行を除いた末尾15行、`screen_hash`=excerptのSHA-256、`prompt`=判定の種類）を記録してlogに書く。同じ`screen_hash`の間は再記録せず、別のダイアログに変わればもう一度記録する。記録した後に兆候が消えるか、idle markerが書かれるかagentのPIDが死ねば`prompt_cleared`（`workspace_id`）を記録する（receiptが来たときは`receipt_observed`がattentionを終わらせるので記録しない）。引き継いだrunは最後の`prompt_waiting`（その後に`prompt_cleared` / `receipt_observed`が無いもの）の`screen_hash`を引き継ぎ、同じ画面を再記録しない。記録済みのダイアログがある間は`prompt_wait`を待たずに読み取りの間隔で画面を読むので、supervisorの不在中に応答されたダイアログもすぐ`prompt_cleared`になる。
- **attention**: `prompt_waiting`はattentionイベントで、`next`は`answer the prompt in workspace <workspace_id>`。`status`は`prompt_waiting`の後に`prompt_cleared`も`receipt_observed`もない`running`のrunを同じ`next`で出す（下記「`status`」）。`cmux notify`は送らない（[人への通知](#人への通知cmux-notify)のとおり、ADR-0022の決定5でrunの遷移は通知の対象外になった）。maintainerは`status` / `watch`でこのattentionを拾う。

receiptの形式は`src/domain.rs`の`Receipt`で、promptとREADMEに同じ契約を書いている。

```json
{"run_id": "...", "result": "succeeded | failed", "commit": "full SHA",
 "tests": {"status": "passed | failed | not_applicable", "evidence_or_reason": "..."},
 "e2e": {...}, "subagent_review": {...}, "summary": "..."}
```

### workerの質問への回答の送信

[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定2。runtimeがsessionのterminalに打ち込むのは`/exit`・resumeの解消依頼と、このworkerへの回答だけ（ADR-0016の決定3の例外）。

- **条件**: `SessionWatch::poll`のたびに、wrapperが生きていて`/exit`をまだ要求していないrunについて、そのrunの`worker_question`のうち回答済みでcloseされていないask（`SqliteQueue::undelivered_answers`）を古い順に見る。`idle.json`のmtime（unix秒）がaskの`created_at`以上なら、workerはaskの後に応答を終えて入力を待っているとみなす（秒単位の比較なので、同じ秒の中でaskより前に書かれたmarkerも通るが、askはworkerの作業中のturnで打たれるので、その前の`Stop`が同じ秒に収まることは実際には無い）。markerが無いかaskより古ければ送らず、次のpollで見直す。
- **送信**: `WorkspaceBackend::send_text`で`answer to ask <id>: <answer>`を送り、Enterを押す（cmuxは`cmux send`で改行を空白に畳んだ1行を打ち、`send-key enter`）。成功すれば同じトランザクションでaskの`closed_at`を書き、`ask_delivered`（`ask_id`、`workspace_id`）を記録する（`SqliteQueue::ask_delivered`）。その間に誰かがcloseしていれば何も書かない。送った後の記録の失敗はlogに書くだけで、生きているrunを手放さない。
- **失敗**: 送信の失敗はrunを手放さず、`ask_delivery_failed`（`ask_id`、`workspace_id`、`error`）を記録してlogに書き、askはcloseしない。送信は1回だけで、`ask_delivery_failed`のあるaskは（引き継いだsupervisorも）再送しない。送信とcloseの間でsupervisorが死んだときだけ、引き継いだsupervisorがもう一度送りうる。
- **attention**: `answer`は`worker_question`の`ask_answered`のpayloadに`runtime_delivers`（回答した時点でrunが`running`か）を足す。`true`ならattentionイベントにしない（maintainerの`watch`を起こさない）、`false`なら`send the answer of ask <id> to the worker and close it`のattentionイベント。`status`は回答済みの`worker_question`を、runが`running`でstaleでないleaseがあり送信に失敗していなければ`next: delivering the answer of ask <id> (runtime)`（何もしなくてよい）、`ask_delivery_failed`があれば`kind: ask_delivery_failed`で、runが`running`でないかleaseが無いかstaleなら（誰も送らない）`kind: ask_answered`で、どちらも`next: send the answer of ask <id> to the worker and close it`として出す。`ask_delivery_failed`はattentionイベント（maintainer宛て）。`ask_delivered`はattentionではない。
- **ダイアログ待ちとの関係**: closeされていない`worker_question`を持つrunは画面を読まず（他のkindのaskはダイアログ待ちの検知を止めない）、記録済みの`prompt_waiting`は`prompt_cleared`にする。`status`もそのrunに`answer the prompt in workspace <id>`を出さない。
- **maintainer**: `status`の`asks`に`worker_question`が出る。worktreeの中の判断ならmaintainerが`answer`し、それ以外は同じ質問で`kind=decide`のaskを登録して人の回答をそのまま`answer`で転送する（`dagq-session` skill）。

### 最初のcommitの観測

supervisorは`SessionWatch::poll`のたび（1秒ごと）に、`first_commit_observed`がまだのrunのworktreeの`HEAD`を`GitRepository::head`で読み、runの`base_commit`から動いていれば`first_commit_observed`（`commit`=そのHEAD、`base_commit`）を1回だけ記録する。時刻は観測した時点（commitからせいぜい1 tick遅れ）。引き継いだrunは既に記録があれば記録しない。HEADが読めないときはsupervisor logに書いて次のpollで読み直し、runには影響させない。`agent_started`→`first_commit_observed`が`stats`の`startup`で、workerが起動してから作業に入るまで（ダイアログ・読み込み）の長さを見る（goal 11の決定4）。

## Validation

`validating`のrunに対して、supervisorが終了したセッションと同じleaseの下で、runごとのthreadで次を順に確認する。最初に外れた項目が`failed`の理由（`last_error`）になり、以降は確認しない。

1. receiptが存在し、`Receipt`として解釈できる。
2. `run_id`が一致し、`result`が`succeeded`である。`tests`/`e2e`/`subagent_review`は`failed`でなく、`passed`には証跡、`not_applicable`には理由が空でなく書かれている。`commit`は完全なSHAである。
3. worktreeのHEADがrun branch `dagq/<run-id>`を指し、そのcommitがreceiptの`commit`と一致する。
4. commitがbase commitと異なり（commitなしを拒む）、base commitの子孫である。
5. `git status --porcelain --untracked-files=all`が空である。untracked fileもdirtyとみなす。
6. （欠番）taskの`verification_commands`はここでは実行しない。同じcommitに対する実行は`integrate`のrebase後の1回だけ（[ADR-0023](../adr/0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)の決定1。[`integrate`](#integrate)のステップ5）なので、validatingの所要時間はreceiptとGitの照合だけで、`verification_command`イベントも`<run-dir>/verify-N.log`も作らない。検証コマンドで壊れるcommitもここでは`awaiting_integration`になり、`integrate`で`needs_session`になって自動resumeされる。
7. **要求evidence**（[ADR-0019](../adr/0019-move-routine-maintainer-work-into-the-runtime.md)の決定5）: taskの`required_evidence`（`add --evidence`）の各checkについて、receiptのそのcheckの`status`が`passed`で`evidence_or_reason`が空白でないこと（`Receipt::missing_evidence`）。1–5をすべて通ったrunだけをここで見るので、receiptやGitの不整合は従来どおり`failed`になる。要求されたcheckに限り、2の「`failed`でない」と「evidence_or_reasonが空でない」はここに回す（`Receipt::check_requiring`）ので、`failed`・`not_applicable`・空のevidenceはどれも`failed`ではなくここで欠落になる。要求されていないcheckの`failed`は従来どおり2で`failed`になる。欠けていればrunを`failed`ではなく`needs_session`にし、`last_error`を`evidence missing: e2e`（複数は`, `区切り、要求の順）にして、`validation_finished`（`status: needs_session`、`evidence_missing`に欠けたcheckの配列）の後に`evidence_missing`イベント（`checks`、`reason`。`status`は持たない: `stats`が`validation_finished`の`needs_session`で1回数えるため）を記録する。runのworkspaceは`awaiting_integration`と同じく閉じる（sessionは終わっていて、resumeは自分のworkspaceを開く）。supervisorはこのrunを[`needs_session`](#needs_session)の手順で自動resumeし、不足しているcheckの実行とreceiptの書き直しを依頼する。要求の無いtaskでは7は何もしない。

結果は`validation_finished`イベント（`status`、`result_commit`、`reason`、receiptの内容、7で欠けたときだけ`evidence_missing`）と`task_runs.result_commit`/`last_error`に保存する。4以降で拒否した場合もcommitは確認済みなので`result_commit`を残す。成功しても`awaiting_integration`はTaskを`in_progress`のまま保持し、下記の統合確認まで依存taskを解放しない。

## `review`

`dagq review ID`は、taskの`awaiting_integration`または`needs_session`のrun（`integrate ID`と同じ選び方）のレビュー資料を`<run_dir>/review.md`に書く（[ADR-0016](../adr/0016-maintainer-notification-and-compact-output.md)の決定7）。どちらのrunも無ければerrorで、何も書かない。DBは読むだけで、eventも状態も変えない。`head`は`<run_dir>/receipt.json`の`commit`（読めなければerror）。`base`はrunの`base_commit`だが、セッションが`head`を現在の`main`の上にrebase済み（`main`が`base_commit`と異なり`head`の祖先）なら`main`にする。そうしないと`needs_session`から戻るrunのレビューに、その間に着地した他taskの変更が混ざる。Gitは`GitRepository`（runの`repo_path`、無ければworktreeで`inspect`）を通して呼ぶ。

`review.md`の節は順に: 見出し（task id・title、run id・status、base（とrunの`base_commit`）、head、branch、worktree、`integrate-verify-N.log`の場所。検証コマンドは`integrate`のrebase後にだけ走るので、着地前のreviewの時点では無いか、前回の`integrate`の試行が残したもの）、Task（description、acceptance、verification commands）、Goal（taskにgoalがあるときだけ。acceptanceとconstraints）、Receipt（summary、tests / e2e / subagent_reviewのstatusとevidence_or_reason、follow_ups）、Commits（`git log --oneline <base>..<head>`）、Diffstat（`git diff --stat <base>...<head>`）、最後にDiff（`git diff <base>...<head>`の全文）。diffは`--no-color --no-ext-diff --no-textconv`で取り、コードフェンスは本文のどのbacktick列より長くする。ファイルは`<run_dir>/.review.md.<pid>.tmp`に書いてからrenameする。

文字コードとtimeout: reviewのGit呼び出しは他のGit呼び出し（`adapters::output`、30秒のtimeoutとstdoutのUTF-8要求）を通らず、review用の経路で`REVIEW_TIMEOUT`（300秒）のtimeoutを持つ。Commits・Diffstatと`--numstat`（返り値の`files_changed`/`insertions`/`deletions`）はstdoutをbytesで受けて`String::from_utf8_lossy`で読むので、UTF-8でないコミットメッセージやファイル名は置換文字になるだけで失敗しない。diff全文はメモリに載せず、Gitのstdoutを`<run_dir>/.review.md.<pid>.diff.tmp`へ直接流し、そのファイルをchunkで読んでbacktick列の最長を数えてから、見出し以降のテキストとフェンスで挟んで`.review.md.<pid>.tmp`へコピーする（フェンスの長さはdiffを読み終えるまで決まらないため）。diffはGitが出したbytesのままなので、Latin-1やShift_JISのテキストファイルを含むrunでは`review.md`はUTF-8でないbytesを含む。一時ファイルは成功でも失敗でも消す。

stdoutは`{"run_id","task_id","path","base","head","files_changed","insertions","deletions"}`だけで（数値は`git diff --numstat`の合計。binaryは1ファイル0行）、diff本文は返さない。maintainerは`path`をsubagentに渡して結論だけを受け取り、自分のコンテキストでdiff全文を読まない。

## `integrate`

`dagq integrate ID`（taskの`awaiting_integration`または`needs_session`のrun）と`dagq integrate --next`（`awaiting_integration`のrunを検証完了の古い順に1件）は、検証済みのrunをruntimeが`main`へ着地させる操作で、maintainerがレビュー後にrepository内で実行する（[ADR-0008](../adr/0008-merge-queue-squash-landing.md)）。`--db PATH`と`--repo REPO`は明示override。repositoryは`GitRepository::inspect`で開き、common directoryが`queue_repository.git_common_dir`と一致することを要求する。

1. **スロット**: runを`integrating`にし、このプロセスのtokenで`run_leases`の行を作る（`begin_integration`、`integration_started`）。同時に`integrating`のrunは1件（`one_integrating_run_per_queue`）で、別のrunが着地中ならerror。leaseは`supervise`と同じthreadで2秒ごとにheartbeatし、`status`/`doctor`に`integrating`のrunとして並ぶ。`--next`の順序は`validation_finished`イベントのid順で、`needs_session`のrunは取らない。
2. **worktreeの前処理**: worktreeが存在し、run branch `dagq/<run-id>`をcheckoutしていること。worktreeのpathはqueueの今の`runs/`から解決したもので、queueディレクトリを移した後でも見つかる。repository側の管理情報が旧pathを指したままだと後始末の`worktree remove`が失敗するので、主working treeで`git worktree repair <worktree>`を実行する（移していなければno-op。[ADR-0017](../adr/0017-resolve-run-paths-from-the-queue-directory.md)）。途中のrebase（`rebase-merge` / `rebase-apply`）が残っていれば`git rebase --abort`する（`integration_rebase_aborted`）。
3. **receiptの検査**: `<run-dir>/receipt.json`がparseでき、`result`が`succeeded`で（`failed`ならrunを`failed`にして終わる。下記）、`Receipt::check`を通り、`commit`がworktreeの現在のHEADに一致する。衝突なしのrunではHEAD = `result_commit`なので検証済みのreceiptがそのまま通る。`needs_session`から戻るrunでは、セッションが新しいheadでreceiptを書き直したことの検出になる。worktreeはcleanであること。`Receipt::check`の後、taskの`required_evidence`が欠けていれば（validationの7と同じ判定。resumeしたsessionがevidenceを書かずにreceiptを書き直した場合）着地せず`needs_session`にし、`last_error`を`evidence missing: …`、`integration_deferred`のpayloadに`checks`（欠けたcheckの配列）を書く。`Receipt::check`と要求evidenceを通った時点で、読んだreceiptを`integration_receipt`イベント（`main`、receiptの`commit`、`receipt`にJSON全体。`follow_ups`を含む）に記録する。HEAD不一致・衝突・再検証の失敗で着地しなかった場合も記録は残り、`integrate`を繰り返せばその回数だけ並ぶ。`validation_finished`は検証時のreceiptしか持たないので、セッションが書き直したreceiptの`commit`、evidence、`summary`、`follow_ups`をDBが持つのはこのイベントだけ。
4. **rebase**: `git rebase --no-autostash --no-verify <main head>`（main headは着地開始時に読んだ`refs/heads/main`）。すでにmainの上にあればno-op。衝突したら`git diff --name-only --diff-filter=U`とGitの出力を取り、`rebase --abort`でworktreeを検証済みheadに戻して`needs_session`にする。成功したら`integration_rebased`（`head_before`、`head_after`）を記録する。
5. **再検証と検証コマンド**: rebase後のHEADがmain headと異なり（同じなら「commitが残らない」として`needs_session`。変更が不要ならセッションが`failed` receiptを書く）、main headの子孫であること。`git status --porcelain --untracked-files=all`が空であること。taskの`verification_commands`を順に`/bin/sh -c`でworktree内で、[Run environment](#run-environment)の`[run.env]`をenvに足して実行し、出力を`<run-dir>/integrate-verify-N.log`、終了コードと末尾を`verification_command`イベント（`phase: integration`）に残す。1件でも非0なら`needs_session`で、supervisorが自動resumeする。各コマンドは30分でタイムアウトし、その場合は着地処理のエラーとして扱う。validatingは検証コマンドを実行しないので、これがそのcommitに対する唯一の実行で（[ADR-0023](../adr/0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)の決定1）、rebaseがno-opでも、headがrunの`result_commit`のままでも必ず実行する。task 48が入れたskip（no-op rebaseで再実行しない）は廃止し、`integration_verification_skipped`イベントはもう記録しない（kindは過去のeventを読むために残す）。
6. **着地**: `git commit-tree <HEAD>^{tree} -p <main head>`で1 commitを作る。messageはtaskのtitle、receiptの`summary`（空なら省略）、trailer `Dagq-Task: <task id>` / `Dagq-Run: <run id>`。`refs/dagq/runs/<run-id>`をrebase後のHEADに向けてから、mainをcheckoutしているworktree（`git worktree list --porcelain`）があればそこで`git merge --ff-only <commit>`、なければ`git update-ref refs/heads/main <commit> <main head>`でmainを進める。
7. **完了**: 1トランザクションでrunを`integrated`、`result_commit`を着地commit、`last_error`をnull、Taskを`completed`にし、lease行を消して`run_integrated`（`result_commit`、`source_commit`、`main_before`、`history_ref`、`message`、`verification_skipped`、`git_common_dir`）、`lease_released`、`task_status_changed`を記録する（`finish_integration`）。
8. **後始末**: `git worktree remove --force <worktree>`と`git branch -D dagq/<run-id>`（`worktree_removed`）。失敗は`cleanup_failed`イベントと`last_error`に残し、statusは変えない。
9. **push**（[ADR-0019](../adr/0019-move-routine-maintainer-work-into-the-runtime.md)の決定3）: 着地したmainを`origin`（固定）へ`git push origin refs/heads/main:refs/heads/main`で送る。Gitはapplication層のport `MainRemote`（`has_remote`、`push_main`）を通して呼び、adapterは`GitRepository`（run worktreeは消えている場合があるのでcommon directoryを`--git-dir`にし、`GIT_TERMINAL_PROMPT=0`、timeout 300秒）。成功は`push_finished`（`remote`、`commit`）、`origin`が無いか`--no-push`なら`push_skipped`（`remote`、`commit`、`reason`）、失敗は`push_failed`（`remote`、`commit`、`error`）を着地したrunに記録する。どの場合もrunは`integrated`、taskは`completed`のままで、`integrate`は成功（exit 0）で終わる。`push_failed`はattention（`push main`）になり、人かmaintainerが原因を直して`git push origin main`を打つ。
10. **follow_upsの登録**（[ADR-0019](../adr/0019-move-routine-maintainer-work-into-the-runtime.md)の決定4）: 着地したreceipt（ステップ3で読んだもの）の`follow_ups`を1件ずつdraft taskとして登録する（`runtime::register_follow_ups`）。titleとdescriptionはfollow_upのもの、acceptance・`verification_commands`・dependenciesは空、goalは元のtaskと同じ（無ければgoalなし）、`context`は「task <id>（<title>）の run <run-id> の receipt が提案した follow_up」。登録ごとに着地したrunへ`follow_up_registered`（`task_id`、`title`、配列内の位置`index`）を記録する。goalが閉じていればgoalなしで登録し、payloadに`goal_closed: true`を書く。同じrunの`follow_up_registered`が既に持つ`index`は登録しないので、同じrunについて2回呼んでも二重に登録しない（`needs_session`で着地しなかった`integrate`は登録しないので、再着地で登録されるのは1回だけ）。`title`が空でない文字列でないか`description`が文字列でない項目は登録せず、`follow_up_registered`に`task_id: null`、`skipped`（理由）、`follow_up`（項目そのもの）を書く。taskの登録とイベントの記録は別のトランザクションなので、その間で記録に失敗した項目だけは次の呼び出しが再び登録しうる。登録の失敗はstderrに出すだけで、着地とexit codeは変えない。draftなのでsupervisorは拾わず、`ready`にするかは人が決める。`follow_up_registered`はattentionにしない。

結果は`IntegrationOutcome`: `{"outcome":"integrated","task":…,"run":…,"verification_skipped":…,"push":{"outcome":"pushed"|"skipped"|"failed","remote":"origin","error":…,"reason":…},"follow_ups":[{"task_id":…,"title":…}]}`（`verification_skipped`はステップ5が検証コマンドを常に実行するので常に`false`で、出力の形を保つために残す。`run_integrated`イベントのpayloadにも同じ項目が入る。`push`はステップ9の結果で、`error`は`failed`のときだけ文字列（ほかは`null`）、`reason`は`skipped`のときだけ入る。`follow_ups`はステップ10でこの呼び出しが登録したtaskの`[{"task_id","title"}]`で、無ければ空配列）、`{"outcome":"needs_session","run":…,"main":…,"reason":…}`、`{"outcome":"failed","run":…,"reason":…}`、`--next`で対象がなければ`{"outcome":"no_run_awaiting"}`。

### `needs_session`

衝突（4）と再検証の失敗（5）は`defer_integration`でrunを`needs_session`にし、理由を`last_error`、詳細（衝突ファイル、Gitの出力の末尾、rebase後のheadなど）を`integration_deferred`イベントに書いて、lease行を消しスロットを空ける。worktreeは衝突なら検証済みhead、再検証の失敗ならrebase済みのheadに置いたまま残す。runはTaskを占有し続け、`ready`/`cancel`はできない。

`integrate`は呼ばれた時点で、runに`integration_approved`イベント（`status`: 呼ばれた時のrunのstatus、`pid`、`push`: `--no-push`でなければtrue）を1回だけ記録する。supervisorが承認済みのrunを着地させるときは、この`push`に従ってmainを`origin`へpushする（`integrate`と同じ`push_main`）。`integrate`を呼ぶことが着地の承認で（[ADR-0016](../adr/0016-maintainer-notification-and-compact-output.md)の決定5）、runtimeは承認済みのrunだけを自分で着地させる。`integration_deferred`のpayloadには`resumes_left`（supervisorが残り何回resumeするか。3から`resume_started`の件数を引いた値で、0未満にならない）が付く。

supervisorは`needs_session`のrunを自分でresumeする（[ADR-0019](../adr/0019-move-routine-maintainer-work-into-the-runtime.md)の決定1。`resume_parked_runs`）。ループの各回で、adoptの後、claimの前に、空きslotの数まで`needs_session`のrunを古い順に見る。`resume_started`が3件（`MAX_RESUME_ATTEMPTS`）に達したrun、staleでないleaseを持つrun、前のsessionのwrapperのプロセスがまだ生きている（未終了でPIDが生きている。heartbeatが止まっていても）run、runは飛ばす。このsupervisorが見ていないsessionと同じworktreeで2つ目のsessionを始めないため。resumeの前に、このrunの`resume_finished`に記録された`workspace_id`のうち閉じていないもの（手放したsession、errorで残したもの）が`WorkspaceBackend::exists`でまだ開いていれば閉じる。workspaceはtitleで探さない（[ADR-0026](../adr/0026-identify-workspaces-by-uuid-env-and-queue-group.md)）ので、人が手で開いたresume workspaceや、resume中に死んだsupervisorが開いたworkspace（IDの記録が無い）は見つけない。後者はwrapperが生きている間は上の条件で飛ばし、終わった後に残ったworkspaceは人が閉じる。

1. **開始**: `begin_resume`が1トランザクションで、runがまだ`needs_session`で試行が3回未満、leaseが無いかstale（staleなら置き換える）で、heartbeatしている未終了のprocessが無いことを確かめ、runをこのsupervisorのtokenでleaseし（`lease_acquired`の`reason: resume`）、`supervisor_token`を移し、前のsessionの`run_processes`の行を消し（履歴はイベントに残る）、`resume_started`（`attempt`、`reason`、`main`）を記録する。`reason`は最後の`integration_deferred` / `integration_error` / `evidence_missing`の`reason`（無ければ`last_error`）。そのイベントが`evidence_missing`か、`checks`を持つ`integration_deferred`（上の要求evidenceで着地しなかった）なら、下の3の依頼をevidence用の文面にする、`main`はその時点の`refs/heads/main`。runの状態は`needs_session`のままで、新しい状態は足さない。進行は`resume_started` / `resume_finished`で読む。
2. **起動**: workerと同じ経路で起動する。runtimeのsnapshot（`<run-dir>/runner`）をこのsupervisorのbinaryで置き換え（workerの時のbinaryは`--resume`を知らないことがある）、`runner --db … session --run <run-id> --lease <token> --claude <claude> --resume`を`WorkspaceBackend::create_resume`でworkerと同じtitle（`run_workspace_name`: `[<repo>]worker#<task-id> - <task title>`。[ADR-0028](../adr/0028-workspace-titles-are-repo-and-role.md)。titleは表示専用）、description `run <run-id> resume`（`resume_workspace_description`）のworkspaceとしてworktreeで開く。workerと同じenvとgroup（`--env DAGQ_ROLE=worker` / `DAGQ_QUEUE`、queueのworkspace group。[ADR-0026](../adr/0026-identify-workspaces-by-uuid-env-and-queue-group.md)）を付け、cmuxが返したUUIDで扱う。wrapperは`register_resume_wrapper`（`needs_session`でこのtokenのlease）で登録し、`AgentProvider::resume_command`でClaudeを起動する。Claude Codeでは`claude --resume <run-id> --debug-file <run-dir>/claude-resume.log --add-dir <run-dir> --settings <run-dir>/claude-settings.json`で、workerと同じ`Stop` hookのidle markerを使う（素の`claude --resume`で開くと出たLSP pluginのダイアログを避ける）。agentは`register_resume_agent`で登録し、runの状態は変えない（`wrapper_started`、`agent_started`は同じkindとpayloadで記録される）。wrapperが45秒以内に登録しなければresumeのerror。
3. **解消依頼**: agentが登録されてから`resume_prompt_delay`（既定5秒）待ち、定型の解消依頼を`WorkspaceBackend::send_text`（cmuxでは`cmux send`とEnter。`cmux send`は`\n`などをキーとして読むので、改行とタブは空白、backslashは`/`にした1行で送る）で1回だけ送る。同じ文面を`<run-dir>/resume-<attempt>.txt`に残す。内容は、`integrate`が`needs_session`を返したこと、`Reason:`（上の`reason`）、rebase先の`main`とrunのbase commit、base..mainに着地したtask（mainのcommitの`Dagq-Task` trailerを古い順に引き、taskのtitleと`integrated` runのreceiptの`summary`）、手順（`git rebase <main>`と衝突の解消、`verification_commands`の再実行とcommit、worktreeをcleanに保つ、自分が起動したbackgroundの処理をすべて止める（`STOP_BACKGROUND`）、receiptを新しいheadで一時ファイルとrenameで書き直す、変更が不要なら`result: failed`と理由、mergeもpushもしない、終わったら短く報告して止まり`/exit`は打たない）。理由が`evidence_missing`（または`checks`付きの`integration_deferred`）のrunでは、冒頭を「validationがreceiptの要求evidenceの欠落を見つけた」に、手順1–2を「reasonが名指す不足しているcheckを実行してreceiptにevidenceを書く。ファイルが変わればcommitして検証コマンドを再実行する」に変える。
4. **終了**: `/exit`を1回だけ送る条件は3つ。(i) resume開始より新しいreceiptがparseでき、`run_id`がこのrunで、`succeeded`で`commit`がcleanなworktreeのHEADに一致し、taskの`required_evidence`を満たす（または`failed`）状態で、idle markerがreceiptより新しくなった。(ii) そういうreceiptが無いまま、idle markerが解消依頼を送った時刻より新しくなった（解消できずに応答を終えた、質問して止まった。実際のClaudeは自分では終わらない）。(iii) 依頼を送ってから`resume_timeout`（既定1時間）経ってもどちらにもならない（依頼が届かなかった、ダイアログで止まった）。wrapperが終了したら画面を`<run-dir>/terminal-resume-<attempt>.txt`に保存し、workspaceを閉じ（失敗はsupervisor logに書き`workspace_closed: false`）、`finish_resume`で`resume_finished`（`attempt`、`outcome`、`head`（worktreeのHEAD）、`workspace_id`、`workspace_closed`、`approved`、`status`: resume後のrunのstatus、未解消なら`exhausted`）を記録する。`outcome`は`resolved`（(i)のreceipt）、`failed`（receiptが`failed`）、`unresolved`（新しいreceiptが無い・別のrunのもの・HEADと一致しない・worktreeがcleanでない・要求evidenceがまだ欠けている）。`/exit`から`exit_timeout`（120秒）経ってもwrapperが終わらなければ、`/exit`は再送せず、sessionを手放す: `outcome: unresolved`、`exit_timed_out: true`、`workspace_closed: false`でleaseを外し、workspaceは残す（slotとleaseと`down --wait`のdrainを永久に止めないため）。そのsessionが生きている間はそのrunを次の試行に回さず、attentionは`resume session`になる。
5. **その後**: `resolved`で`integration_approved`があれば、leaseを持ったまま着地スロットが空くのを待ち（他のrunが`integrating`なら次の回に回す）、`begin_integration`（同じtokenのleaseはそのまま使う）から`integrate`と同じ関数（`land_integrating`: rebase、再検証、squash、完了、後始末）を別threadで走らせる。着地がまた`needs_session`になればleaseは外れ、次の回に1に戻る。`integration_approved`が無ければrunを`awaiting_integration`に戻してleaseを外す（`result_commit`は検証済みのheadのままなので、次の`integrate`は書き直されたheadを再検証する）。`failed`ならrunを`failed`にし、`last_error`を`session reported the run as failed: <summary>`にする。`unresolved`は`needs_session`のままleaseを外し、試行が残っていれば次の回に再びresumeする。
6. **打ち切りとerror**: 3回目の`resume_finished`が未解消なら`exhausted: true`になり、runは`needs_session`のまま人に返る（attention `resume session`）。workspaceの作成・送信・heartbeat切れなどresume自体のerrorは`resume_finished`（`outcome: error`、`error`、`exhausted`）とsupervisor logに書き、leaseを外す。runの`last_error`（衝突の理由）は変えず、試行は1回と数える。resume workspaceは、leaseを外した時点でwrapperが未登録（leaseが無いのでもう登録できない）か終了済みなら閉じ（作成の途中で失敗してIDが返らなければ何もしない）、wrapperが生きていれば調査のため残す（`resume_finished`の`workspace_id`に記録する）。残したworkspaceは上の条件で次の試行を止めるが、このrunの`resume_finished`に記録されたworkspaceは、runのwrapperがもう生きていなければ次の回に閉じて次の試行を始める。着地スロットを待つ間（`AwaitingSlot`）のerrorでは`resume_finished`は記録済みなので、leaseだけを外す。resume中にsupervisorが死んだ場合、leaseはstaleになる。runの状態は`needs_session`のままでadoptの対象ではなく、`integrate`はleaseがあるので拒否される。前のsessionが終われば（人が画面を見て`/exit`を送る）、次のsupervisorがstaleなleaseを置き換えて（`lease_acquired`の`previous_token`）次の試行を始める。

maintainerはresume workspaceを作らない。`status`のattentionは、supervisorがresume中（`resume_started`の後に`resume_finished`が無く、leaseがstaleでない）か、試行が残っていて前のsessionのwrapperが生きていない`needs_session`のrunで`next: resuming (runtime)`、それ以外（試行が尽きた、resume中のsupervisorが死んだ、生きているsessionが次の試行を止めている）で`resume session`になる（`watch::resume_pending`）。

セッションが変更不要と判断した場合はreceiptを`result: failed`と理由（`summary`）で書き直す。`integrate ID`は`fail_integration`でrunを`failed`にし（`integration_failed`。payloadの`receipt`にそのreceiptのJSON全体を持つ。`integration_receipt`は記録しない）、mainには触れない。worktreeは残る。再試行は`ready ID`、取り消しは`cancel ID`。

### errorと復旧

mainを進める前のGit・ファイル・DBのerror（worktreeがない、mainのcheckoutにローカル変更があって`--ff-only`が失敗する、など）は`abort_integration`で`integration_error`イベントと`last_error`を書き、runを着地開始時のstatus（`awaiting_integration` / `needs_session`）に戻してleaseを解放する。`integrate`は非0で終わり、原因を直して再実行する。

`integrate`プロセスが途中で死ぬとrunは`integrating`のまま、leaseはstaleになる。`doctor`が`integrating`のrunをlease付きで報告し、leaseのPIDが死んでheartbeatが30秒以上古ければ`recover RUN_ID`が`awaiting_integration`に戻す（`run_recovered`の`previous_status: integrating`）。次の`integrate`は途中のrebaseをabortしてやり直す。mainを進めた後にDBの更新が失敗した場合はrunを戻さず、error messageに着地commitを含める（[ADR-0008](../adr/0008-merge-queue-squash-landing.md)のConsequences）。

## Cleanup and recovery

workspaceの終了はsupervisorが行う。leaseはrun単位で（[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)）、1 runの復旧が他のrunに影響しない。receipt検証を通った`awaiting_integration`のrunだけが対象で、workspaceだけを閉じ、worktreeとbranchは`integrate`が着地するまで残す（着地後に`integrate`が削除する）。`failed`（非0終了、検証拒否）、provisioningや検証処理のエラー、wrapper heartbeat切れの場合はworkspaceもworktreeも調査のため残し、closeを呼ばない。

cmux 0.64.25の`workspace create --command`はコマンドをログインシェルに打ち込む形で起動し、wrapperが終了してもシェルとworkspaceは残る（[010](../journal/010-failure-path-smoke.md)の実機確認。[015](../journal/015-e2e-happy-path.md)が観測した「終了後1〜2秒で自動的に閉じる」挙動は010の環境では再現せず、cmuxの設定に依存するとみられる）。どちらの場合もsupervisorの手順は同じで、先にworkspaceが消えていれば`cmux workspace close`は`not_found`で失敗して`cleanup_failed`になり、wrapper終了直後の`read-screen`も`screen_capture_failed`になりうる。`failed`・`interrupted`のrunのworkspaceは誰も閉じないので、調査が済んだらmaintainerが`show`の`workspace_id`を`cmux workspace close`に渡して閉じる（`doctor`は未完了runしか列挙しないので`failed`・`interrupted`のrunは出ない）。

closeの成否は`task_runs.workspace_closed_at`で表す。nullは「閉じたことを確認していない」で、closeの失敗だけでなく、cmuxが閉じた後にDBへ書けなかった場合も含む。closeの失敗は`cleanup_failed`イベントと`last_error`に残るが、run状態は変えない。閉じていないworkspaceをcleaned扱いにせず、再試行は`doctor`/`recover`（[009](../journal/009-doctor-recover.md)）で扱う。

### backendの呼び出しの失敗

`WorkspaceBackend`（cmux adapter）の呼び出しが失敗するかtimeoutすると（cmuxはどの呼び出しも30秒、`WorkspaceBackend::call_timeout`）、runtimeは`backend_call_failed`をrun_eventsに記録する（task 109）。負荷が高いとcmuxが詰まることをqueueに残し、observerが`stats`の`backend_failures`から並列度の見直しを根拠つきで提案できるようにするためで、記録するのはruntime、observerは`stats`を読むだけ。

- **payload**: `op`（`create` / `create_named` / `capture` / `close` / `send_exit`（`send`と`send-key`）/ `exists` / `ensure_group`。`ask`の`notify`は記録しない（[人への通知](#人への通知cmux-notify)）。起動時の`preflight` / `preflight_detached`はcmuxに繋がるかの確認で、失敗すればコマンド自体が止まるので記録しない）、`workspace_id`（無い呼び出しはnull）、`timeout_secs`、`error`（先頭300文字）、`load_avg`（getloadavg(3)の1分値。取れなければnull）、`slots`、`parallel`。
- **run**: runのための呼び出し（`create`、runの開いたworkspaceへの`capture` / `close` / `send_exit`）はそのrunのイベントとして`task_id`と`run_id`を持つ。workspaceからrunを引けない呼び出し（`up`のmaintainer / supervisor workspaceの`create_named`と`exists`、queueのworkspace groupの`ensure_group`、`down`のsupervisor workspaceの`close`）は`task_id`も`run_id`も持たない（0012で`run_events`のCHECKがこのkindだけに認める）。
- **slots / parallel**: supervisorの呼び出しはそのsupervisorのtokenのlease数（握っているslot）と`--parallel`。`up` / `down`の呼び出しは全leaseの数と、登録済みsupervisorの`parallel`の合計（登録が無ければnull）。
- **記録する場所**: cmux adapterではなくapplication層の`runtime::RecordingBackend`（`WorkspaceBackend`を包むdecorator）が、`supervise`・`up`・`down`で渡されたbackendを包んで記録する（[ADR-0013](../adr/0013-layered-architecture-and-type-function-style.md)）。記録は自前の接続で書き、書けなくても呼び出し元へ返すエラーは元のまま。needs_sessionのresume（[`needs_session`](#needs_session)）の`create_resume`（runに記録）、`send_text`・`send_exit`・`close`・`exists`・`capture`も同じ経路を通る。resume workspaceのIDは`task_runs.workspace_id`に無いので、それらの失敗はrunを持たない記録になる（`create_resume`はrunに付く）。
- 既存の記録はそのまま残す: closeの失敗は`cleanup_failed`、wrapper終了後の`read-screen`の失敗は`screen_capture_failed`で、どちらも同じ失敗を`backend_call_failed`としても記録する（呼び出しの直後なので`backend_call_failed`が先）。`/exit`後にsessionが終わらない`exit_request_timed_out`はcmuxの呼び出しの失敗ではないので`backend_call_failed`にならない（`backend_call_failed`になるのは`/exit`の送信（`send_exit`）そのものが失敗かtimeoutしたときだけ）。`create`の失敗（provisioningの失敗）と`/exit`の送信失敗はrunをabandonするが、`backend_call_failed`はabandonの`runtime_error`より前に入る。

supervisorの再起動ではrunごとのleaseとheartbeatを確認し、孤児プロセスを勝手に再実行しない。wrapperが生きている（heartbeatが30秒以内か`exited_at`記録済み）`running` / `validating`のrunだけは、staleなleaseごと次のsupervisorが引き継いで同じrunを続ける（[ADR-0012](../adr/0012-adopt-stale-lease-of-live-wrapper.md)、[`supervise`](#supervise)の5）。それ以外（wrapperが死んだ・黙った、`claimed` / `starting`、`integrating`、leaseなし）はユーザーが`recover`で明示的に復旧した後に新しいTaskRunを作る。

### `status`

`dagq status`はrunのprocessを調べずに登録とleaseだけを返す。引き継がれたrunは引き継いだsupervisorのtokenのleaseを持つので、他のrunと同じくそのsupervisorの`run_ids`に並ぶ。`supervisors`は`supervisors`表の登録を`started_at`順に、続けて登録のないtokenのlease保持者（着地中の`integrate`プロセス、または登録表以前のsupervisor）をlease順に並べ、leaseはtokenで登録に結び付ける。各項目は`pid`、`alive`（`kill -0`）、`registered`、`mode`と`workspace_id`、`binary_version`（そのプロセスが動いているdagqのversion。登録がなければnull、列より古いbinaryの登録もnull。[ADR-0014](../adr/0014-up-replaces-a-supervisor-of-another-binary-version.md)）、`parallel`と`started_at`（登録がなければnull）、`heartbeat_at`、`heartbeat_age_secs`、`stale`（PIDが死んでいるかheartbeatが30秒より古い）、`run_ids`（そのtokenのlease）。runを持たない常駐supervisorは`run_ids: []`で並ぶ。`runs`は未完了run（`claimed`/`starting`/`running`/`validating`/`integrating`）ごとに`run_id`、`task_id`、`status`、`workspace_id`、`worktree_path`（queueの今の`runs/`から解決したpath。[ADR-0017](../adr/0017-resolve-run-paths-from-the-queue-directory.md)）、`lease`（なければnull。`pid`でどのsupervisorが持つかが分かる）。`awaiting_integration`と`needs_session`はプロセスを持たないので並ばない。

`status`は続けて`attention`と`cursor`を返す（[ADR-0016](../adr/0016-maintainer-notification-and-compact-output.md)）。`cursor`はrun_eventsの最新id（空のqueueは0）で、状態を読む前に取るので、その後の遷移は`watch --after cursor`で必ず拾える（重複はありうるが取りこぼさない）。`attention`はmaintainerか人の判断で止まっているものを今の状態から導出し、supervisorを先、runを後に並べる。各項目は`run_id`、`task_id`、`status`、`kind`、`last_error`（300文字で切り詰め）、`next`（定型の短い句）で、supervisorの項目は`run_id`/`task_id`がnullで`pid`を持つ。

- supervisor: `supervisors`表の登録のうちstale（PIDが死んでいる、またはheartbeatが`HEARTBEAT_TIMEOUT_SECS`より古い）なものが`kind: supervisor_stale`（`status`は`dead`か`stale`）、登録が1件もなければ`kind: supervisor_stopped`（`status: stopped`、`pid`なし）。`next`は`restart supervisor`。lease保持者だけの`integrate`は数えない。この2つのkindはrun_eventsに書かれない導出値。
- run: `in_progress`のtaskの最新runを`domain::run_attention`で判定する。`awaiting_integration`→`review and integrate`、`needs_session`→supervisorがresume中か次の試行を始められれば（[`needs_session`](#needs_session)の末尾。`watch::resume_pending`）`resuming (runtime)`、そうでなければ`resume session`、`failed`→`inspect and close workspace`、`exit_request_timed_out`の後に`session_exited`がない`running`→`send /exit`、lease行の無い`claimed` / `starting` / `running` / `validating` / `integrating`→`recover run`（supervisorが手放したrun。[ADR-0025](../adr/0025-leaseless-unfinished-run-is-a-recover-run-attention.md)。`send /exit`より優先する）、それ以外で`prompt_waiting`の後に`prompt_cleared`も`receipt_observed`もない`running`→`answer the prompt in workspace <workspace_id>`（[ダイアログ待ちの検知](#ダイアログ待ちの検知)）。`integrate`はleaseの取得・解放を`integrating`への出入りと同じトランザクションで行うので、leaseの無い`integrating`は正常な経路では生じない。leaseがstaleなrunはここに出さない（supervisorが死んだならその項目、wrapperが生きていればadopt。死んだ`integrate`が残したleaseはどのattentionにも出ない）。leaseの有無はrunのstatusより先に確かめ直す（supervisorはstatusを動かしてからleaseを解放するので、読む間に解放されたrunを取り違えない）。`kind`はそのrunでattentionと判定された最後のイベントのkind（`recover run`は直近の`runtime_error`。無ければstatus名）、`last_error`はrunの`last_error`。taskが再試行・cancel・完了されたrunは出ない。
- push: `push_failed`を持つ`integrated`のrunのうち、その`push_failed`がqueue全体で最後の`push_finished`より後のもの（`runs_with_pending_push`）を`domain::run_attention`で`push main`にする。`kind`は`push_failed`、`last_error`はその`error`。taskは`completed`でも出る。後の`integrate`のpushが成功すればmainはそれまでの着地を含むので消える。`push_skipped`は消さない。手で`git push origin main`してもqueueには記録されないので、次の`push_finished`までは残る。
- ask（[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)、[ask / answer](#ask--answer--asks)）: closeされていないaskを`asks`表の順に並べる。未回答のものは`kind: ask_opened`、`status: open`、`next: answer ask <id>`、回答済みのものは`kind: ask_answered`、`status: answered`、`next: read the answer of ask <id> and close it`。ただし回答済みの`worker_question`は`delivering the answer of ask <id> (runtime)`か`send the answer of ask <id> to the worker and close it`（[workerの質問への回答の送信](#workerの質問への回答の送信)）。項目は`ask_id`を持ち、`run_id`はaskのrun（taskだけのaskはnull）、`last_error`はnull。

`status --role <maintainer|inbox|planner>`は`attention`をそのroleに宛てたものだけにする（省略時は全部）。宛先は`domain::attention_role(kind)`が決め、`ask_opened`はinbox、それ以外（`ask_answered`、supervisor、run、push）はmaintainer、plannerに宛てたattentionは今は無い。`asks`はroleに関わらずopenなask（未回答でcloseされていないもの）の一覧で、各項目は`id`、`kind`、`question`（先頭200文字。切ったときは末尾に`…`）、`task_id`、`run_id`、`asked_by`、`age_secs`（登録からの秒数）。

### `events` / `watch`

`dagq events --after <id> [--limit N（既定100）] [--all]`はidより後のrun_eventsを古い順に返す純粋なクエリで、既定はattentionイベントだけ、`--all`で全kind。返り値は`{events, cursor}`で、`cursor`はlimitに達したら最後に返したイベントのid、そうでなければ読んだ時点の最新id。各イベントは圧縮形で、`id`、`kind`、`task_id`/`goal_id`/`run_id`（あるものだけ）、`created_at`と、payloadから`status`（なければ`to`）、`exit_code`、`ask_id`、`reason`（`reason`/`message`/`error`のどれか、300文字で切り詰め）だけを取り、path・receipt・出力は省く。attentionイベントは`next`も持つ。

attentionイベントの判定は`domain::event_attention(kind, payload)`（候補kindは`ATTENTION_KINDS`）: `validation_finished`で`status`が`awaiting_integration`（`review and integrate`）か`failed`、`supervision_finished`と`integration_failed`で`failed`（`inspect and close workspace`）、`integration_deferred`と`integration_error`で`needs_session`（`resume session`。ただし`integration_deferred`の`resumes_left`が1以上ならsupervisorがresumeするのでattentionにしない）、`exit_request_timed_out`（`send /exit`）、`push_failed`（`push main`）、`runtime_error`でpayloadの`lease_released`が`true`のもの（supervisorのabandon。`recover run`）、`prompt_waiting`（payloadの`workspace_id`で`answer the prompt in workspace <id>`。`prompt_cleared`はattentionではない）、`ask_opened`（payloadの`ask_id`で`answer ask <id>`）、`ask_answered`（`read the answer of ask <id> and close it`。payloadの`kind`が`worker_question`なら、`runtime_delivers: true`はattentionにせず、`false`は`send the answer of ask <id> to the worker and close it`）、`ask_delivery_failed`（`send the answer of ask <id> to the worker and close it`）、`resume_finished`で`status`が`awaiting_integration`（未承認のrunが戻った。`review and integrate`）か`failed`（`inspect and close workspace`）、`needs_session`で`exhausted: true`（`resume session`）。leaseを手放さない`runtime_error`（`record_runtime_error`など、`lease_released`が無いかfalse）はattentionにしない。`push_finished`と`push_skipped`もattentionにしない。validation後の後始末（workspaceのclose）の失敗によるabandonは、runが`awaiting_integration`に着いた後でも`lease_released: true`の`runtime_error`を書くので、`events` / `watch`は`recover run`を返すが、`status`はstatusどおり`review and integrate`を出す（`status`の判定を正とする）。`integration_error`で`awaiting_integration`に戻ったものは`integrate`の呼び手がerrorを受け取っているのでattentionにしない。`integration_rebase_aborted`はstatusを変えず着地が続くので、その結果（`integration_deferred`など）の方がattentionになる。既存のkind名とpayloadは変えていない（`integration_deferred`に`resumes_left`を足しただけ）。

`dagq watch [--after <id>] [--timeout SECS（既定600）] [--interval SECS（既定2）] [--role <maintainer|inbox|planner>]`はqueueをinterval秒ごとに読み、idより後にattentionイベントが1件以上あるか、登録済みsupervisorの健全性（tokenの集合と各`pid`・`alive`・`stale`、`domain::SupervisorPulse`）がwatch開始時のsnapshotと変わるまでblockする。返り値は`{events, supervisors_changed, supervisors, cursor}`で、`supervisors`は`status`と同じ形。timeoutでは`events`が空、`supervisors_changed: false`、`cursor`は渡したままで、exit codeは0。`--after`を省くと開始時の最新idから待つ。`watch`はqueueを読むだけで何も書かず、`integrate`を呼ばない。

`--role`を渡すと、そのroleに宛てたattentionイベント（`domain::attention_role`、`status --role`と同じ）だけで起き、supervisorの健全性の変化で起きるのは`maintainer`だけ（inboxとplannerは`supervisors_changed`が常にfalse）。inboxは`watch --role inbox`で`ask_opened`を、maintainerは`watch --role maintainer`で`ask_answered`とADR-0016のattentionを受ける。cursorはrun_eventsのidのままで、askの登録と回答も`ask_opened` / `ask_answered`としてrun_eventsに書かれるのでcursorに乗る（roleの違うwatchが同じcursorを使ってよい）。`events`には`--role`は無い。

### `ask` / `answer` / `asks`

[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)の決定1。人の判断を要する相談を`asks`表の行にする（[persistence](persistence.md#asks)）。

- `dagq ask --kind <approve_landing|answer_prompt|decide|worker_question|blocked> --question <text> [--option <text>]... [--task ID | --run RUN_ID]`は相談を登録してaskを返す（`Ask`の全列と`created`）。`--run`はそのrunのtaskも決める。`--task` / `--run`のどちらかは必須で、例外は`blocked`（observerが上げる閾値超え。[ADR-0024](../adr/0024-retire-maintainer-into-jobs-and-observer.md)の決定4）だけ: taskにもrunにも紐づかない閾値（`idle_slots`、`backend_failures`）は`task_id` / `run_id`の無いaskにし、その`ask_opened` / `ask_answered`もtaskの無いrun_eventsになる（schema v16）。同じ（task、run、kind）でopenなaskがあれば登録せずにそれを`created: false`で返す（`--question`と`--option`は捨てる）。新しいaskは`ask_opened`（payloadは`ask_id`、`kind`、`asked_by`）を同じトランザクションで書き、commitの後にinboxへ`cmux notify`を1回送る（observerの`blocked`も同じ。出力に`notified`、失敗なら`notify_error`。[人への通知](#人への通知cmux-notify)）。`asked_by`はsessionの`DAGQ_ROLE`（無ければnoteの`by`と同じく`human`）。observer（`DAGQ_ROLE=observer`）は読み取りの`asks`と`status`と、`ask --kind blocked`だけを使え、それ以外の`ask`・`answer` / `ask close`は拒否される。
- `dagq answer ASK_ID --text <text>`はopenなaskに`answer`と`answered_at`を書き、`ask_opened`と同じtask / runに`ask_answered`（`ask_id`、`kind`）を書く。回答済みかcloseされたaskにはerror。
- `dagq ask close ASK_ID`は回答済みのaskに`closed_at`を書く。maintainerが回答を読んだ印で、イベントは書かない。未回答のaskはcloseできず（error）、取り下げは`answer`で取り下げた旨を書いてからcloseする。run_eventsでaskを終えるのは`ask_answered`だけで（`worker_question`の送信の`ask_delivered` / `ask_delivery_failed`は後から足したkindで、`stats`の対には使わない）、`stats`はこの2つを`ask_id`で対にして未回答のaskを数えるため、イベントなしで閉じたaskが`stats`に残り続けないようにする。回答した時点で同じ（task、run、kind）の新しいaskを登録できる。
- `dagq asks [--open] [--role <role>] [--all]`はaskを古い順に`{asks}`で返す。既定はcloseされていないもの、`--all`はcloseされたものも、`--open`は未回答のものだけ、`--role`はそのroleが今動かすもの（`Ask::waits_for`: 未回答はinbox、回答済みでcloseされていないものはmaintainer、plannerは無し）。

### `stats`

`dagq stats [--since <event id>] [--goal ID] [--full]`は、run_eventsから時間と閾値超えを導出して返す読むだけのコマンド（[ADR-0023](../adr/0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)の決定5）。新しい表は持たず、集計は`domain::stats::stats`（events、task→goalの対応、今の時刻、supervisorの空きslotのsnapshotを受ける純粋関数）が行い、`runtime::stats`はqueueを読んで渡すだけ。

- **対象のrun**: 終わったrun。終わりのイベントは`run_integrated`か、payloadの`status`が`failed` / `interrupted`になった最初のイベントで、そのidが`finished_event_id`。既定は終わった順の直近50件、`--full`で全件。`--since`はそのidがcursorより大きいrunだけにし、50件を超えるときは古い方から50件を返して`next_cursor`をその最後の`finished_event_id`にする（続きは同じ`--since next_cursor`で読める）。それ以外の`next_cursor`は読んだ時点のrun_eventsの最新id（`status`の`cursor`と同じ値）。`--goal`はそのgoalのtaskのrunだけに絞る（alertsも同じ）。
- **`runs`**: runごとに`run_id`、`task_id`、`goal_id`、`status`（`integrated` / `failed` / `interrupted`）、`finished_event_id`と、秒の区間と回数。区間は端のイベントが無ければnull。
  - `work`: `run_claimed`→最初の`receipt_observed`
  - `validate`: 最初の`receipt_observed`→最初の`validation_finished`
  - `wait_to_land`: 最初の`validation_finished`→`run_integrated`
  - `startup`: `agent_started`→`first_commit_observed`（[最初のcommitの観測](#最初のcommitの観測)。記録の無いrunはnull）
  - `resumes`: `resume_started`（ADR-0019の自動resume。記録されるまでは0）の数、`review_verdict`: 最後の`review_finished`の`verdict`（goal 11のreview工程が記録するまではnull）、`needs_session` / `failed`: payloadの`status`がその値のイベントの数。`integration_error`は試行前のstatusに戻すだけなので`needs_session`に数えない
- **`goals`と`overall`**: goalごと（goal昇順、goalの無いrunは`goal_id: null`で最後）と全体で、`runs`（件数）と区間ごとの`{count, total, median}`。区間の無いrunは数えない。中央値は偶数個なら中央2つの平均の切り捨て。
- **`alerts`**: `[{kind, task_id, run_id, value, threshold}]`（`value`と`threshold`は秒か回数）。対象のrunに加えて、まだ終わっていないrunも見る。
  - `awaiting_integration`: `wait_to_land`が15分（900秒）を超えたrun。まだ`awaiting_integration`にいるrunは最初に`awaiting_integration`になってからの経過で判定する（着地の失敗で戻っても起点は変えない）
  - `needs_session`: `needs_session`が3回目に達したrun
  - `ask_unanswered`: `ask_opened`から60分答えられていないask（ADR-0022。`ask_answered`とはpayloadの`ask_id`（無ければ`id`）とrun・taskで対にする）
  - `task_failed`: 同じtaskのrunの`failed`が合わせて2回。回数はpageに関係なく全runで数え、`run_id`はその最後に失敗したrunで、そのrunが対象に入るときに出す（`--since`で2回目だけが新しくても出る）
  - `work_over_median`: `work`がそのgoal（goalの無いrunはgoalの無いrun同士）の中央値の2倍を超えたrun
  - `idle_slots`: staleでないsupervisorの`parallel`の合計から実行中（`integrating`以外の未完了）runを引いた空きslotがあるのに、candidatesがゼロで`ready`のtaskが残っている（依存で詰まっている）。`value`は空きslot数で、`task_id` / `run_id`はnull。readyのtaskが無い空のqueueは詰まりではないので出さない。draftのgoalに属するreadyのtaskは`goal ready`を待っているだけなので数えない。`stats`を読んだ時点のsnapshotで判定し、時間帯の履歴は持たない
  - `backend_failures`: 同じwindowの`backend_call_failed`が2件以上。`value`は件数、`task_id` / `run_id`はnull
- **`backend_failures`**: `{count, by_op, max_load_avg, max_slots}`。`backend_call_failed`の件数、`op`ごとの件数、記録された`load_avg`の最大（無ければnull）、`slots`の最大（無ければnull）。windowは`--since`があればcursorより後から`next_cursor`まで、無ければ対象のrunの最初のイベントのうち最も古いもの以降（`--full`か対象のrunが無ければ全件）。`--goal`はそのgoalのrunの失敗だけを数える（runの無い失敗は数えない）

### `doctor`

`dagq doctor`は状態を変えずにJSONで報告する。以下は`doctor --full`の内容で、既定の出力はsupervisor 1件・run 1件につき1行相当に圧縮する（ADR-0016の決定4。キー名は変えず省くだけ）: supervisorは`pid`、`alive`、`registered`、`mode`、`workspace_id`、`binary_version`、`heartbeat_age_secs`、`stale`、`run_ids`、runは`run_id`、`task_id`、`status`、`lease_stale`（leaseがなければnull）、`recoverable`、`blocker_count`（`blockers`の件数）、`workspace_id`、`worktree_path`（`RunHealth::summary` / `SupervisorHealth::summary`）。

- `supervisors`: `status`と同じ。staleな登録は報告するだけで、`doctor`も`recover`も`integrate`も消さない。
- `runs`: `claimed`/`starting`/`running`/`validating`/`integrating`のrunごとに、`workspace_id`、worktreeとrun directoryとreceiptの存在、`last_error`、そのrunの`lease`（PID、`kill -0`による生存、heartbeatの経過秒数、30秒を超えた`stale`。なければnull）、登録済みwrapper/agentプロセスのPID・生存・heartbeat経過秒数・終了コード。`exited_at`が記録済みのプロセスはPIDが再利用されうるため生存確認せず`alive: null`にする。
- `blockers`: そのrunの`recover`を拒む理由の一覧。そのrunのprocessとleaseだけを見る。空なら`recoverable: true`。

cmux workspaceの存在は確認しない（cmuxなしで動く）。IDを見てユーザーが`cmux workspace list`で確認する。

### `recover RUN_ID`

1. runが`claimed`/`starting`/`running`/`validating`/`integrating`でなければ拒否する。
2. `doctor`と同じ確認を行い、未終了として登録されたプロセスのPIDが生きている、そのrunのleaseのheartbeatが30秒以内、leaseのPIDが生きている、のいずれかなら拒否する。heartbeatが止まったまま生きているsupervisorのleaseを`recover`は奪わず、ユーザーが止める（wrapperが生きていれば[adopt](#supervise)が引き継ぐ）。supervisorがabandonしたrunはleaseがないので、processが止まれば復旧できる。`recover`が扱うのは、引き継ぎの条件を満たさないrun: wrapperが死んだか30秒以上黙っている、`claimed` / `starting`、`integrating`、leaseのないrun。
3. `BEGIN IMMEDIATE`の中でそのrunのleaseが新鮮でないことと`run_processes`の行数が確認時と同じことを再検査し、runを`interrupted`（`integrating`なら`awaiting_integration`: 検証済みの成果は残っており、次の`integrate`が途中のrebaseをabortしてやり直す）にし、確認した内容を`run_recovered`イベント（`previous_status`、`status`、`lease_deleted`、`run`）に記録し、そのrunのleaseだけを削除する。

他のrun、そのlease・process、`run_processes`、worktree、branch、workspace、run directoryは触らない。Taskは`in_progress`のまま残る。再試行は`ready ID`（編集するなら`draft ID`）で行い、動いているsupervisor（または次のsupervisor）が新しいTaskRunと新しいworktreeを作る。`recover`はTaskを`ready`に戻さない: 復旧と再実行は別の判断であり、`failed`で止まったTaskの再試行と同じ経路にまとめるため。

### `rebind`

`dagq rebind [--repo REPO]`はrepositoryを移動した後に、開いたqueueの`queue_repository`を`--repo`（既定はcwd）の`GitRepository::inspect`が返すcanonicalなcommon directoryに付け替える（[ADR-0020](../adr/0020-rebind-queue-to-a-moved-repository.md)）。repositoryから解決したqueueでopen直後の`assert_repository`を通らない唯一のコマンドで、`bind_repository`を使う`init`・`supervise`は今までどおり別のrepositoryを拒否する。

1. **拒否**: `supervisors`の登録のうちPIDが生きているものがあれば失敗する（heartbeatの古いhungも含む。PIDの死んだ登録は無視する）。`integrating`のrunのleaseのPIDが生きていれば（着地中の`integrate`）失敗する。どちらも束縛は変えない。
2. **付け替え**: `rebind_repository`が1トランザクションで旧値を読み、新しいcommon directoryをupsertする。旧値と同じなら`outcome: unchanged`。
3. **記録**: 変わったときだけ`<queue dir>/logs/rebind.jsonl`に1行追記し、`<queue dir>/repository`があれば新しいpathに書き換える。`run_events`には書かない。
4. **worktreeのrepair**: runのうちworktree（queueの今の`runs/`から解決したpath）が残っているものに、新しいrepositoryの主working treeで`git worktree repair <worktree>`を実行する。main working treeの移動で壊れた`.git`ファイルが直る。失敗は`worktrees[].error`に出すだけで`rebind`は成功する。
5. **出力**: `previous_git_common_dir`、`git_common_dir`、`db`、`queue_dir`、`repository_queue_dir`（新しいcommon directoryから解決されるqueueディレクトリ。data homeが決まらなければnull）、`move_to`（それが`queue_dir`と違うときだけ。data homeが決まらないときもnull。行き先が既にあるかは見ないので、手順で「存在しないこと」を求める）、`worktrees`。
