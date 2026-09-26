---
id: design-supervisor-lifecycle-logs
type: design
title: "Logs"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0033
---

# Logs

runtimeの進行と診断のメッセージは`tracing`の1系統で出す（[ADR-0033](../../adr/0033-one-tracing-pipeline-with-local-json-lines-and-optional-otlp.md)の決定1・2・7。ADRはproposedのまま、実装はtask 194）。applicationは`tracing`のマクロ（`info!` / `warn!` / `error!`）を直接使い、人が読む1行の文言をmessageに、`run_id`・`task_id`・`ask_id`・`op`（`integrate` / `push` / `follow_up` / `cleanup`）・`error`・`reason`などをfieldに持たせる。`integrate::land_integrating`は`integrate` span（`run_id`・`task_id`）の中で走る。portを挟まないのは、`tracing`がI/Oを持たないfacadeで、出口（subscriber）は起動部分が選び、testは`tracing`の`Dispatch`を差し替えて捕まえられるため（ADR-0013の方針1の「portは外部I/Oのため」に照らして、`NoteLog` portと`SupervisorLog`は廃止した）。supervisorがheartbeat・検証・着地のthreadを起こすときは`supervise::spawn_traced`で起こし、起こした側のsubscriberを引き継ぐ。

subscriberは`infrastructure::telemetry::Telemetry`で、`main`がコマンドごとに組み立てて`install`する（`set_global_default`とpanic hook）。`supervise`・`integrate`・`observe`・session wrapper（`session`）は`<queue dir>/logs/<process>-<YYYYMMDDTHHMMSSZ>-<pid>.jsonl`（`supervise`は`--log-dir DIR`があればDIR、無ければqueueの`logs/`。どちらもdirは作る）に1行1レコードのJSON Linesを追記し、ほかのコマンドはstderrだけに出す。レコードは`timestamp`（ISO 8601のUTC、ミリ秒）、`level`、`target`（moduleのpath）、`message`、`fields`（eventのfieldに、入っているspanのfieldを重ねたもの。同名はeventが勝つ）、`spans`（外側からの`name`とfield）を持つ。messageやfieldの改行はJSONの`\n`になるので、cmuxのstderrの末尾を含んでも1行は1レコードのまま。1レコードは1回の`write`で追記する。fileの先頭は`target: dagq::telemetry`の起動レコード（`process`・`pid`・`version`）で、panicは`dagq::telemetry::panic`のレコード（`thread`・`location`）になり、どちらもfileだけに書く（panicのstderrは既定のhookが従来どおり出す）。stderrには各eventのmessageだけを1行で出し、文言は従来のstderrと`supervisor-*.log`のものと同じ（新しく足したのは`observe`の`observer (<mode>) started` / `finished: <outcome>`の2行だけ）。コマンドが失敗して終わるときは、stderrの`{"error":…}`に加えて`dagq::telemetry::exit`のレコードをfileに残す。失敗や保留はWARN（heartbeatの失敗とsupervisorの異常終了はERROR）、進行はINFOで、runに関わるeventは`run_id`をfieldに持つ（`jq 'select(.fields.run_id == "<run-id>")'`で1 runを追える）。fileが開けなければその旨をstderrに1行出してstderrだけで続け（以前の`--log-dir`はdirが作れないと`supervise`を起動失敗にしていたが、ADR-0033の決定どおり止めない）、書けなかったレコードは捨てる（processは止めない）。ローテーションはしない（人が消す）。file名の時刻はprocessの起動時刻で、`supervisors.started_at`とは一致しない（pidで対応が付く）。以前のバイナリが書いた`supervisor-<started_at>-<pid>.log`（`[unix time] message`のテキスト）はそのまま残し、runtimeは読まない。

`up`が作るagentは`--log-dir <queue dir>/logs`で起動し、launchdが拾うstdout / stderrは同じdirの`launchd.log`に溜まる（起動ごとのファイルはruntimeが分け、`launchd.log`は分けない。ローテーションはしない）。`locate`は`log_dir`、`label`、`launch_agent`（plistのpath。存在しなくても出す）を返す。
