---
id: journal-010
type: journal
title: Failure path smoke on a disposable repository
status: done
created: 2026-09-22
updated: 2026-09-22
plan_step: 4
queue_task: null
depends_on_journal: [6, 9, 15, 18]
related:
  - journal-003
---

# 010: Failure path smoke on a disposable repository

## Goal

[plans/current.md](../plans/current.md) ステップ4。ステップ4の完了条件を、ステップ6の並列実行とステップ7のmerge queueを含めて実機で確認する。使い捨てrepositoryで`supervise --parallel 2`以上を動かし、以下を起こして二重起動や成果の喪失がないことを確認する。

- Claude異常終了（wrapperの子プロセスをkill）
- supervisor再起動（supervisorをkillして`doctor`→`recover`→再実行）
- 検証コマンド失敗
- cleanup失敗（workspaceを先に閉じておく）
- 並列中の1 runの異常終了とrecoverが、他のrunに影響しない
- merge queueの衝突（2 taskで同じ行を変える）を`needs_session`からresumeで解消して着地する

完了条件: 6シナリオそれぞれの観測結果と、DBの状態・保持されたリソースをこのジャーナルに記録し、plans/current.mdのステップ4を完了にできる。

## Log

### 2026-09-22 13:20 claude (worker)

- worker として開始。branch `journal/010-failure-path-smoke`、worktree `.worktrees/010-failure-path-smoke`。003/005-009/015/017/018、ADR-0006..0008、design 3件、runtime.rs / adapters.rs / main.rs / tests/e2e.rs を読了
- 環境: cmux 0.64.25 (106) [b685a275c]、Claude Code 2.1.278、cargo 1.93.0。`claude` on PATH は cmux の CLI shim（`cmux-cli-shims/.../claude` → `cmux-claude-wrapper` → 実体 `~/.local/bin/claude`、最後は `exec` なので pid は保たれる）。wrapper は自分の hook 用 `--settings` を注入し、渡された `--settings` を additively に fold するとコメントにある。ドッグフーディングでも SV は cmux 端末から supervisor を起動するので、この shim 経由が実運用の経路。実 Claude のシナリオは `--claude claude`（shim）で流す
- 隔離: scratchpad `<scratch>/smoke/`（セッション固有、消える）。`cmux-taskq`（このworktreeの `cargo build --locked` のコピー）、`repo/`（使い捨て repository、`seed.txt` / `shared.txt` 3行 / `.claude/settings.json` に `permissions.defaultMode: auto` / `CLAUDE.md`）、`xdg/`（`XDG_DATA_HOME`、queue は `xdg/cmux-taskq/f70746af541fc668/queue.db`、schema v6）、`stub.sh`（tests/e2e.rs の stub を拡張: title の `[break]` で seed.txt を削除して検証を壊す、`[hang]` で commit も receipt も書かず待つ、`delay=N` で N 秒待つ。通常は `stub-task<N>.txt` を commit、receipt → 2秒 → idle marker → `/exit` 待ち）、`tq`（repo を cwd に `XDG_DATA_HOME` 付きで binary を呼ぶ）、`supervise.sh`（`supervise --parallel 2 --claude <agent>` を `tee` で `supervisor-<N>.log` に残す）
- 計画: stub で 3 → 4 → 5 → 2（supervisor A を kill、B で再実行）、実 Claude で 1 と 6（supervisor C、task 7 を kill、task 8/9 が `shared.txt` の同じ行を変更、9 を `claude --resume` で解消）

### 2026-09-22 13:25 claude (worker) シナリオ3・4（stub、supervisor A）

- supervisor A: `cmux workspace create --name "TASKQ-010 supervisor" --cwd <repo> --command "<scratch>/supervise.sh <scratch>/stub.sh supervisor-A.log"` → `OK workspace:102`（UUID `CF4183FA-...`）。`supervise --parallel 2 --claude stub.sh`、lease pid 45280
- 観測（cmux）: `new-workspace --command` はコマンドをログインシェル（fish）に打ち込む形で、wrapper が終了しても workspace は閉じずプロンプトに戻る（画面に wrapper の JSON と `❯` が残る）。015 の「1〜2秒で自動 close」はこの環境では再現しない。失敗 run の workspace は supervisor が close しないので開いたまま残り、operator が閉じる必要がある
- **シナリオ3 検証コマンド失敗**: task 1 `[break] scenario 3 verification failure`、`--verify "test -f seed.txt"`。run `6ba0f328`、workspace `4C2B14BE`。stub が seed.txt を `git rm` して commit → receipt → idle → supervisor が `/exit` 送信 → `session_exited` exit 0 → `validating` → `verification_command` exit 1 → `validation_finished` `accepted: false` → run `failed`、`last_error` = `verification command "test -f seed.txt" exited with 1; see .../verify-1.log`、`result_commit` は commit `0caaa09`（commit は確認済みなので残る）、`lease_released`。task は `in_progress`。保持: worktree（`git worktree list` に残る）、branch `taskq/6ba0f328...`、run dir 一式（prompt / runner / receipt / idle / terminal-final / verify-1.log）、workspace `4C2B14BE`（閉じない）。他 run（task 2）は同時に `awaiting_integration` になり影響なし。receipt 受領から `session_exited` まで人の操作なし
- **シナリオ4 cleanup 失敗**: 1回目（task 2、`sleep 20 && test -f seed.txt`）は手動 close が間に合わず supervisor の close が先に成功（`workspace_closed`、`closed_at` 1790050845）。手動 close は `Error: not_found`。2回目 task 3 `sleep 90 && test -f seed.txt`、run `4df0358a`、workspace `35BD11DB`。`validating` 中（13:22:01）に `cmux workspace close 35BD11DB-...` → `OK workspace:105`。検証完了後の supervisor の close は `Error: not_found: Workspace not found` → `cleanup_failed` イベント + `last_error`、run は `awaiting_integration`、`workspace_closed_at` null、`lease_released`。supervisor は生き続け、次の候補を待つ。設計どおり（006）
- 観測: `status` の `supervisors` は lease の pid ごとなので、active run が無い常駐 supervisor は `supervisors: []` になり見えない。生きているかは `doctor` でも分からず、プロセスを見るしかない

### 2026-09-22 13:30 claude (worker) シナリオ5（stub、supervisor A 続き）

- **シナリオ5 並列中の1 runの異常終了と recover**: task 4 `[hang] scenario 5 victim`（commit も receipt も書かない）と task 5 `scenario 5 survivor delay=60` を同時に投入。A が2件とも claim（run `16226324` / workspace `071F439C`、run `44f26133` / workspace `8C3FEECC`）。`doctor` は両 run に lease pid 45280、wrapper / agent の pid と alive、blockers 4件を返す
- 13:24:45 task 4 の wrapper（runner pid 50743）に `kill -KILL`。fish の画面に `terminated by signal SIGKILL`。stub（pid 50803）は ppid 1 の孤児として生存。workspace は開いたまま
- 30秒後（04:25:15Z）supervisor A が `wrapper heartbeat expired; session may still be alive` で run 4 を abandon: `runtime_error`（`lease_released: true`）+ `last_error`、run は `running` のまま、lease 行は削除。A は継続し、task 5 は 04:25:37Z に `awaiting_integration`（`workspace_closed_at` 1790051137、`last_error` null）。1 run の異常が他に波及しない（017）
- `doctor`: run 4 は `lease: null`、wrapper `alive: false`（`exited_at` null のまま）、agent `alive: true` → `blockers: ["agent pid 50803 is alive"]`、`recoverable: false`。`recover` は `refusing to recover run ...: agent pid 50803 is alive` で拒否。`kill -TERM 50803` 後の `recover` → `{"outcome":"recovered"}`、run `interrupted`、`run_recovered`（`previous_status: running`、`lease_deleted: false`（abandon 時に消えている）、確認した process の状態を payload に記録）、task は `in_progress`。保持: worktree（`git worktree list` 6行 = main + run 5件）、branch、run dir、workspace `071F439C`（開いたまま）
- 観測: wrapper が SIGKILL で死ぬと `run_processes.exited_at` は wrapper 行も agent 行も null のまま。`doctor` は生存確認で補うので判定に問題はない

### 2026-09-22 13:35 claude (worker) シナリオ2（stub、supervisor A → B）と着地

- **シナリオ2 supervisor 再起動**: task 6 / 7（`delay=45`）を投入し、両方 `running`（run `250f0b5c` / workspace `9E385207`、run `0fb301d4` / workspace `801195B4`、agent_started 済み）の 13:26:40 に supervisor A（lease pid 45280。`supervise.sh` が表示する `$$` は sh の pid で、`exec ... | tee` のためパイプの子で exec されている。実 pid は `status` の lease pid か `pgrep -f 'cmux-taskq supervise'`）へ `kill -KILL`。supervisor workspace はプロンプトに戻り開いたまま
- 直後の `status`: run 6/7 `running`、lease pid 45280 `alive: false`、`stale: false`（heartbeat 直後）。stub は影響なく 45 秒後に commit → receipt → idle → `/exit` 待ちで止まる（誰も `/exit` を送らない）
- supervisor B を同じ workspace に `cmux send` で起動（lease pid 54205）。B は task 6/7 を claim しない（未完了 run が task を占有、`candidates` は `[]`）。孤児 run にも触らない
- 61 秒後の `doctor`: `supervisors` に pid 45280 `alive: false` `stale: true` `run_ids` 2件。両 run とも lease stale、wrapper / agent は `alive: true`、`blockers` 2件、`receipt_exists: true`。`recover` は `agent pid ... is alive; wrapper pid ... is alive` で拒否
- 手動で `cmux send --workspace <uuid> -- /exit` + `send-key enter` → stub が終了、wrapper が `session_exited` exit 0 を記録（`supervision_finished` は無い: supervisor が死んでいる）。`doctor`: process は `exited_at` 付き `alive: null`、`blockers: []`、`recoverable: true`。`recover` 2件 → `interrupted`、`run_recovered`（`lease_deleted: true`、stale lease の内容を payload に記録）。`status` は runs / supervisors とも空
- `ready 6` / `ready 7` → B が次の poll で claim、新 run `41cc9879` / `b85d3a68`（新 worktree、新 workspace `0AE428DC` / `2858A2DC`）→ 約 60 秒で `awaiting_integration`（workspace は close 済み）。`show 6` には interrupted の旧 run と新 run が並ぶ。旧 run の worktree / branch / workspace は残る
- **着地**: `integrate --next` を繰り返し → task 2, 3, 5, 6, 7 の順（`validation_finished` 順の FIFO）で `integrated`、6回目は `no_run_awaiting`。main は `seed → seed2 → task2 → task3 → task5 → task6 → task7` の直線、各 commit は title / summary / `Taskq-Task` / `Taskq-Run` trailer。main を checkout している repo の作業ツリーにも `stub-task*.txt` が現れる（`merge --ff-only` 経路）。着地した run の worktree と branch は削除され `refs/taskq/runs/<run-id>` が残る。`git worktree list` は main + 保持 4件（failed 1、interrupted 4 / 6旧 / 7旧）
- B に SIGINT → `{"outcome":"stopped","errors":[],"runs":[task 6, 7 awaiting_integration]}` で終了（graceful drain の経路、017 で手動確認済みのものを再確認）

### 2026-09-22 13:40 claude (worker) シナリオ1・6（実 Claude、supervisor C）

- supervisor C を同じ workspace に `supervise.sh claude supervisor-C.log`（`--claude claude`）で起動（lease pid 58830）。`executable("claude")` は supervisor workspace の PATH で `~/.local/bin/claude` → `~/.local/share/claude/versions/2.1.278` に解決した。cmux の CLI shim は Claude Code が起動した端末の PATH にだけ入るので、`cmux workspace create` で起動した supervisor からは shim を通らず実体が直接 agent になる（ドッグフーディングでも同じ経路）
- task 8 `Add notes.txt describing the repository`（`--verify "test -f seed.txt"`, `"test -f notes.txt"`）、task 9 / 10 `Mark line 2 of shared.txt as changed by task 9 / 10`（同じ行を書き換える、`--verify "test -f seed.txt"`, `"grep -q 'changed by task' shared.txt"`）を投入。`--parallel 2` で 8（run `45050118` / workspace `BEB1A458`）と 9（run `ea6962b5` / workspace `29BCEB71`）が先に起動
- 各 workspace で新規 worktree の信頼確認（`Yes, I trust this folder`）を `cmux send-key down` + `enter` で通した。repo の `.claude/settings.json`（`permissions.defaultMode: auto`）が効き、画面に `⏵⏵ auto mode on`。以後の操作は不要。モデルはユーザー既定（`claude-fable-5-1[1m]`）。画面に「You've used 93% of your session limit · resets 3:40pm」が出ていたので実 Claude は最小限にした
- **シナリオ1 Claude 異常終了**: 13:33:32、task 8 の Claude（`doctor` の agent pid 60208、`versions/2.1.278 --session-id 45050118...`）が notes.txt を commit した直後・receipt 前に `kill -KILL`。wrapper が即座に `session_exited` `exit_code: 128`（signal 終了は `code()` が None → 128）→ `supervision_finished` `failed` → `lease_released`。画面に wrapper の JSON `{"exit_code": 128, ...}`。run は `failed`、`result_commit` null、`last_error` null、receipt なし。保持: worktree（commit `74f48ee` を含む）、branch、run dir（`claude.debug.log`、`latest` → debug log への symlink（Claude が作る）、`terminal-final.txt`）、workspace `BEB1A458`（開いたまま）。task 9 は影響なく進行し、空いた slot に task 10（run `aaa491a0` / workspace `DAC0C35A`）が claim された（`main` は同じ `33d3687`）
- task 9 / 10 とも人の操作なしに完走: `receipt_observed` → 約 5〜7 秒後に `session_idle_observed`（`hook_event_name: Stop`、`session_id` = run ID、marker は `transcript_path` 付きの hook JSON）→ `exit_requested` → 1 秒で `session_exited` exit 0 → 検証コマンド 2 件 exit 0 → `awaiting_integration` → `workspace_closed`。receipt は `tests: passed`（検証コマンドの結果と行の比較を証跡に）、`e2e` / `subagent_review` は理由付きの `not_applicable`（task 10 は subagent review を実施して `passed`）。`terminal-final.txt` には `/exit` 後の「Resume this session with: claude --resume <run-id>」と wrapper の JSON
- **シナリオ6 merge queue の衝突**: `integrate --next` → task 9 が `9ede494` として着地（`completed`、worktree 削除）。`integrate --next` → task 10 は `git rebase 9ede494` が shared.txt で衝突 → `rebase --abort` → `needs_session`（`integration_deferred`: `conflicts: ["shared.txt"]`、`aborted: true`、`output_tail` に Git の出力、`lease_released` reason `integration_deferred`）。`last_error` は「rebase onto main 9ede494... conflicted in shared.txt; resolve it in the worktree (git rebase 9ede494...), rerun the verification commands, and rewrite the receipt with the new head」。worktree は検証済み head `051e107` のまま clean
- `cmux workspace create --name "TASKQ-010 resume 10" --cwd <worktree> --command "claude --resume aaa491a0-..."` → 元セッションの最終報告が画面に復元され、auto mode のまま。`cmux send` で `last_error` の内容と「`git rebase 9ede494...`、line 2 を `line2 changed by task 9 and task 10` に解消、`--continue`、検証コマンド再実行、`<run-dir>/receipt.json` を新 head で書き直す、`/exit` は打たない」を送信。約 30 秒で完了報告: 新 head `5183c37`、receipt は `commit` = `5183c37`、summary に rebase と解消の説明。worktree clean
- `integrate 10` → `integration_started`（`previous_status: needs_session`）→ receipt が HEAD を指す → rebase は no-op（`head_before == head_after`）→ 検証コマンド 2 件（`integrate-verify-N.log`）→ `4541b80` として着地、`completed`、`worktree_removed`。main は `seed → ... → task 9 → task 10` の直線、`shared.txt` line 2 は `line2 changed by task 9 and task 10`、commit message は title + セッションの summary + trailer。resume 中のセッションは worktree が消えた後も生きていたので `/exit` を送って閉じた
- 後始末: supervisor C に SIGINT → `{"outcome":"stopped","errors":[],"runs":[8 failed, 9 / 10 awaiting_integration]}`。`pgrep` で Claude / stub / wrapper の残存なし。作成した workspace（supervisor、resume、失敗 / interrupted の run 5 件、probe 2 件）を `cmux workspace close <uuid>` で全て閉じた。残ったのは自分（`TASKQ-010 failure-path-smoke`）だけ
- 最終状態: task 1 / 4 / 8 `in_progress`（run は failed / interrupted / failed）、他 7 件 `completed`。`task_runs`: failed 2、interrupted 3、integrated 7。`run_leases` 0 行。`doctor` は runs / supervisors とも空。worktree は main + 保持 5 件（1、4、6旧、7旧、8）。`refs/taskq/runs/` に着地 7 件

### 2026-09-22 13:45 claude (worker) 観測と所見

- cmux 0.64.25 で `workspace create --command` はコマンドをログインシェル（fish）に打ち込み、コマンド終了後もシェルが残って workspace は閉じない（`--command true` / `sleep 1` の probe で確認、5 秒以上残る）。015 が記録した「1〜2 秒で自動 close」はこの環境では起きない。supervisor の close はこれに依存せず動くが、設計文書の記述は直す（Promoted）
- 失敗・中断した run の workspace は誰も閉じない（設計どおり調査用に保持）。`recover` も閉じない。SV は `show` / `doctor` の `workspace_id` を見て `cmux workspace close <uuid>` する手順が要る（012 / 014 の手順に入れる）
- 非0終了で `failed` になった run は `last_error` が null で、理由（exit code）は `supervision_finished` イベントにしかない。`list` / `show` の一覧で失敗理由を見るには不便（Found 1）
- `status` / `doctor` の `supervisors` は lease 由来なので、active run のない常駐 supervisor は見えない。生存確認は `pgrep -f 'cmux-taskq supervise'` になる（Found 2）
- 実 Claude は新 worktree ごとに信頼確認で止まる。`--settings` の hook は承認なしで有効、`.claude/settings.json` の `defaultMode: auto` は信頼後に効く。ドッグフーディングでは SV が run ごとに `send-key down / enter` を送るか、信頼確認を省く方法が要る（Found 3）
- `integrate ID` は resume 中のセッションが開いたままでも worktree を削除する（セッションは生き続け、シェルの cwd が消える）。SV は `integrate` の前にセッションを `/exit` する手順にした方が安全（Found 4、手順の問題で runtime の bug ではない）
- `supervise` の `--claude claude` は supervisor 側の PATH で解決される。cmux の CLI shim（`cmux-claude-wrapper`）は Claude Code 内の端末にしか無いので、`cmux workspace create` で起動した supervisor は実体を直接起動し、cmux の hook 注入は起きない
- runtime の bug は見つからなかった。005〜009、017、018 の未検証項目（新 receipt 形式の実機、supervise 経由の close、supervisor kill → doctor → recover、実 Claude 2 件同時、`claude --resume` での衝突解消）は全て実機で確認できた。未確認のまま残るもの: `exit_request_timed_out` の実機、検証コマンドの 30 分 timeout、main を進めた後の DB 更新失敗

## Result

使い捨て repository（scratchpad、`XDG_DATA_HOME` 隔離、schema v6）で `supervise --parallel 2` を専用 cmux workspace から動かし、6 シナリオを実機（cmux 0.64.25 (106)、Claude Code 2.1.278）で確認した。runtime のコードは変更していない。

| # | シナリオ | agent | 結果 |
| --- | --- | --- | --- |
| 1 | Claude 異常終了（agent を `kill -KILL`） | 実 Claude | `session_exited` exit 128 → run `failed`、worktree / branch / run dir / workspace 保持、隣の run と空 slot の claim に影響なし |
| 2 | supervisor 再起動（`kill -KILL` → `doctor` → `recover` → `ready` → 新 run） | stub | lease が stale、process 生存中は `recover` 拒否、手動 `/exit` 後に `recover` → `interrupted`、新 supervisor は孤児 run を触らず `ready` 後に新 run を `awaiting_integration` まで完走 |
| 3 | 検証コマンド失敗 | stub | `verification_command` exit 1 → `failed`、`last_error` に理由と log path、`result_commit` は残る、workspace は閉じない |
| 4 | cleanup 失敗（`validating` 中に手動 close） | stub | `cleanup_failed` イベント + `last_error`、run は `awaiting_integration`、`workspace_closed_at` null、後で正常に着地 |
| 5 | 並列中の 1 run の異常（wrapper を `kill -KILL`）と recover | stub | 30 秒で abandon（`runtime_error`、lease 削除、status 維持）、隣の run は完走、agent 停止後に `recover` → `interrupted` |
| 6 | merge queue の衝突（2 task が `shared.txt` の同じ行を変更） | 実 Claude | `--next` で 1 件目着地、2 件目は `needs_session`、`claude --resume <run-id>` のセッションが rebase・解消・検証・receipt 書き直し → `integrate ID` で着地、main は直線で 1 task = 1 commit |

二重起動や成果の喪失は起きなかった。`integrate --next` で 7 件を FIFO で着地させ、main は `seed → 7 commit` の直線、着地した run の worktree と branch は削除され `refs/taskq/runs/<run-id>` が残る。各シナリオのコマンド、イベント列、DB の状態、保持リソースは Log にある。

Found（runtime の bug ではない改善候補。別ジャーナルで扱う）:

1. 非0終了で `failed` になった run は `last_error` が null。理由は `supervision_finished` イベントにしかない
2. `status` / `doctor` の `supervisors` は lease 由来で、active run のない常駐 supervisor が見えない
3. 実 Claude は新 worktree ごとに信頼確認で止まり、operator の `send-key` が要る。ドッグフーディングの SV 手順か adapter で扱う
4. `integrate ID` は resume 中のセッションが開いていても worktree を削除する。SV の手順として `integrate` の前にセッションを `/exit` する
5. 失敗・中断した run の workspace は誰も閉じない（設計どおり）。SV の手順に `cmux workspace close <workspace_id>` を入れる

未確認のまま残るもの: `exit_request_timed_out` の実機、検証コマンドの 30 分 timeout、main を進めた後の DB 更新失敗（ADR-0008 の既知の限界）。

## Promoted

- [plans/current.md](../plans/current.md): ステップ4を完了に
- [design/supervisor-lifecycle.md](../design/supervisor-lifecycle.md): cmux の workspace はコマンド終了後も閉じない（015 の観測を訂正）、失敗 run の workspace は operator が閉じる、`last_verified` を更新
- Found 1〜5 は SV に報告し、後続ジャーナル（012 / 014 の手順、または runtime の改善）で扱う。ADR は追加しない（決定の変更はない）
