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
  - adr-0009
  - adr-0010
  - adr-0011
  - adr-0012
  - adr-0014
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
      → worktree and branch removed; history kept at refs/taskq/runs/<run-id>
    conflict / failed re-validation → needs_session
      → the maintainer resumes a session in the worktree; it resolves, reruns verification,
        rewrites the receipt → integrate ID again (failed receipt → run failed)
  → dependents become candidates; the resident loop claims them from the landed main
```

## Implementation status

ステップ3で`claim`から`running`、セッション終了検知までを、ステップ4の[005](../journal/005-receipt-validation.md)でreceiptの検証と`awaiting_integration`への遷移を、[006](../journal/006-workspace-close.md)で受理後のworkspace終了を、[007](../journal/007-session-exit-request.md)でreceipt受領後の終了要求を、[009](../journal/009-doctor-recover.md)で`doctor`/`recover`を、[008](../journal/008-integration-confirm.md)で統合確認`integrate`と`completed`への遷移を`src/runtime.rs`に実装した。ステップ6の[017](../journal/017-parallel-runs.md)（[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)）でleaseをrun単位にし、`supervise`を上限付き並列の常駐ループにした。ステップ7の[018](../journal/018-merge-queue.md)（[ADR-0008](../adr/0008-merge-queue-squash-landing.md)）で`integrate`を手動mergeの確認からruntimeによる着地（rebase → 再検証 → squash）に置き換えた。[021](../journal/021-maintainer-up-down.md)で`up` / `down`（`src/lifecycle.rs`）、launchdによる常駐、`supervise --log-dir`、maintainer promptを足した。task 24（[ADR-0012](../adr/0012-adopt-stale-lease-of-live-wrapper.md)）で、supervisorが死んだ後もwrapperが生きているrunを次のsupervisorが引き継ぐ（adopt）ようにした。[ADR-0011](../adr/0011-cmux-socket-password-and-in-cmux-fallback.md)のtask 21で`up`にcmux外接続のpreflightを、task 22で`up --in-cmux`（launchdなしのfallback）と`supervisors.mode`を足した。task 30（[ADR-0014](../adr/0014-up-replaces-a-supervisor-of-another-binary-version.md)）で`supervisors.binary_version`を足し、`up`がversionの違うliveなsupervisorをdrainして入れ替えるようにした（`--no-wait`は走行中のrunがあれば入れ替えない）。

## Roles

- **supervisor**: runtimeの`supervise`プロセス。taskをclaimし、runごとにworktreeとworkspaceを作って監視し、receiptを検証する。`up`がlaunchdのLaunchAgentとして常駐させるか（既定）、`--in-cmux`なら`taskq <repo> supervisor` workspaceの中で動かす。
- **maintainer**: 常駐のClaude Code session（旧称SV / operator）。登録・監視・レビュー・着地を行う。`up`が`taskq <repo> maintainer`のcmux workspaceで、runtimeが生成した初期prompt付きで起動する。CLIの使い方はpluginの`taskq-maintain` skillが持つ。
- **worker**: run session。runごとのcmux workspace `taskq <repo> <task-id> <run-id>`で動くClaude session。

runtimeの中で人が打つ`/exit`や復旧を指す語はすべてmaintainerに寄せた（`src/`に`operator`は残らない）。

## `up` / `down`

`cmux-taskq up [--parallel N] [--in-cmux] [--plugin-dir PATH] [--repo PATH] [--cmux EXE] [--claude EXE]`はqueueのruntimeをcold startする1コマンドで、`src/lifecycle.rs`の`up`が行う。冪等で、続けて2回叩けば2回目は全部`reused` / `skipped`になる。外部（launchctl、cmux、PIDの生存とsignal）は`LaunchAgent`、`WorkspaceBackend`、`ProcessControl`のtrait越しに呼び、`tests/lifecycle.rs`はfakeで判定を、`tests/e2e.rs`は実launchdと実cmuxで`up → status → down --wait`を両方のmodeで確認する（in-cmux modeのe2eはsocket passwordを要らないので、`cmuxOnly`のままでも通る）。

1. **preflight**: queueが`init`済み（DBが存在する。`--db`がなければcwdのrepositoryから解決）、repositoryのroot、cmux（`ping`）、Claude（`--version`）。`--plugin-dir`は絶対pathに正規化する。
2. **stale登録の削除**: `supervisors`表のうちPIDが死んでいる行を`deregister_supervisor`で消し、消したtokenとpidを結果の`pruned_supervisors`に出す。`run_leases`は触らない（そのrunの復旧は`doctor` / `recover`の仕事）。PIDが生きていてheartbeatが30秒より古い登録（hang）は消さず、reuseもしない。
3. **supervisor**: 生きていてheartbeatが新しい登録があり、その`binary_version`が全部`up`自身のversion（`cmux_taskq::VERSION` = `CARGO_PKG_VERSION`）と同じなら`{"outcome":"reused","mode":…,"version":…,"pid":…}`で、plistにもlaunchctlにもcmuxにも触らない（下記のcmux外接続のpreflightもしない。`mode`はその登録に記録されているものをそのまま返し、手で起動したsupervisorはnullのまま）。1つでもversionが違えば**入れ替える**（下記）。liveな登録が無ければ`--in-cmux`の有無でmodeが決まる。
   - **launchd mode（既定）**: まずcmux外接続のpreflight（下記）を通し、それからLaunchAgentを書いて起動し、登録が現れるまで（30秒）待って`{"outcome":"started","mode":"launchd","pid":…,"workspace_id":null,"plist":…}`。
   - **in-cmux mode（`--in-cmux`）**: launchdには一切触らず（plistを書かず、launchctlも呼ばない）、cmux外接続のpreflightもしない（supervisorはcmuxのterminalの子になるので、socket passwordが要らないのがこのmodeの目的）。`taskq <repo> supervisor`という名前のworkspaceが既に開いていれば、そのIDと`cmux workspace close <id>`を挙げたerrorで止まる（cmuxはcommandが終わってもworkspaceを閉じないので、crashしたsupervisorのworkspaceや、生きているがheartbeatの止まったsupervisor——`up`はreuseもkillもしない——のworkspaceが残る。どちらも人が中を見て閉じる。[ADR-0011](../adr/0011-cmux-socket-password-and-in-cmux-fallback.md)のConsequences）。開いていなければ`cmux workspace create --name "taskq <repo> supervisor" --cwd <repository root> --command "<up自身の絶対path> --db <db> supervise --parallel N --log-dir <queue dir>/logs --cmux <resolved> --claude <resolved>" --focus false`で作り（`shell_join`で1引数ずつquoteする。argvはlaunchd modeの`ProgramArguments`と同じ`supervise_arguments`）、`identify`でUUIDを得て、登録が現れるまで（30秒）待って`{"outcome":"started","mode":"in_cmux","pid":…,"workspace_id":…,"name":"taskq <repo> supervisor","plist":null}`。登録が現れなければerrorで止まり、文面はそのworkspace IDを挙げる（画面を読んでから閉じる）。launchdの`KeepAlive`に相当するものはないので、このmodeのsupervisorが死んでも何も再起動しない。人が`up --in-cmux`を打ち直す。
   - **待つ相手**: `wait_for_registration`は`up`が起動したsupervisorの登録だけを受け取る。2で見た時点で既に表にあったtoken（PIDが生きていてheartbeatの古い「生きているが黙っている」登録を含む。`up`はこれをpruneもreuseもしない）は除外する。除外しないと、待っている間にその古いsupervisorがheartbeatを再開したときにそれを自分が起動したものと取り違え、modeと新しいworkspace IDを別プロセスの行に書いてしまう（`supervisors`は`started_at`順なので古い行が先に出る）。その後の`down`は古いsupervisorにSIGINTを送りながら、新しいsupervisorがまだ動いているworkspaceを閉じることになる。
   - **modeの記録**: 起動したsupervisorが登録に現れた直後に、`up`が`supervisors`の行へ`mode`（`launchd` / `in_cmux`）と、in-cmux modeなら`workspace_id`を書く（`set_supervisor_mode`。schema v9）。書くのは`up`だけで、`supervise`自身は書かない。modeはプロセスの性質なので登録行と寿命を共にし、supervisorがgracefulに終われば行ごと消える（queue dirのsidecar fileにしなかった理由は[persistence](persistence.md)のRuntime ownership）。`status` / `doctor`は`supervisors[].mode`と`workspace_id`として出し、`down`はこれを見てSIGTERM（launchd mode / 手起動）とSIGINT＋workspace close（in-cmux mode）を使い分ける。
   - **versionの違うsupervisorの入れ替え**（[ADR-0014](../adr/0014-up-replaces-a-supervisor-of-another-binary-version.md)）: 登録の`binary_version`は`supervise`プロセス自身が`register_supervisor`で書いた自分のversionで、列（schema v10）より古いbinaryの行はnull。`up`はliveな登録のどれか1つでも自分のversionと違えば（nullを含む）、live全部をdrainしてから1つ起動し直す。停止は`down --wait`と同じ順序: LaunchAgentを必ず外し（bootoutがSIGTERMを運び、同時に`KeepAlive`が古いbinaryを即座に立て直すのも止める）、`mode`が`in_cmux`の登録にはSIGINT、launchdがsignalしなかったプロセス（手起動、またはagentのPIDでないもの）にはSIGTERMを送り、その登録が全部消えるかPIDが死ぬまで`poll`ごとに待ち、`in_cmux`のworkspaceを閉じる（`down`と同じ`close_supervisor_workspaces`を`SeenThrough`で呼ぶ。閉じておかないと新しいin-cmux supervisorが同じ名前を取れない）。それから通常の起動経路に入る（起動し直すmodeは入れ替えられる側のmodeではなく、その`up`が指定されたmode）。launchd modeで起動し直すときのcmux外接続のpreflightは、drainより**前**に1回だけ通す（`prove_detached_cmux`）: 動いているsupervisorをdrainした後で新しいsupervisorが起動できないと分かるのでは、queueを serve するものが何も無くなる。拒まれたら何もsignalせず、plistも触らず、いつもの文面（`DETACHED_CMUX_HINT`）で止まる。待つ相手の除外集合はdrainの後に取り直すので、生き残った「生きているが黙っている」登録を自分の起動したものと取り違えない。結果は`{"outcome":"restarted","version":…,"previous_version":…,"replaced":[{"token","pid","mode","workspace_id","version"}],"supervisor_workspaces":[…]}`に、起動したsupervisorの`mode` / `pid` / `token` / `workspace_id` / `plist` / `log_dir`が並ぶ。drainに上限は置かない（runはClaude sessionなので待ち時間はrunの長さそのもの）。`up --no-wait`は待たないための逃げ道で、`active_runs`（`claimed` / `starting` / `running` / `validating` / `integrating`）が1件でもあれば件数とrun idを挙げたerrorで止まる。判定はsignalもuninstallも何もする前に行うので、止まったときの状態は`up`を打つ前と同じ（PIDの死んだ登録のprune（2）だけは済んでいる）。走行中のrunが無ければそのまま入れ替えるが、`--no-wait`のときはdrainの待ちにも上限（`startup_timeout`、既定30秒）が付く: runが無くても止まらないsupervisorはありうる（loopがcmuxやgitでhangしていてもheartbeat threadは別なので登録は新しいまま、判定とsignalの間にrunがclaimされることもある）ため、上限を超えたら残っているtokenとpidを挙げたerrorで止める（停止は既に頼んであり、agentも外れているので、`status`から消えたら`up`を打ち直す）。
   - **入れ替えの対象はliveな登録だけ**: PIDが生きていてheartbeatの止まった「生きているが黙っている」supervisorは、従来どおりreuseもpruneもkillもしないので、それが古いbinaryでも入れ替えられず、`up`はその隣に新しいversionのsupervisorを立てる。`status`がstaleとして報告するので、人が`down --force`で止めてから`up`をやり直す（[ADR-0014](../adr/0014-up-replaces-a-supervisor-of-another-binary-version.md)のConsequences）。
   - **起動できないと分かる条件はdrainより前に確かめる**: launchd modeならcmux外接続のpreflight（上記）、in-cmux modeなら`taskq <repo> supervisor`という名前が空いているかどうか（`ensure_supervisor_workspace_free`）。名前を握っているworkspaceがこの入れ替えで閉じる登録（`live`の`in_cmux`）のものでなければ、そのIDと`cmux workspace close <id>`を挙げてdrainの前に止まる。`up`はpruneのときworkspaceを閉じないし、cmuxはcommandが終わってもworkspaceを閉じないので、登録の無いcrash済みsupervisorのworkspaceが名前を握っていることがある。
   - **drainがPIDの死で終わったとき**: 登録が消えるのではなくPIDが死んで待ちが終わることがある（launchdの`ExitTimeOut`によるSIGKILL、heartbeat失敗であえて行を残す経路）。その登録行は`down --force`と同じようにここで消す。残すと、直後に閉じたworkspaceを指す行が生き残り、次の`down`が同じcloseをやり直して`close_failed`を報告する。
   - cmux外接続のpreflight: launchdが起動するsupervisorはcmuxのterminalの子プロセスではなく、cmuxはそういうプロセスをsocket passwordでしか受け入れない。`up`はplistを書く前に`WorkspaceBackend::preflight_detached(SupervisorEnvironment)`で、plistが持たせるのと同じ環境（`SupervisorEnvironment`: `PATH`と、`up`を叩いたshellが`CMUX_SOCKET_PASSWORD`をexportしていればそれ）で`cmux ping`を実行する。その際、`up`自身が今いるcmux sessionから継承した`CMUX_*`の環境変数（`CMUX_SOCKET_CAPABILITY`、`CMUX_WORKSPACE_ID`、`CMUX_SOCKET_PATH`など）はすべて外す（`adapters::detach`）。継承した`CMUX_SOCKET_PASSWORD`も一度外し、shellがexportしていた値だけを載せ直す。環境を外すだけでは足りない: cmux 0.64.25は接続元をプロセスの系譜で判定していて、cmuxのterminalの子孫なら環境変数がどうであれ通し、launchd配下なら拒む（`CMUX_*`を全部外した`cmux ping`はcmux内の子プロセスとしてはPONGを返し、同じ環境でlaunchdから起動したsupervisorは拒まれた。実機確認）。そこで`Cmux::preflight_detached_within`はpingをcmuxのプロセスツリーの外で走らせる: 外側の`/bin/sh`（`setsid`で`up`のsessionと端末からも切り離す）が内側の`/bin/sh`をbackgroundで起動して即終了し、launchd（pid 1）が内側を引き取る。内側は外側のpidが消えるまで（`kill -0`）待ってから（cmuxが系譜を見る時点で親がlaunchdになっているように）、自分のpidを`pid=N`として出力し、`exec cmux ping`になる。runtimeはpid行の後の出力がPONGであれば通し、そうでなければstderr（cmuxの拒否文）を`DetachedRefusal`のerrorにする。pingを起動できない・pidが読めない・期限内に終わらない場合は`DetachedRefusal`ではない普通のerrorで、`up`はこれをpasswordの案内にはつなげず「cmux could not be asked …」として止める。cmuxのCLIは`CMUX_SOCKET_PATH`なしだとsocketを自分で探し、0〜11秒かかった（見つかると`last-socket-path`に覚えて次は即答）ので、期限は60秒（`DETACHED_PING_TIMEOUT`）で、過ぎたらそのpidにSIGKILLを送る。この孤児のpingが実機（cmux 0.64.25、`cmuxOnly`）で拒まれることは`tests/e2e.rs`の`up`/`down`で確認した（`up`がplistを書く前に上記の文面で止まる）。`tests/lifecycle.rs`はstubのcmuxで、届いた環境に`CMUX_SOCKET_PASSWORD`以外の`CMUX_*`がないこと、`PATH`がplistのものであること、stubの親がpid 1であること、拒否・変な応答（`DetachedRefusal`）・hang（そうでないerror、pidはkillされる）を確認する。cmuxが拒めば`up`はerrorで止まり、plistは書かず、launchctlも呼ばず、maintainer workspaceも作らない。errorの文面（`lifecycle::DETACHED_CMUX_HINT`）は拒否の事実と3つの対処（cmuxのSettingsにsocket passwordを保存する。cmuxのCLIはそれを自分で使う / `up`を叩くshellで`CMUX_SOCKET_PASSWORD`をexportする / `up --in-cmux`を使う。自動再起動がないことも書く）を載せ、末尾にcmuxのerrorをそのまま付ける。
   - plist: `~/Library/LaunchAgents/com.cmux-taskq.<queue hash>.plist`。`Label`は同名、`ProgramArguments`は`[up自身の絶対path, --db <db>, supervise, --parallel N, --log-dir <queue dir>/logs, --cmux <resolved>, --claude <resolved>]`（cmuxとclaudeは`up`がpreflightした実行ファイルの絶対path。launchdのPATHに頼らないため）、`WorkingDirectory`はrepository root、`EnvironmentVariables`は`PATH`（`up`を叩いたshellのPATH）と、そのshellが`CMUX_SOCKET_PASSWORD`をexportしていたとき（空でないとき）だけ`CMUX_SOCKET_PASSWORD`（Settingsに保存したpasswordは読まないし書かない）、`KeepAlive` true、`RunAtLoad` true、`StandardOutPath` / `StandardErrorPath`は`<queue dir>/logs/launchd.log`、`ExitTimeOut` 86400（launchdの既定20秒ではdrainが待てない）。生成は`src/infrastructure/launchd.rs`の`LaunchAgentSpec::xml`。passwordを持ちうるので、plistは0600で書く。
   - 起動: `launchctl bootout gui/<uid>/<label>`（未loadなら無視）の後に`launchctl bootstrap gui/<uid> <plist>`。既にbootstrap済みでも定義を差し替えるためbootoutしてからbootstrapし直す。bootoutは即返り、serviceは旧プロセスが終わるまで`launchctl print`に残り、その間のbootstrapはexit 5で失敗する（実機確認）ので、`print`が消えるまで0.5秒ごとに待つ。60秒で消えなければ`launchctl kill SIGKILL`を送り、さらに60秒待って諦める（hangしたsupervisorがlabelを`ExitTimeOut`の間占有しないため）。
   - 登録が30秒以内に現れなければerrorで止め、agentはそのまま残す（`launchd.log`を見る）。preflightに通らない環境ではKeepAliveで再起動を繰り返すので、`down`で外す。
4. **maintainer workspace**: `up`自身の環境に`CMUX_TASKQ_ROLE=maintainer`があり`CMUX_TASKQ_QUEUE`が同じqueue DB（正規化して比較）なら`skipped`（maintainer sessionのskillから`up`を呼んでも二重にならない。cmuxには問い合わせない）。そうでなければ`cmux --json workspace list`のtitleが`taskq <repo> maintainer`（`<repo>`はrepository rootのbasename）のworkspaceがあれば`reused`、なければ`cmux workspace create --name "taskq <repo> maintainer" --cwd <repository root> --command "env CMUX_TASKQ_ROLE=maintainer CMUX_TASKQ_QUEUE=<db> <claude> [--plugin-dir PATH] -- '<maintainer prompt>'" --focus false`で作り、`identify`でUUIDを得て`created`。引数は`shell_join`で個別にquoteする（promptの改行はquoteの中に収まり、cmuxがログインシェルに打ち込んでも1コマンドになる。[010](../journal/010-failure-path-smoke.md)）。
5. **結果**: `{"supervisor": {"outcome": "started"|"reused"|"restarted", "mode": "launchd"|"in_cmux"|null, "version", "pid", "token", "workspace_id", "plist", "log_dir"}（in-cmux modeで起動したときは`name`も、`restarted`のときは`previous_version` / `replaced` / `supervisor_workspaces`も）, "maintainer": {"outcome": "created"|"reused"|"skipped", "workspace_id", "name"}, "pruned_supervisors": [{"token","pid"}], "doctor": {"unfinished_runs": [{"run_id","task_id","status","lease_stale"}], "awaiting_integration": [{"run_id","task_id","last_error"}], "needs_session": [...]}}`。`lease_stale`はleaseのPIDが死んでいるかheartbeatが30秒より古いとき`true`、leaseがなければnull。

前提: launchdが起動したsupervisorはcmuxのterminalの外で動くので、cmuxのsocketがcmux外のプロセスからの接続を受け付ける必要がある。開発環境のcmux 0.64.25は既定では「アクセスが拒否されました。cmux内で起動されたプロセスのみ接続できます」で`cmux ping`を拒み、supervisorはpreflightで落ちて登録に現れなかった（`launchd.log`にそのerrorが残り、`up`は30秒でerrorになり、KeepAliveで再起動が続いた。journal 021）。`cmux --help`のSocket Authはpasswordによる認証（`--password`、`CMUX_SOCKET_PASSWORD`、Settingsに保存したpassword）を載せている。cmuxは接続元をプロセスの系譜で判定するので（上記）、`up`はplistを書く前にcmuxのプロセスツリーの外からのpreflightを通し、拒まれたらplistを書かずに止まる。ユーザーの用意はcmuxのSettingsにsocket passwordを保存する（`~/.config/cmux/cmux.json`の`automation.socketControlMode: "password"`と`automation.socketPassword`。既定は`cmuxOnly`。推奨。plistに何も残らない）か、`up`を叩くshellで`CMUX_SOCKET_PASSWORD`をexportする（そのときだけplistに書かれる）かのどちらか。どちらもなければ`up --in-cmux`を使う（launchdを使わず、supervisorをcmux workspaceの中で動かす。自動再起動はない）。cmuxのterminalで`supervise`を手で起動して`up`にreuseさせてもよい。

前提とfallbackは[ADR-0011](../adr/0011-cmux-socket-password-and-in-cmux-fallback.md)で決めた: launchd modeはcmuxのsocket password（Settings保存、または`up`を打ったshellがexportした`CMUX_SOCKET_PASSWORD`）を前提にして`up`がcmux外からの`ping`をpreflightで確かめ（task 21）、`up --in-cmux`がlaunchdなし・自動再起動なしで`taskq <repo> supervisor` workspaceにsupervisorを起動する（task 22）。どちらも実装済み。

`cmux-taskq down [--wait] [--force] [--cmux EXE]`はsupervisorを止める。どちらのmodeで起動したものも、手で起動したものも同じ1コマンドで止まる。まずPIDの生死で登録を分け、次にLaunchAgentを必ず外す（`launchctl print`でagentの有無とPIDを読み、`launchctl bootout gui/<uid>/<label>`してplistも消す。`RunAtLoad`のため残すと次のloginで復活する。in-cmux modeに切り替えたqueueに前のlaunchd modeのagentが残っていても、これで外れる）。PIDの生きている登録が1件もなければ`{"outcome":"not_running","launch_agent_unloaded":…}`（`--force`ならPIDの死んだ登録行も消して`pruned_supervisors`に出す）。あれば登録ごとにsignalを選ぶ: `mode` が`in_cmux`ならSIGINT（cmuxのterminalでCtrl-Cを押したのと同じ。runtimeはSIGTERMと同じくdrainに入る。このmodeにはsignalを届けてくれるservice managerがない）、そうでなければ従来どおりSIGTERM——ただしbootoutでlaunchdがagentのプロセスにSIGTERMを届けるので、agentのPIDには送らない（runtimeは1回目のsignalでdispositionを既定に戻すので、2回目は即死になる）。agentがloadされているのにPIDが読めないときは誰にも送らない。既定は`{"outcome":"draining","pid":…,"pids":[…]}`で即返り、`--wait`はその登録が消えるかPIDが死ぬまで2秒ごとに待って`stopped`、`--force`はSIGKILLを送って登録行を消し`killed`（leaseは30秒でstaleになり、runは`doctor` / `recover`で扱う）。

in-cmux modeのsupervisor workspaceは`down`が閉じる。判定はどの経路でも同じで、`supervisors`の全登録（`not_running`のときの死んだ登録も含む）が対象になる。`down`がその停止を見届けたとき——`--wait`はdrainの完了を待った後、`--force`はkillした後、`not_running`は最初から誰も生きていない——は無条件に閉じる。PIDの生死は見ない: `kill(2)`は対象が回収される前に返るのでSIGKILLの直後の`kill(pid,0)`はまだ成功し（実測20/20）、`--wait`もsupervisorが登録を消してから終了するので戻った時点ではまだPIDが見えている。ここで生死を条件にすると、止めたはずのworkspaceが開いたまま`left_open`として報告される。既定の`down`だけは別で、supervisorはまだdrain中かもしれない——cmuxのworkspaceを閉じるとその中のsupervisorも終わってdrainが途切れる——ので、`down`が何もsignalする前に読んだ生死だけを見て、既に終わっていたものだけを閉じる。結果の`supervisor_workspaces`は1件ずつ`{"workspace_id","outcome":"closed"}`、`{"workspace_id","outcome":"left_open","reason":"supervisor pid N is still draining; `down --wait` closes it"}`、`{"workspace_id","outcome":"close_failed","reason":…}`のいずれか。cmuxがcloseを拒んでも`down`はerrorにしない（止めるという仕事はもう終わっている）。既定の`down`でdrain中に残ったworkspaceは、人が閉じるか`down --wait`を打ち直す（閉じないまま`up --in-cmux`を打つと、上記のとおり止まる）。`--force`は生きた登録に加えて死んだ登録も消す。閉じたworkspaceを指す行が残ると、次の`down`が同じcloseをやり直して`close_failed`を報告するため。`--cmux`はこのcloseに使う実行ファイルで、解決できなくても`down`自体は進む（閉じる相手がいなければ使わない）。maintainer workspaceは閉じない。

### Logs

`supervise --log-dir DIR`はDIRを作り、`supervisor-<started_at unix>-<pid>.log`に起動時のtoken / pid / parallel / db / repositoryと、従来stderrに出していた進行メッセージ（claim、workspace、receipt受領、終了要求、run終了、abandon、rejection）と最後の結果JSON（またはerror）を`[unix time] message`の形で追記する。stderrにも従来どおり出す（`SupervisorLog`）。`up`が作るagentは`--log-dir <queue dir>/logs`で起動し、launchdが拾うstdout / stderrは同じdirの`launchd.log`に溜まる（起動ごとのファイルはruntimeが分け、`launchd.log`は分けない。ローテーションはしない）。`locate`は`log_dir`、`label`、`launch_agent`（plistのpath。存在しなくても出す）を返す。

### Naming

cmux workspaceの名前は複数repositoryで同じcmuxを使うためrepository名を含む: workerは`taskq <repo> <task-id> <run-id>`（`run_workspace_name`。`<repo>`はrunの`repo_path`のbasename）、maintainerは`taskq <repo> maintainer`（`maintainer_workspace_name`）、in-cmux modeのsupervisorは`taskq <repo> supervisor`（`supervisor_workspace_name`。[ADR-0011](../adr/0011-cmux-socket-password-and-in-cmux-fallback.md)の決定3）。`up`はtitleの完全一致でmaintainer workspaceとsupervisor workspaceを探す。

### Maintainer prompt

`src/runtime.rs`の`maintainer_prompt(db, log_dir)`がworker promptの隣で生成する。内容: この queue（db path）のmaintainer sessionであること、役割名の1行（supervisor / maintainer / worker）、supervisorのlog dir、taskq pluginの`taskq-maintain` skillで`status`と`doctor`を確認し、staleなsupervisor・未完了run・`awaiting_integration`・`needs_session`を報告してユーザーの指示を待つこと、skillが無ければそう報告すること、queue DBを直接触らずCLIだけを使うこと。CLIの手順はpromptに書かずskillに置くので、skillを変えてもpromptは変わらない。

## `supervise`

`cmux-taskq supervise [--parallel N] [--once] [--log-dir DIR]`はrepository内で実行する常駐ループ（通常は`up`がlaunchdで起動する。手で専用ターミナルから起動してもよい）で、依存が解けたtaskを上限N（既定4）まで同時に実行する。queueはcwdから解決し（[persistence](persistence.md)のQueue location）、repositoryのcheckoutもcwdを使う。`--db PATH`と`--repo REPO`はそれぞれの明示override（[016](../journal/016-queue-per-repository.md)）。

起動時:

1. DBのpathを正規化し、checkoutのroot、Git common directoryを取得する。DBはworktree外か、common directory配下に置く（ユーザーDIRのqueueは常に満たす）。worktreeの作成元は`repo_path`に記録したcheckout。
2. cmux（`ping`）とClaude（`--version`）のpreflightを行う。
3. queueをrepositoryに束縛する（`bind_repository`）。別repositoryに束縛済みなら開始しない。queue全体の排他はなく、同じqueueに別のsupervisorがいても構わない。
4. supervisorプロセスのtoken（UUID）を作り、`supervisors`表に自分を登録する（`register_supervisor`: token、PID、`--parallel`、`started_at`）。runを1つも持たない常駐supervisorも、この登録で`status`/`doctor`に並ぶ。続けて別スレッドで2秒ごとにそのtokenの登録と全leaseのheartbeatを1トランザクションで更新する（`heartbeat(token)`）。heartbeatの失敗はループで検知し、全runに`runtime_error`を記録してleaseと登録を残したまま終了する（プロセス終了後にstaleになる）。

ループ（1秒ごと）:

5. **adopt**（[ADR-0012](../adr/0012-adopt-stale-lease-of-live-wrapper.md)）: active runが上限未満なら、claimの前に、他のtokenのleaseを持つ`running` / `validating`のrunを`runs_leased_by_others`で読み、leaseがstale（pidが死んでいるかheartbeatが30秒より古い）で、wrapperが生きていてheartbeatが30秒以内か`exited_at`が記録済みのものを`adopt_run`で引き継ぐ。`adopt_run`は`BEGIN IMMEDIATE`の中でstatusとstaleを再検査し、lease行の`token` / `pid` / `heartbeat_at`と`task_runs.supervisor_token`を自分のものにして`run_adopted`を書く（同じrunを2つのsupervisorが取ろうとしても1つしか通らない。負けた方は何もしない）。引き継いだrunのslotはDBから組み立てる: pathは`run_planned`のもの、`receipt_seen`はreceiptファイルと`receipt_observed`イベントの有無、`exit_requested`イベントがあれば`/exit`を再送せずtimeoutをいまから数え直し、wrapperの登録待ちは持たない。`validating`のrunは9の検証をはじめから行う。`claimed` / `starting`（wrapperの登録にclaimしたtokenが要る）、leaseのないrun（abandon済み・`recover`済み）、`integrating`、wrapperが死んでいるか黙っているrunは引き継がず、[`recover`](#recover-run_id)に残す。引き継ぎはlogに1行で残す。
   **claim**: 続けて、`candidates`が空でなければ、`refs/heads/main`を読み直してbase commitにし、`claim_for_supervisor`でrun・`supervisor_token`・lease行を1トランザクションで作る。`integrate`で依存が解けたtaskは次のループで、先行taskを含む`main`から始まる。
6. **provision**: run管理領域（DBと同じdirの`runs/<run-id>/`）のpath、branch `taskq/<run-id>`、worktree（`runs/<run-id>/worktree`）、receipt、logのpathを`run_planned`として先にDBへ保存し、ディレクトリ、`prompt.txt`、runtimeバイナリのスナップショット`runner`、worktreeを作り、cmux workspaceを`--name "taskq <repo> <task-id> <run-id>" --cwd worktree --command '<runner> --db ... session --run ... --lease <token> --claude ...'`で作成して、`identify`で解決したUUIDを`workspace_created`として保存する。wrapperにはDBのpathを`--db`で明示的に渡す。provisioningの失敗は環境要因とみなし、そのrunをabandon（下記）した上で以後のclaimを止め、active runをdrainしてから非0で終了する。`prompt.txt`の内容は下記[Prompt](#prompt)。
7. **監視**: 各tickの先頭で、そのrunのlease行がまだ自分のtokenであることを確認する。なければ（別のsupervisorが引き継いだ、または`recover`された）そのrunをslotから外し、DBには何も書かず結果の`errors`に載せる。tickの途中でleaseを失ってlease付きの書き込みが失敗した場合も同じで、abandonしない（`last_error`を書かない）。検証threadが動いていればそのまま終わらせる（結果は記録されない。引き継いだ側が検証をやり直す）。続けてrunごとの`SessionWatch`が、wrapperの登録（45秒以内）、wrapper heartbeat（30秒以内）、receiptファイルの出現、idle marker、wrapperの終了を確認する。receiptの出現は`receipt_observed`（`validated: false`）として記録するだけで、セッション終了とは別に扱う。receipt観測後にidle markerがreceiptより新しければ`session_idle_observed`を記録し、`WorkspaceBackend::send_exit`で一度だけ終了を要求して`exit_requested`を記録する（下記）。wrapperが`exited_at`を記録済みのsession（自分で終わった、maintainerが`/exit`を打った、引き継ぐ前に終わっていた）には終了を要求しない。
8. wrapper終了後に画面を`terminal-final.txt`へ保存し、`supervision_finished`でrunを終了コード0なら`validating`、それ以外なら`failed`にする。非0のときは同じトランザクションで`last_error`に`session exited with code N`を書き、`show`だけで理由が分かるようにする。Taskは`in_progress`のまま残す。
9. `validating`のrunはreceipt検証（下記）をrunごとのthread（専用SQLite接続）で行い、ループは完了を待ちながら他のrunを監視し続ける。完了したら`validation_finished`でrunを`awaiting_integration`または`failed`にする。
10. `awaiting_integration`になったrunだけ`cmux workspace close <workspace_id>`でworkspaceを閉じ、`OK workspace:N`の応答を確認して`workspace_closed`（`task_runs.workspace_closed_at`）を記録する。worktreeとbranchは統合まで残す。closeが失敗したら`cleanup_failed`イベントと`last_error`に記録し、runは`awaiting_integration`、`workspace_closed_at`はnullのままにする。
11. `awaiting_integration`または`failed`になったrunのleaseを解放する（`lease_released`）。
12. active runがなく、`--once`か停止要求（下記）か、provisioning失敗でclaimを止めていればループを抜ける。それ以外はactive runがない間2秒ごとに`candidates`を見る。ループを抜けたら（claimやGitのエラーで抜ける場合も含む）自分の登録を消す（`deregister_supervisor`）。heartbeat失敗で終わるときだけは消さない。

結果は`{"outcome": "finished" | "stopped", "runs": [休止したrun], "errors": [{run_id, task_id, message}]}`。SIGINT/SIGTERMは1回目でclaimを止めてactive runの終了を待ち（graceful drain）、2回目で既定の動作（即終了）になる。即終了した（killされた）supervisorのleaseはPIDが死んだ時点で（遅くともheartbeatの30秒で）staleになり、wrapperが生きているrunは次のfill passで別のsupervisorが引き継ぐ（5）。登録はPIDが死んだ時点から`stale`として`status`/`doctor`に残る。`status` / `doctor` / `recover` / `integrate`は登録を消さず、次の`up`がPIDの死んだ登録だけを消す（[`up` / `down`](#up--down)）。

### Prompt

`prompt.txt`は`src/runtime.rs`の`prompt(task, run, goal, predecessors, siblings)`が生成するclaim時点のスナップショットで、run中にqueueが変わっても書き換えない。goalの`goal edit`も、兄弟taskの状態変化も、走行中のrunには届かず、次のclaimのpromptから反映される。task単体の情報（ID、run ID、title、description、acceptance、verification_commands）、receiptの契約（pathとJSONの形）に加えて、[ADR-0009](../adr/0009-goal-groups-tasks.md)の次の4節をこの順で検証コマンドとreceiptの契約の間に載せる。どれも常に書き、該当がなければ`none`にして、promptの節構成をgoal・context・依存・並列の有無で変えない。

- **Goal**: taskに`goal_id`があれば、claim時点の`TaskQueue::show_goal()`のgoalを`Goal ID` / `Goal title` / `Goal description` / `Goal acceptance` / `Goal constraints` / `Goal doc`の行で載せる。`doc`はrepository内のpathをそのまま書き、内容は読まない（なければ`Goal doc: none`）。goalのないtaskは`Goal: none, this task stands alone`。
- **Context**: taskの`context`が空白でなければ本文をそのまま載せ、空なら`Context: none`。
- **Predecessor tasks**: taskの直接の依存元（`task_dependencies`のpredecessor）ごとに1行、`- task <ID>: <title>; result commit <sha>; summary: <text>`。`result commit`は依存元の`integrated` runの`result_commit`（`integrate`がmainに積んだsquash commit）、`summary`はそのrunのreceipt（`receipt_path`。なければ`<run-dir>/receipt.json`）の`summary`（空白を1つに畳む。空なら`(no summary)`）。receiptが読めない・parseできない・pathが不明なら`(receipt unavailable)`、integrated runがなければ（手で`completed`にしたなど）`result commit (not landed)`と書き、いずれもprovisionを止めない。取得はqueueの読み取り専用操作`TaskQueue::predecessors(task_id)`（ID順。依存元の`Task`と`integrated` runの`Option<TaskRun>`）で、summaryの読み取りはruntime側（`PredecessorSummary::from_predecessor`）が行う。依存元がなければ`Predecessor tasks: none`。
- **Sibling tasks in progress**: `TaskQueue::tasks_in_progress()`が返す`in_progress`のtask（ID順）から自分のtaskを除き、taskにgoalがあれば同じ`goal_id`のtaskに限定したものを`- task <ID>: <title>`で並べる（`siblings_in_progress`）。goalのないtaskはgoalの有無を問わず全`in_progress` taskを見る。claimは`fill_slots`で1件ずつ順に行うので、同じpassで後にclaimされたtaskのpromptには先にclaimされたtaskが載り、その逆は載らない。`awaiting_integration`や`needs_session`のrunを持つtaskも`in_progress`なので載る。なければ`Sibling tasks in progress: none`。

4節の後に「担当はこのtaskだけ。兄弟taskの範囲を変えず、範囲外の仕事を見つけたら受け持たずにreceiptの`follow_ups`に書く」の一文を置き、receipt JSONの例に任意の`follow_ups`（`{title, description}`の配列。`Receipt::check`は配列であることだけを見る）を含める。

schemaとCLIは変えない。`tests/e2e.rs`のstubはpromptの1行目とreceipt pathの行だけを読み、`follow_ups`のないreceiptを書くので、節の追加に影響されない。

### 1 runの異常（abandon）

wrapper heartbeat切れ、終了要求のtimeout、検証処理そのもの（Git呼び出しやDB）の失敗、closeの記録失敗など、監視中のruntime errorは**そのrunだけ**を手放す: `last_error`と`runtime_error`イベント（`lease_released: true`）を書き、そのrunのlease行を削除し、status・`run_processes`・workspace・worktreeは変えない。supervisorは他のrunを続け、結果の`errors`にそのrunを載せる。leaseを消すのは、常駐supervisorが生きている間も`recover`がrunのprocessだけで判定できるようにするため。taskは未完了runで占有されたままなので二重実行にはならず、未登録のwrapperはleaseがなければ登録できずClaudeを起動しない。leaseがないので他のsupervisorも引き継がない。`show`と`doctor`で確認し、[recover](#recover-run_id)で扱う。

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
2. **終了要求**: `WorkspaceBackend::send_exit(workspace_id)`で、cmuxでは`cmux send --workspace <uuid> -- /exit`の後に`cmux send-key --workspace <uuid> -- enter`を送る。maintainerが打つのと同じ経路で、1回だけ送り、再送やプロセスのkillはしない。`exit_requested`に`timeout_secs`を記録する。
3. **終了確認**: wrapperの`session_exited`を待ち、通常どおり`supervision_finished`へ進む。要求から`WorkspaceBackend::exit_timeout`（cmuxは120秒）以内に終了しなければ`exit_request_timed_out`を記録し、runtime errorとして`supervise`を終える。runは`running`、workspace・worktree・leaseはそのまま残り、人が`/exit`を送るか`recover`（[009](../journal/009-doctor-recover.md)）で扱う。この場合もwrapperは後から`session_exited`を記録する。

maintainerの手動`/exit`はいつでも有効で、markerがない（hookが無効化されているなど）場合は従来どおり手動終了を待つ。

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

`cmux-taskq integrate ID`（taskの`awaiting_integration`または`needs_session`のrun）と`cmux-taskq integrate --next`（`awaiting_integration`のrunを検証完了の古い順に1件）は、検証済みのrunをruntimeが`main`へ着地させる操作で、maintainerがレビュー後にrepository内で実行する（[ADR-0008](../adr/0008-merge-queue-squash-landing.md)）。`--db PATH`と`--repo REPO`は明示override。repositoryは`GitRepository::inspect`で開き、common directoryが`queue_repository.git_common_dir`と一致することを要求する。

1. **スロット**: runを`integrating`にし、このプロセスのtokenで`run_leases`の行を作る（`begin_integration`、`integration_started`）。同時に`integrating`のrunは1件（`one_integrating_run_per_queue`）で、別のrunが着地中ならerror。leaseは`supervise`と同じthreadで2秒ごとにheartbeatし、`status`/`doctor`に`integrating`のrunとして並ぶ。`--next`の順序は`validation_finished`イベントのid順で、`needs_session`のrunは取らない。
2. **worktreeの前処理**: worktreeが存在し、run branch `taskq/<run-id>`をcheckoutしていること。途中のrebase（`rebase-merge` / `rebase-apply`）が残っていれば`git rebase --abort`する（`integration_rebase_aborted`）。
3. **receiptの検査**: `<run-dir>/receipt.json`がparseでき、`result`が`succeeded`で（`failed`ならrunを`failed`にして終わる。下記）、`Receipt::check`を通り、`commit`がworktreeの現在のHEADに一致する。衝突なしのrunではHEAD = `result_commit`なので検証済みのreceiptがそのまま通る。`needs_session`から戻るrunでは、セッションが新しいheadでreceiptを書き直したことの検出になる。worktreeはcleanであること。`Receipt::check`を通った時点で、読んだreceiptを`integration_receipt`イベント（`main`、receiptの`commit`、`receipt`にJSON全体。`follow_ups`を含む）に記録する。HEAD不一致・衝突・再検証の失敗で着地しなかった場合も記録は残り、`integrate`を繰り返せばその回数だけ並ぶ。`validation_finished`は検証時のreceiptしか持たないので、セッションが書き直したreceiptの`commit`、evidence、`summary`、`follow_ups`をDBが持つのはこのイベントだけ。
4. **rebase**: `git rebase --no-autostash --no-verify <main head>`（main headは着地開始時に読んだ`refs/heads/main`）。すでにmainの上にあればno-op。衝突したら`git diff --name-only --diff-filter=U`とGitの出力を取り、`rebase --abort`でworktreeを検証済みheadに戻して`needs_session`にする。成功したら`integration_rebased`（`head_before`、`head_after`）を記録する。
5. **再検証**: rebase後のHEADがmain headと異なり（同じなら「commitが残らない」として`needs_session`。変更が不要ならセッションが`failed` receiptを書く）、main headの子孫であること。`git status --porcelain --untracked-files=all`が空であること。taskの`verification_commands`を順に`/bin/sh -c`でworktree内で再実行し、出力を`<run-dir>/integrate-verify-N.log`、結果を`verification_command`イベント（`phase: integration`）に残す。1件でも非0なら`needs_session`。
6. **着地**: `git commit-tree <HEAD>^{tree} -p <main head>`で1 commitを作る。messageはtaskのtitle、receiptの`summary`（空なら省略）、trailer `Taskq-Task: <task id>` / `Taskq-Run: <run id>`。`refs/taskq/runs/<run-id>`をrebase後のHEADに向けてから、mainをcheckoutしているworktree（`git worktree list --porcelain`）があればそこで`git merge --ff-only <commit>`、なければ`git update-ref refs/heads/main <commit> <main head>`でmainを進める。
7. **完了**: 1トランザクションでrunを`integrated`、`result_commit`を着地commit、`last_error`をnull、Taskを`completed`にし、lease行を消して`run_integrated`（`result_commit`、`source_commit`、`main_before`、`history_ref`、`message`、`git_common_dir`）、`lease_released`、`task_status_changed`を記録する（`finish_integration`）。
8. **後始末**: `git worktree remove --force <worktree>`と`git branch -D taskq/<run-id>`（`worktree_removed`）。失敗は`cleanup_failed`イベントと`last_error`に残し、statusは変えない。

結果は`IntegrationOutcome`: `{"outcome":"integrated","task":…,"run":…}`、`{"outcome":"needs_session","run":…,"main":…,"reason":…}`、`{"outcome":"failed","run":…,"reason":…}`、`--next`で対象がなければ`{"outcome":"no_run_awaiting"}`。

### `needs_session`

衝突（4）と再検証の失敗（5）は`defer_integration`でrunを`needs_session`にし、理由を`last_error`、詳細（衝突ファイル、Gitの出力の末尾、rebase後のheadなど）を`integration_deferred`イベントに書いて、lease行を消しスロットを空ける。worktreeは衝突なら検証済みhead、再検証の失敗ならrebase済みのheadに置いたまま残す。runはTaskを占有し続け、`ready`/`cancel`はできない。

maintainerは`cmux workspace create --cwd <worktree> --command "claude --resume <run-id>"`でセッションを開き直し、`last_error`の理由と「mainへrebaseして解消し、検証コマンドを再実行し、新しいheadでreceiptを書き直す」指示を送る。完了を確認したら`integrate ID`で再開する。手順は1から同じで、rebaseはmainが動いていなければno-op、動いていれば再びrebaseする（再衝突すれば再び`needs_session`）。receiptの`commit`が現在のHEADと一致しなければ、セッションが終わっていないものとして理由付きで`needs_session`のまま。

セッションが変更不要と判断した場合はreceiptを`result: failed`と理由（`summary`）で書き直す。`integrate ID`は`fail_integration`でrunを`failed`にし（`integration_failed`。payloadの`receipt`にそのreceiptのJSON全体を持つ。`integration_receipt`は記録しない）、mainには触れない。worktreeは残る。再試行は`ready ID`、取り消しは`cancel ID`。

### errorと復旧

mainを進める前のGit・ファイル・DBのerror（worktreeがない、mainのcheckoutにローカル変更があって`--ff-only`が失敗する、など）は`abort_integration`で`integration_error`イベントと`last_error`を書き、runを着地開始時のstatus（`awaiting_integration` / `needs_session`）に戻してleaseを解放する。`integrate`は非0で終わり、原因を直して再実行する。

`integrate`プロセスが途中で死ぬとrunは`integrating`のまま、leaseはstaleになる。`doctor`が`integrating`のrunをlease付きで報告し、leaseのPIDが死んでheartbeatが30秒以上古ければ`recover RUN_ID`が`awaiting_integration`に戻す（`run_recovered`の`previous_status: integrating`）。次の`integrate`は途中のrebaseをabortしてやり直す。mainを進めた後にDBの更新が失敗した場合はrunを戻さず、error messageに着地commitを含める（[ADR-0008](../adr/0008-merge-queue-squash-landing.md)のConsequences）。

## Cleanup and recovery

workspaceの終了はsupervisorが行う。leaseはrun単位で（[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)）、1 runの復旧が他のrunに影響しない。receipt検証を通った`awaiting_integration`のrunだけが対象で、workspaceだけを閉じ、worktreeとbranchは`integrate`が着地するまで残す（着地後に`integrate`が削除する）。`failed`（非0終了、検証拒否）、provisioningや検証処理のエラー、wrapper heartbeat切れの場合はworkspaceもworktreeも調査のため残し、closeを呼ばない。

cmux 0.64.25の`workspace create --command`はコマンドをログインシェルに打ち込む形で起動し、wrapperが終了してもシェルとworkspaceは残る（[010](../journal/010-failure-path-smoke.md)の実機確認。[015](../journal/015-e2e-happy-path.md)が観測した「終了後1〜2秒で自動的に閉じる」挙動は010の環境では再現せず、cmuxの設定に依存するとみられる）。どちらの場合もsupervisorの手順は同じで、先にworkspaceが消えていれば`cmux workspace close`は`not_found`で失敗して`cleanup_failed`になり、wrapper終了直後の`read-screen`も`screen_capture_failed`になりうる。`failed`・`interrupted`のrunのworkspaceは誰も閉じないので、調査が済んだらmaintainerが`show`の`workspace_id`を`cmux workspace close`に渡して閉じる（`doctor`は未完了runしか列挙しないので`failed`・`interrupted`のrunは出ない）。

closeの成否は`task_runs.workspace_closed_at`で表す。nullは「閉じたことを確認していない」で、closeの失敗だけでなく、cmuxが閉じた後にDBへ書けなかった場合も含む。closeの失敗は`cleanup_failed`イベントと`last_error`に残るが、run状態は変えない。閉じていないworkspaceをcleaned扱いにせず、再試行は`doctor`/`recover`（[009](../journal/009-doctor-recover.md)）で扱う。

supervisorの再起動ではrunごとのleaseとheartbeatを確認し、孤児プロセスを勝手に再実行しない。wrapperが生きている（heartbeatが30秒以内か`exited_at`記録済み）`running` / `validating`のrunだけは、staleなleaseごと次のsupervisorが引き継いで同じrunを続ける（[ADR-0012](../adr/0012-adopt-stale-lease-of-live-wrapper.md)、[`supervise`](#supervise)の5）。それ以外（wrapperが死んだ・黙った、`claimed` / `starting`、`integrating`、leaseなし）はユーザーが`recover`で明示的に復旧した後に新しいTaskRunを作る。

### `status`

`cmux-taskq status`はrunのprocessを調べずに登録とleaseだけを返す。引き継がれたrunは引き継いだsupervisorのtokenのleaseを持つので、他のrunと同じくそのsupervisorの`run_ids`に並ぶ。`supervisors`は`supervisors`表の登録を`started_at`順に、続けて登録のないtokenのlease保持者（着地中の`integrate`プロセス、または登録表以前のsupervisor）をlease順に並べ、leaseはtokenで登録に結び付ける。各項目は`pid`、`alive`（`kill -0`）、`registered`、`mode`と`workspace_id`、`binary_version`（そのプロセスが動いているcmux-taskqのversion。登録がなければnull、列より古いbinaryの登録もnull。[ADR-0014](../adr/0014-up-replaces-a-supervisor-of-another-binary-version.md)）、`parallel`と`started_at`（登録がなければnull）、`heartbeat_at`、`heartbeat_age_secs`、`stale`（PIDが死んでいるかheartbeatが30秒より古い）、`run_ids`（そのtokenのlease）。runを持たない常駐supervisorは`run_ids: []`で並ぶ。`runs`は未完了run（`claimed`/`starting`/`running`/`validating`/`integrating`）ごとに`run_id`、`task_id`、`status`、`workspace_id`、`lease`（なければnull。`pid`でどのsupervisorが持つかが分かる）。`awaiting_integration`と`needs_session`はプロセスを持たないので並ばない。

### `doctor`

`cmux-taskq doctor`は状態を変えずにJSONで報告する。

- `supervisors`: `status`と同じ。staleな登録は報告するだけで、`doctor`も`recover`も`integrate`も消さない。
- `runs`: `claimed`/`starting`/`running`/`validating`/`integrating`のrunごとに、`workspace_id`、worktreeとrun directoryとreceiptの存在、`last_error`、そのrunの`lease`（PID、`kill -0`による生存、heartbeatの経過秒数、30秒を超えた`stale`。なければnull）、登録済みwrapper/agentプロセスのPID・生存・heartbeat経過秒数・終了コード。`exited_at`が記録済みのプロセスはPIDが再利用されうるため生存確認せず`alive: null`にする。
- `blockers`: そのrunの`recover`を拒む理由の一覧。そのrunのprocessとleaseだけを見る。空なら`recoverable: true`。

cmux workspaceの存在は確認しない（cmuxなしで動く）。IDを見てユーザーが`cmux workspace list`で確認する。

### `recover RUN_ID`

1. runが`claimed`/`starting`/`running`/`validating`/`integrating`でなければ拒否する。
2. `doctor`と同じ確認を行い、未終了として登録されたプロセスのPIDが生きている、そのrunのleaseのheartbeatが30秒以内、leaseのPIDが生きている、のいずれかなら拒否する。heartbeatが止まったまま生きているsupervisorのleaseを`recover`は奪わず、ユーザーが止める（wrapperが生きていれば[adopt](#supervise)が引き継ぐ）。supervisorがabandonしたrunはleaseがないので、processが止まれば復旧できる。`recover`が扱うのは、引き継ぎの条件を満たさないrun: wrapperが死んだか30秒以上黙っている、`claimed` / `starting`、`integrating`、leaseのないrun。
3. `BEGIN IMMEDIATE`の中でそのrunのleaseが新鮮でないことと`run_processes`の行数が確認時と同じことを再検査し、runを`interrupted`（`integrating`なら`awaiting_integration`: 検証済みの成果は残っており、次の`integrate`が途中のrebaseをabortしてやり直す）にし、確認した内容を`run_recovered`イベント（`previous_status`、`status`、`lease_deleted`、`run`）に記録し、そのrunのleaseだけを削除する。

他のrun、そのlease・process、`run_processes`、worktree、branch、workspace、run directoryは触らない。Taskは`in_progress`のまま残る。再試行は`ready ID`（編集するなら`draft ID`）で行い、動いているsupervisor（または次のsupervisor）が新しいTaskRunと新しいworktreeを作る。`recover`はTaskを`ready`に戻さない: 復旧と再実行は別の判断であり、`failed`で止まったTaskの再試行と同じ経路にまとめるため。
