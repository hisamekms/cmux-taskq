---
id: design-supervisor-lifecycle-plan-planners
type: design
title: "`plan` / `planners`"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0044
  - design-persistence
---

# `plan` / `planners`

[ADR-0044](../../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定1・6・12・13。plannerは常駐せず、proposalごとにオンデマンドのworkspaceを開く。use caseは`src/application/planner.rs`、entry pointは`compose::plan` / `compose::planners` / `compose::planner_session`。

- **記録**: planner 1つが`planners`表（schema v23、[persistence](../persistence.md)）の1行。`session_workspaces`はroleが主キーでplannerを1つしか持てないので使わない。行は`origin`（`person` / `runtime`）、runtimeが立てたときの`proposal_id`、workspaceのUUID（`workspace_id`、UNIQUE）、session wrapperの`wrapper_pid` / `agent_pid` / `heartbeat_at` / `exit_code` / `exited_at`、終わった印の`closed_at`と`error`を持つ。plannerのディレクトリは`<queue dir>/planners/<planner-id>/`で、初期prompt（`prompt.txt`）、wrapperとして動くバイナリの写し（`runner`。runの`runner`と同じく、ビルドし直しても動いているplannerは変わらない）、Claudeのsettings（`claude-settings.json`）・debug log（`claude.log`）・idle marker（`idle.json`）とその履歴（`idle.log`）を置く。
- **`dagq plan [--plugin-dir PATH] [--repo PATH] [--cmux EXE] [--claude EXE]`**: cmux（`ping`）とClaude（`--version`）をpreflightし、`--plugin-dir`を絶対pathにして、人が開くplanner（`origin: person`）を1つ開く（`open_person_planner`）。何度打っても新しいworkspaceを開くので、plannerは複数同時に開ける。開き方（`open_planner`）: 行を作り、ディレクトリにpromptと`runner`を書き、`cmux workspace create --name "[<repo>]planner#<id>" --description "dagq role=planner queue=<queue hash> planner=<id>" --env DAGQ_ROLE=planner --env DAGQ_QUEUE=<db> --env DAGQ_PLANNER_ORIGIN=<origin> --env DAGQ_PLANNER_ID=<id> [--group <queueのgroup>] --command "<planner dir>/runner --db <db> planner-session --planner <id> --claude <resolved> [--plugin-dir PATH]" --focus false --cwd <repository root>`で作り、UUIDを行に書く。cmuxが作れなければ行を`error`付きで閉じてerrorで止まる。色`Blue`とpill`dagq_role planner --icon map`を当て（失敗は`warnings`）、ピンは付けない。結果は`{"planner": {planners表の行}, "name", "dir", "warnings"}`。`plan`も`planners`もplannerの行を閉じない（行を閉じるのは、workspaceを開けなかったときと、後続taskでsupervisorが終わりを見届けたとき。見えなかっただけのworkspaceを閉じたと記録しないため。存在判定は下記[Naming](naming.md#naming)のとおり全windowを見る）。`submit`は`CMUX_WORKSPACE_ID`と`DAGQ_PLANNER_ORIGIN`からproposalの持ち主を決めるので、plannerが出したproposalはそのworkspaceのものになる。
- **runtimeが立てるplanner**: `open_runtime_planner(launch, proposal, tasks, reasons)`がproposalの存在を確かめ、`origin: runtime`・`proposal_id`付きの行で`[<repo>]planner#<id> - proposal <proposal-id>`を開く。初期promptは`runtime_planner_prompt`で、proposal、そのtask（ID・状態・title）、plan reviewの指摘（reasons）を載せ、直して`dagq submit --proposal <id>`で出し直すこと、計画の意図を変える修正はinboxにaskして答えを待つことを指示する。`DAGQ_PLANNER_ORIGIN=runtime`で出し直すので、そのplannerがproposalの持ち主になる。supervisorはplan reviewのrevise（持ち主のplannerが生きていないとき）とreopenでこれを呼ぶ（[Plan review (supervisor)](plan-review.md#plan-review-supervisor)の9）。follow_upの経路はgoal 29の後続taskが足す。
- **session wrapper（`planner-session`、hidden）**: runの`session`と同じく、自分を`register_planner_wrapper`で登録し（1つのplannerに1回だけ。閉じたplannerは拒む）、`AgentProvider::planner_command`（Claudeでは`claude --debug-file <dir>/claude.log --add-dir <dir> --settings <dir>/claude-settings.json [--plugin-dir PATH] -- '<prompt>'`をrepository rootで。settingsはworkerと同じ`stop_hook_settings`で、`Stop` hookが`<dir>/idle.json`を書く）でagentを起動して`register_planner_agent`し、agentが終わるまで`wait_interval`ごとに`heartbeat_planner`し、終了コードを`planner_exited`で記録する。logは`<queue dir>/logs/planner-session-*.jsonl`。
- **生存とidleの判定**（`planner_view`、`PlannerSession::state`）: workerと同じ材料で決める。閉じた行か、workspaceのUUIDが`cmux workspace list`に居なければ`closed`。workspaceかwrapperの登録がまだなら`opening`、行を作ってから120秒（`PLANNER_STARTUP_SECS`）たってもそのままなら`lost`（cmuxがwrapperを起動できなかった、開いたプロセスがworkspaceを記録する前に死んだ）。agentの終了が記録されていれば`exited`。wrapperのPIDが死んでいるかheartbeatが30秒（`HEARTBEAT_TIMEOUT_SECS`）より古ければ`lost`。それ以外は動いているsessionで、画面（`capture`）が作業中（`AgentSignals::working`）なら`working`、idle markerがあってbackgroundの作業が残っていなければ（`AgentSignals::idle_hook`）`idle`、markerが無いかbackgroundが走っていれば`working`。`alive`は`opening` / `working` / `idle`。`idle_since`はidleのときのmarkerの書かれた時刻（Unix秒）で、送った文面より後にidleになったかの比較に使える。
- **`dagq planners [--all] [--cmux EXE]`**: 閉じていないplanner（`--all`で全部）を古い順に、行の各fieldに`state`・`alive`・`idle_since`・`dir`を足して`{"planners": [...]}`で返す。読むだけで、observerとreviewerも打てる（`plan`と`planner-session`は拒む）。
- **test**: `tests/it/lifecycle_plan.rs`がfakeのcmuxで`plan`の記録・title・env・見た目・閉じたplannerの片付け・workspaceを作れなかったとき、runtimeが立てるplanner、wrapperと状態の判定を、`tests/e2e.rs`の`plan_opens_planners_side_by_side_that_submit_go_idle_and_exit`が実cmuxとstubのClaudeで、2つのplannerを同時に開き、それぞれがproposalをsubmitしてidleになり、`/exit`で終わるまでを確かめる。
