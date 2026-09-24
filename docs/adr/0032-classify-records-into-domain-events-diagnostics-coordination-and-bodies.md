---
id: adr-0032
type: adr
title: 記録をドメインevent・診断telemetry・協調状態・本文の4つに分類し、それぞれの送り先（Web / OTLP / ローカル）を決める
status: proposed
created: 2026-09-24
updated: 2026-09-24
owners:
  - hisamekms
tags:
  - runtime
  - observability
  - persistence
  - architecture
related:
  - adr-0010
  - adr-0016
  - adr-0020
  - adr-0023
  - adr-0033
  - adr-0034
  - design-persistence
  - design-supervisor-lifecycle
---

# ADR-0032: 記録をドメインevent・診断telemetry・協調状態・本文の4つに分類し、それぞれの送り先（Web / OTLP / ローカル）を決める

## Context

2026-09-24にユーザーとplannerがsupervisorのlogを調べた（goal 21）。記録の置き場所が性質で分かれておらず、置き場所ごとに読めるものと失われるものが決まっている。

- `run_events`（queue DB）: taskとrunに起きたこと。ただし同じpayloadに、仕事の事実（`status`、`verdict`、`commit`）と、そのマシンでしか意味を持たない値（`pid`、`workspace_id`、`worktree_path`、`log_path`）と、自由文（`message`、`error`、`output_tail`、`excerpt`）が混ざる。`events --all`はpayloadを`watch::compact_event`で`status` / `exit_code` / `ask_id` / `reason`に削り、`show`は10件を300字で切るので、詳細はsqlite3でしか読めない。
- supervisorのテキストlog（[ADR-0010](0010-maintainer-and-resident-supervisor.md)の決定6、`SupervisorLog`の`<queue dir>/logs/supervisor-<started_at>-<pid>.log`）: `NoteLog::note`の進行メッセージ。自由文で、機械で読めない。
- stderr（`eprintln!`、21か所）: `integrate`の進行、pushの結果、follow_upの登録失敗、supervisorのheartbeatの失敗。in-cmux modeではsupervisorのworkspaceを閉じると消える。
- `logs/rebind.jsonl`（[ADR-0020](0020-rebind-queue-to-a-moved-repository.md)の決定3）: ADR-0020の時点では`run_events`がqueue単位の出来事を受け付けなかったので別のファイルに置いた（今はschema v12以降で`backend_call_failed`と`observe_*`がtaskもrunも無いeventとして入る）。
- run dir: worker / review / triageの画面とlog、`integrate-<attempt>-verify-N.log`（[ADR-0023](0023-verify-once-review-in-supervisor-run-env-graph-and-stats.md)）。

ユーザーは、将来の拡張としてWebを複数マシンの協調役にし、監査の正本をWebに置く方針を決めた。また、OTLPで自分のbackendやSaaSに計測を送れるようにしたい。どちらも、何を外に出してよく、何がそのマシンの中でしか意味を持たないかが決まっていないと始められない。本ADRはその分類を決める。送る実装はしない。

## Decision

1. **記録を4つに分類する。** 新しく記録する値は必ずどれか1つに属させる。1つのeventのpayloadに複数の分類の値があるときは、項目ごとに分類する（下の表）。

   | 分類 | 中身 | 性質 |
   | --- | --- | --- |
   | ドメインevent | 仕事（goal・task・run・ask）に起きたことの事実。状態の遷移、判定（verdict、分類コード）、commit、回数、所要時間、数値の条件（parallel、load、トークン数、コスト）、人やjobの判断の要約 | マシン依存の値を持たない。将来Webに送る正本で、監査の正本 |
   | 診断telemetry | 進行・失敗の経過、pid、path、cmuxのworkspace / surface ID、Claude Codeのtranscriptの場所、loadの時系列、自由文のエラー | そのマシンの中でだけ意味がある。障害調査に使う |
   | 協調状態 | lease、heartbeat、process（wrapper / agent）、supervisorの登録、`session_workspaces` | 今この瞬間の排他と生存。履歴ではなく現在値が正 |
   | 本文 | 画面（capture）、debug log、検証コマンドのlog、`output_tail`、receiptの`evidence_or_reason`、reviewの`reasons` / `summary`、noteの`text` | 大きく、秘密やpathを含みうる |

   「マシン依存の値」は、そのマシンの中の場所や実体を指す識別子（path、pid、workspace / surface ID、lease token、hostname）をいう。数値の計測（load、所要時間、トークン数、コスト）はマシンを指さないので、runの条件としてドメインeventに持ってよい。loadを時系列で追うのは診断で、claim時点の1つの値はrunの条件としてドメインである。

2. **送り先を分類で決める。**

   | 分類 | Web（将来の協調役） | OTLP（有効なときだけ、[ADR-0033](0033-one-tracing-pipeline-with-local-json-lines-and-optional-otlp.md)） | ローカル |
   | --- | --- | --- | --- |
   | ドメインevent | 常に送る（事実と分類コード） | spanの属性とeventとして、既定の許可範囲（ADR-0033の決定3: ID・時刻と所要時間・分類コード・kind / status / verdict・数値・version）だけ | queue DBの`run_events`（今の正本） |
   | 診断telemetry | 送らない | 既定では送らない。`dagq.toml`で属性の許可を広げたときだけ | queueの`logs/`のJSON Lines（ADR-0033） |
   | 協調状態 | 協調役になった後はWebが正本（下の決定4） | 送らない | queue DBの表（今の正本） |
   | 本文 | 明示の操作（`dagq export RUN`のようなcommand）で、秘密を伏せて、必要なrunだけ送る | 送らない | run dirとqueueの`logs/` |

   Webに送る本文の範囲はユーザーが案1に決めた: 事実と分類コードは常に送り、本文は必要なrunだけ人が明示の操作で送る。exportは送る前に既知の秘密（`[run.env]`の値、tokenらしい文字列）を伏せ、何を送ったかをドメインeventに残す。自動で本文を送る経路は作らない。

3. **今の`run_events`はローカルの混在の置き場として残し、項目ごとの分類を公開契約にする。** kind名は[ADR-0016](0016-maintainer-notification-and-compact-output.md)で公開契約で、既存CLIの出力も変えないので、今あるpayloadの項目は動かさない。新しい値は分類に従って置く: ドメインeventの項目は`run_events`に、診断は`logs/`のJSON Linesに、本文はrun dirに。ドメインeventのpayloadに、マシン依存の値を新たに足さない。Webへ送る実装ができたときは、下の表でドメインに分類した項目だけを送り、診断と本文の項目は落とす。表の「分類」列はそのkindの主な性質で、送るかどうかは項目ごとに決まる（診断や協調状態が主のkindでも、ドメインの項目と[ADR-0034](0034-domain-events-carry-reason-codes-actor-and-configuration-changes.md)の分類コードは送る。ドメインの項目が無いkindは送らない）。

   今の`run_events`の全kind（2026-09-24のmain、`src/infrastructure/{sqlite,runtime_store,asks}.rs`・`src/application/{supervise,integrate,recording}.rs`・`src/observer.rs`の記録箇所から拾った）:

   | kind | 分類 | ドメインの項目 | 診断の項目 | 本文の項目 |
   | --- | --- | --- | --- | --- |
   | `task_created` | ドメイン | `goal_id` | | |
   | `task_status_changed` | ドメイン | `from`, `to` | | |
   | `task_goal_changed` | ドメイン | `from`, `to` | | |
   | `task_paths_changed` | ドメイン | `from`, `to`（repository root起点のglob） | | |
   | `dependency_added` | ドメイン | `predecessor_id` | | |
   | `dependency_removed` | ドメイン | `predecessor_id` | | |
   | `goal_created` | ドメイン | `goal` | | |
   | `goal_updated` | ドメイン | `old`, `new` | | |
   | `goal_status_changed` | ドメイン | `from`, `to` | | |
   | `goal_closed` | ドメイン | `verdict`, `tasks` | | |
   | `observation` | ドメイン | `kind`, `by` | | `text` |
   | `ask_opened` | ドメイン | `ask_id`, `kind`, `asked_by` | | |
   | `ask_answered` | ドメイン | `ask_id`, `kind`, `answer`, `runtime_delivers`, `runtime_closed` | | |
   | `ask_delivered` | ドメイン | `ask_id` | `workspace_id` | |
   | `ask_delivery_failed` | ドメイン | `ask_id` | `workspace_id`, `error` | |
   | `run_claimed` | ドメイン | `from`, `to`, `provider` | | |
   | `run_planned` | 診断 | | `repo_path`, `run_dir`, `branch`, `worktree_path`, `receipt_path`, `log_path` | |
   | `worktree_created` | 診断 | | `path`, `branch` | |
   | `workspace_created` | 診断 | | `workspace_id` | |
   | `wrapper_started` | 協調状態 | | `pid` | |
   | `agent_started` | 協調状態 | `session_id`（run IDと同じ値） | `pid` | |
   | `lease_acquired` | 協調状態 | `reason` | `pid`, `previous_token` | |
   | `lease_released` | 協調状態 | `reason` | | |
   | `run_adopted` | 協調状態 | `previous_heartbeat_age_secs` | `previous_token`, `previous_pid`, `wrapper`, `token`, `pid` | |
   | `wrapper_heartbeat_expired` | 協調状態 | `heartbeat_age_secs` | `pid`, `workspace_id` | |
   | `first_commit_observed` | ドメイン | `commit`, `base_commit` | | |
   | `receipt_observed` | ドメイン | `validated` | `path` | |
   | `session_idle_observed` | 診断 | | Stop hookの入力（`session_id`、`transcript_path`など） | |
   | `prompt_waiting` | ドメイン | `prompt` | `workspace_id`, `screen_hash` | `excerpt` |
   | `prompt_cleared` | 診断 | | `workspace_id` | |
   | `screen_capture_failed` | 診断 | | `error` | |
   | `exit_requested` | ドメイン | `timeout_secs` | `workspace_id` | |
   | `exit_request_timed_out` | ドメイン | `timeout_secs` | `workspace_id` | |
   | `session_exited` | ドメイン | `exit_code` | | |
   | `supervision_finished` | ドメイン | `status`, `exit_code`, `session_live` | | |
   | `runtime_error` | ドメイン | `lease_released` | `message` | |
   | `run_recovered` | ドメイン | `previous_status`, `status`, `lease_deleted` | recoverの報告のうちpath・pid・workspace ID | |
   | `backend_call_failed` | 診断 | `op`, `timeout_secs`, `load_avg`, `slots`, `parallel` | `workspace_id`, `error` | |
   | `validation_finished` | ドメイン | `accepted`, `result_commit`, `status`, `evidence_missing`, `scope_violation`, `allowed_paths`, `receipt`の`result` / `commit` / 各checkの`status` / `summary` / `follow_ups` | `reason`（pathを含む自由文） | `receipt`の`evidence_or_reason` |
   | `scope_violation` | ドメイン | `paths`, `allowed`（repository root起点） | `reason` | |
   | `evidence_missing` | ドメイン | `checks` | `reason` | |
   | `review_started` | ドメイン | `attempt`, `session_live` | `workspace_id` | |
   | `review_finished` | ドメイン | `verdict`, `attempt`, `duration_secs` | | `reasons`, `summary` |
   | `review_failed` | ドメイン | `attempt`, `duration_secs`, `status` | `error` | |
   | `revise_requested` | ドメイン | `attempt`, `sent_at` | | `reasons` |
   | `revise_finished` | ドメイン | `attempt`, `head` | | |
   | `revise_receipt_rejected` | ドメイン | `attempt` | `reason` | |
   | `conflict_precheck` | ドメイン | `attempt`, `main`, `head`, `merge_base`, `conflicts`（repository root起点）, `requested`, `sent_at` | `asked`, `error` | |
   | `conflict_resolved` | ドメイン | `attempt`, `head` | | |
   | `conflict_receipt_rejected` | ドメイン | `attempt` | `reason` | |
   | `landing_decided` | ドメイン | `ask_id`, `answer`, `status` | | `reason`（reviewの理由を含む文） |
   | `resume_started` | ドメイン | `attempt`, `main` | `reason`（`last_error`の自由文） | |
   | `resume_skipped` | ドメイン | `head`, `main`, `approved`, `status` | | |
   | `resume_finished` | ドメイン | `attempt`, `outcome`, `status`, `head`, `approved`, `session_live`, `exit_timed_out`, `exhausted`, `workspace_closed` | `workspace_id`, `error` | |
   | `triage_started` | ドメイン | `attempt`, `status` | | |
   | `triage_finished` | ドメイン | `attempt`, `by`, `verdict`, `action`, `ask_id`, `resumes`, `overridden`, `failures`, `duration_secs`, `previous_status`, `status` | | `reason`, `instruction` |
   | `triage_failed` | ドメイン | `attempt`, `duration_secs`, `status` | `error` | |
   | `triage_decided` | ドメイン | `ask_id`, `answer`, `status` | | `reason` |
   | `integration_approved` | ドメイン | `status`, `push`, `ask_id` | `pid` | |
   | `integration_started` | ドメイン | `main`, `previous_status` | `pid` | |
   | `integration_receipt` | ドメイン | `main`, `commit`, `receipt`（`validation_finished`と同じ分け方） | | `receipt`の`evidence_or_reason` |
   | `integration_rebased` | ドメイン | `main`, `head_before`, `head_after` | | |
   | `integration_rebase_aborted` | ドメイン | | `reason` | |
   | `verification_command` | ドメイン | `phase`, `attempt`, `index`, `command`, `exit_code` | `log_path` | `output_tail` |
   | `integration_deferred` | ドメイン | `status`, `resumes_left`, detailの`main` / `head` / `conflicts` / `aborted` / `command` / `exit_code` / `checks` / `scope_violation` / `allowed`（pathはrepository root起点） | `reason` | detailの`output_tail` |
   | `integration_failed` | ドメイン | `status`, `receipt`（`validation_finished`と同じ分け方） | `reason` | |
   | `integration_error` | ドメイン | `status` | `reason` | |
   | `run_integrated` | ドメイン | `result_commit`, `commit`, `source_commit`, `main_before`, `history_ref`, `message`, `verification_skipped` | `git_common_dir` | |
   | `push_finished` | ドメイン | `remote`, `commit` | | |
   | `push_skipped` | ドメイン | `remote`, `commit`, `reason` | | |
   | `push_failed` | ドメイン | `remote`, `commit` | `error` | |
   | `follow_up_registered` | ドメイン | `task_id`, `title`, `index`, `skipped`, `goal_closed` | | `follow_up`（receiptの項目そのまま。`description`の自由文を含む） |
   | `worktree_removed` | 診断 | | `path`, `branch` | |
   | `workspace_closed` | 診断 | `by`, `resume_attempt`, `closed_at` | `workspace_id` | |
   | `cleanup_failed` | 診断 | | `workspace_id`, `message` | |
   | `observe_started` | ドメイン | `mode`, `since` | `dir` | |
   | `observe_finished` | ドメイン | `mode`, `outcome`, `exit_code`, `since`, `cursor`, `cursor_saved`, `notes`, `asks`, `goals`, `duration_secs` | `error`, `dir` | |

   `verification_command`の`command`はtaskに登録した文字列（repositoryの文書と同じ性質）なのでドメインにする。`branch`は`dagq/<run-id>`でrun IDから決まるが、Gitの参照名は実装の詳細なので診断にする。自由文の`reason` / `message` / `error`は診断に分類し、その代わりにドメインeventに分類コードを持たせる（[ADR-0034](0034-domain-events-carry-reason-codes-actor-and-configuration-changes.md)）。

4. **将来の拡張: Webを複数マシンの協調役にする。** Webがtask・run・lease・askの正本になり、各マシンのsupervisorはWebからleaseを取ってrunを動かす。そのとき要るものを先に決めておく。実装は別のADRと別のgoalで行う。
   - **eventのIDがマシンをまたいで一意であること。** 今の`run_events.id`はqueue DBのrowidで、マシンごとに衝突する。送るeventにはUUIDv7のような時間順で衝突しないIDを持たせる（run IDとask IDはすでにマシンをまたいで衝突しない形にできる）。
   - **actor。** 誰がそのeventを起こしたか（ADR-0034）。複数マシンになると`asked_by`や`by`のような役割名だけでは区別できないので、マシンとbinary versionを含める。
   - **outboxとcursorによる同期。** ドメインeventはまずローカルのqueue DB（outbox）に書き、送れた位置をcursorで持つ。Webが落ちていてもsupervisorは止まらず、戻ったら続きから送る。送信は冪等（eventのIDで重複を捨てる）。
   - **leaseの移動。** 協調状態の正本がWebに移り、heartbeatはWebへ送る。ローカルの`run_leases`はWebのleaseの写しになる。マシンが落ちたときの引き継ぎ（今の`adopt`と`recover`、[ADR-0012](0012-adopt-stale-lease-of-live-wrapper.md)・[ADR-0025](0025-leaseless-unfinished-run-is-a-recover-run-attention.md)）は、同じマシンのwrapperが生きているかを見られないので、Webのlease期限で判定し直す。
   - 本文は協調役になった後も案1のまま（明示のexportだけ）。

## Alternatives

- **分類せず、送るときにkindごとに決める。** 送り先ができるたびに全kindを見直すことになり、payloadにpathを足す変更がレビューで止まらない。分類を先に決め、新しい値を足すときに分類を問う方が安い。
- **`run_events`をドメインevent専用の表と診断の表に分けるmigrationをする。** 既存のkindとpayloadは公開契約で、`events` / `show` / `stats` / `watch`の出力を変えずに分けるには全経路の書き換えが要る。migrationは固定バイナリの入替を伴う（AGENTS.md）。今は項目ごとの分類を契約にし、表の分割はWebへの送信を実装するときに判断する。
- **本文もWebへ常に送る（案2）。** 障害調査はWebだけで完結するが、画面とlogには秘密とpathが入りうる。伏せる処理の漏れがそのまま外へ出る。ユーザーは案1を選んだ。
- **本文を一切送らない（案3）。** Webで他のマシンのrunを調べるときに手段が無くなる。明示の操作で必要なrunだけ送れる方がよい。

## Consequences

- 新しく記録する値は「どの分類か」を決めてから置き場所を選ぶ。ドメインeventのpayloadにpath・pid・workspace IDを足す変更は、この分類に反するので診断へ回す。
- 今の`run_events`は分類が混ざったままで、Webへ送るときは上の表で項目を落とす。表はkindやpayloadを足すたびに更新が要る（designの[persistence](../design/persistence.md)に移すかは実装taskで決める）。
- 診断の置き場所（`logs/`のJSON Lines）と、stderr・テキストlog・`rebind.jsonl`の扱いはADR-0033で決める。
- Webを協調役にする実装、eventのマシンをまたぐID、outbox、leaseの移動は未着手で、本ADRはproposedのまま方針だけを置く。
