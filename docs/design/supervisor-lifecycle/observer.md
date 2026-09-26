---
id: design-supervisor-lifecycle-observer
type: design
title: "Observer"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0044
  - design-domain-model
---

# Observer

[ADR-0044](../../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定4・18・23（task 292）。observerはnoteとdraft goalを書かず、findingを記録・更新し、findingに紐づけた`blocked`のaskを上げる（`--because`は`scope` / `discard` / `recovery_failed`から選ぶ）。同じADRの決定19〜22のうち、変化の無いときに起動しないこと、MCPを読まないこと、既定の間隔を3時間にすること、proposalを求める印からplannerを立てること、`events --full` / `timeline` / `observe --history`はgoal 31の後続taskが実装する。dagqが回っているかを観察して継続的改善の材料を残すjobで、個々の詰まりは解消しない。cmux workspaceを持たず、`AgentProvider::headless_command`（Claudeでは`claude -p --allowedTools 'Bash(dagq:*)' -- <prompt>`）で起動する。実装は`src/observer.rs`。

- **`dagq observe [--since CURSOR] [--daily] [--dry-run] [--timeout SECS] [--claude PATH]`**: 1回のobservation。
  1. 入力を集める: `stats --since <cursor>`（`--since`が無ければ前回のobserveが保存した`<queue dir>/observer/cursor`、それも無ければ`stats`の既定の直近50件。`--daily`は24時間前より前の最後のevent id）、`open` / `proposed`のfinding（`findings`）、直近20件のnote（`notes`）、openなask（`asks --open`）、`graph`の`candidates`と`critical`。
  2. promptを作る。役割は「dagqが回っているかを観察し、うまくいっていないことを`finding record`でfindingに記録し（根拠はrun_eventsのidを`--evidence`で渡し、本文に埋めない。同じ種類・対象・subjectは既存のfindingの更新になるので、持っていない根拠か読みの変化があるときだけ記録し直す）、remedyが要るfindingには`--propose`で理由を付け、もう起きていない問題は`finding resolve`し、今人の判断が要るものは`ask --kind blocked --finding ID`でinboxに上げる（openなaskのあるfindingには上げない）。note、goal、taskは書かず、個々の詰まりは解消しない。run / task / goalの状態は変えない」。queueのコマンドは`dagq --db <db>`の形で渡す。`--daily`は24時間の傾向を見る別の文面にする。`--dry-run`はpromptを`{dry_run, mode, since, cursor, prompt}`で返し、何も起動・記録しない。
  3. `<queue dir>/observer/<started_at>/`（同じ秒に既にあれば`-1`以降の接尾辞）を作り、`prompt.md`と`input.json`を書き、`observe_started`（`mode`: `hourly` / `daily`、`since`、`dir`）をtaskの無いrun_eventsに記録する。
  4. そのdirをcwdに、env `DAGQ_ROLE=observer`、`DAGQ_QUEUE=<db>`、PATHの先頭に`dagq`のdirを置いてagentを起動し、stdout / stderrを`output.log`に書く。`--timeout`（既定1800秒）を過ぎたらkillする。
  5. 終了後、`observe_started`より後にobserverが書いた`finding_recorded`と`finding_updated`（payloadの`by: observer`）とobserverのaskを数え、`observe_finished`（`mode`、`outcome`: `succeeded` / `failed`（非0終了） / `error`（起動できない・timeout）、`exit_code`、`error`、`since`、`cursor`（`stats`の`next_cursor`）、`cursor_saved`、`findings_recorded`、`findings_updated`、`asks`、`duration_secs`、`dir`）を記録して返す。`succeeded`のhourlyのときだけ`cursor`を`<queue dir>/observer/cursor`に保存する（一時ファイルからrename）。失敗したobservationのwindowは次のobservationが読み直す。dailyはcursorを動かさない。
- **権限**: observerのenvからのCLIは許可一覧で判定する（`main.rs`の`observer_access`、一覧は[domain-model](../domain-model.md#current-operations)）。読み取り（`findings`と`notes`を含む）、`finding record` / `finding resolve`、`ask --kind blocked`だけが通り、`note`、`goal add`（`--draft`を含む）、`add`、`finding dismiss`、`ready` / `integrate` / `recover` / `goal ready` / `answer` / `observe` / `supervise`などは`{"error":"observer may not change queue state"}`で拒否される。
- **timer**: `supervise --observe-interval SECS`（既定3600、`--once`のときは既定0。0でobserverを起動しない。dailyも含む）と`--observe-daily BOOL`（既定true）。ループの各passで、走っているobserverが無ければ、dailyが有効で最後のdailyの`observe_started` / `observe_finished`から24時間経っていればdailyを、そうでなく最後のhourlyから`--observe-interval`秒経っていればhourlyを、`<runner> --db <db> observe --claude <claude> [--daily]`の子プロセスで起動する（cwdはcheckout、`DAGQ_ROLE`は外す）。一度も記録の無いmodeは期日が来ている。期日はqueueのrun_eventsで判定するので、別のsupervisorや手の`observe`も数える。加えて同じプロセスが同じmodeを起動してから間隔が経つまでは再起動しない（記録を書く前に落ちたobserverを毎passで起動しないため）。同時に走るobserverは1つで、run slotを使わない。子プロセスの終了はlogに1行残し、記録は`observe_finished`が持つ。launchd modeでもin-cmux modeでも`supervise`の既定値で同じに動く（`up`はこのflagを渡さない）。
- **出力の扱い**: findingは人とplannerが`findings`で影響の大きい順に読み、手当てしないと決めたものを`finding dismiss ID --reason`にする。proposalを求める印からruntimeがplannerを立てる経路（ADR-0044の決定19）は後続taskが実装するので、今は印の付いたfindingも人とplannerが読んで扱う。`blocked`のaskは`status --role inbox`に`ask_opened`として出る。導入前にobserverが書いたnoteとdraft goalは記録として残り、draft goalは人が開いたplannerで扱う。
