---
id: design-supervisor-lifecycle-marks
type: design
title: "変更の印（`mark` / `marks`）"
status: current
created: 2026-09-26
updated: 2026-09-26
last_verified: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-stats
  - design-supervisor-lifecycle-supervise
  - adr-0051
  - adr-0045
  - adr-0049
---

# 変更の印（`mark` / `marks`）

[ADR-0051](../../adr/0051-kpi-time-series-report-and-push.md)の決定10〜13の実装（task 429）。KPIの期間を「いつ何が変わったか」で区切るための印で、記録する印（run_eventsの行）と導く印（claimの属性から読むだけで何も書かない）の2種類があり、同じ変化を二重に記録しない。印はrunの状態を変えず、attentionにもならない（`events`の既定の絞り込みにも`watch`にも出ない）。読み書きの規則は`src/domain/marks.rs`、CLIは`src/compose.rs`の`record_mark` / `retract_mark` / `marks`。

## 記録する印

どれもtask・goal・runを持たないqueue自身のevent（`record_queue_event`）で、migration `0036_change_marks.sql`（breaking）が`run_events`のCHECKに5つのkindを足した。

| kind | 書くもの | payload |
| --- | --- | --- |
| `supervisor_started` | supervisorの起動と、handoffでexecした次のprocess（[ADR-0045](../../adr/0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)の決定10）。`stall_config_loaded`の直後に1件 | `supervisor`（token）、`dagq_version`（build識別子）、`parallel`、`mode`（`up`がsupervisorの引数に渡すhiddenの`--mode launchd` / `--mode in_cmux`。`up`は登録を見てから登録の`mode`を書くので、起動の時点の値は引数から取る。handoffのexecは`--mode`を落とす（引き継ぐbinaryが`--mode`を知らなくても起動できるように）ので、handoffでは`up`が書いた登録の`mode`。手で起動したsupervisorは`null`）、`auto_update`、`handoff`（bool）、`previous_version`（handoffのときexecの前のbuild識別子） |
| `supervisor_stopped` | loopが終わって登録を消すsupervisor（`down`のdrain、`--once`の終わり、失敗）。handoffでexecするprocessは書かず、次のprocessの`supervisor_started`（`handoff: true`）が引き継ぎを表す。heartbeatに失敗したsupervisorとstaleになったsupervisorは書けない | `supervisor`、`dagq_version`、`outcome`（`stopped` / `failed`） |
| `run_env_changed` | supervisorが起動時と周回のたびに（`[run.env]`のプログラムの検査と同じ場所で）、main checkoutの`dagq.toml`の`[run.env]`を書かれたまま（`${DAGQ_*}`を展開せずに）読み、正規化したhashがqueueの最新の`run_env_changed`の`hash`と違うときだけ1件 | `hash`（キーで並べた`KEY=VALUE\n`の並びを、queueの秘密のsaltで鍵付けしたSHA-256の先頭16桁）、`previous_hash`、`keys`（キーごとの`KEY=VALUE`の同じく鍵付きのhash。次の変化で変わったキーを名指すため）、`changed`（前のhashから変わった・足された・消えたキーの名前）、`supervisor` |
| `mark_recorded` | `dagq mark <label>` | `label`、`note`、`at`（`--at`の時刻。無ければnull）、`by`（`DAGQ_ROLE`、無ければ`human`） |
| `mark_retracted` | `dagq mark --retract <id>` | `mark`（取り消した印のevent ID）、`kind`、`label`、`by` |

- `[run.env]`の値はpayloadに書かない（キーの名前とhashだけ）。hashはsalt（`<queue dir>/run-env-salt`。最初の検査で乱数から作り、ownerだけが読めるfileにする。eventには載せない）とともに求めるので、`CARGO_BUILD_JOBS=4`のような短い値を推して素のhashと照らしても見つからない。saltのfileが消えると次の検査で新しいsaltを作り、次の`run_env_changed`が全部のキーを`changed`にして1件記録する。saltが読めない周回は何も記録しない。saltの無かった版が記録した`run_env_changed`の後では、最初の検査がhashの違いとして全部のキーを`changed`にした1件を記録する。`[run.env]`以外の表（`[stall]`など）や、`[run.env]`の並べ替え・コメントの変更は印にならない。`[run.env]`が空のqueueは、最初の`[run.env]`が現れるまで何も記録しない。空でなかった`[run.env]`が空になったときは、全部のキーを`changed`にした1件を記録する。`dagq.toml`が無いとき（checkoutが書き換えている途中のこともある）と、読めない・壊れた`dagq.toml`では何も記録しない（provisioningと`integrate`がそのファイルを報告する）。表を外すときは`dagq.toml`に空の`[run.env]`を残すか、人が`dagq mark`で残す。最新の印をqueueから読んで比べるので、supervisorを起動し直しても同じ内容で重ねて記録しない。
- 同じbuild識別子と`parallel`で起動し直したsupervisorも`supervisor_started`を記録する（supervisorが生きていた区間を`slot_usage`の分母に使うため。ADR-0051の決定3）。変化の有無は導く印が表し、同じ値の起動からは導く印が出ない。

## 導く印

eventを書かず、`marks`がclaimの順（`run_claimed`のevent IDの順）にtask 197のclaim時の属性を並べ、前のclaimと値が変わった最初のclaimの時刻を印にする。kindは`derived:<属性>`で、`detail`に`attribute`・`from`・`to`・`run_id`・`task_id`・`claim_event`を持つ。

| 属性 | claimの記録 |
| --- | --- |
| `dagq_version` | `run_claimed.dagq_version`（build識別子） |
| `claude_version` | `run_claimed.claude_version` |
| `parallel` | `run_claimed.parallel` |
| `toolchain` | `run_claimed.rustc_release`と`rustc_host`の組（`1.90.0 aarch64-apple-darwin`）。どちらかがnullのclaimは記録の無いclaimとして飛ばす |

- 属性の記録の無いclaim（手での`claim`、task 197より前のrun）は飛ばすので、最初の値やnullから値への変化は印にならない。
- toolchainはdagqを通らずに変わる（hostの`mise`の更新など）ので、変わった時刻はclaimのときにしか分からない。記録する印にはしない。
- 前のclaimと今のclaimの間に`supervisor_started`があり、その`dagq_version`・`parallel`が今のclaimの新しい値と同じなら、その属性の導く印は出さない（起動の印が同じ変化を表し、区切りはその時刻になる）。

## `dagq mark`

`dagq mark <label> [--note TEXT] [--at <cursor>]`は`mark_recorded`を記録し、`marks`と同じ形の印を1件出力する。`label`は前後の空白を除いて1〜120文字。`--at`は`stats`と同じcursor（event ID、`@<unix秒>`、RFC 3339の時刻）で、印の効いた時刻（`at`）になる（event IDならそのeventの時刻。無いIDと、今より後の時刻は拒否する）。event自体は今の時刻で記録する。

`dagq mark --retract <id>`は`mark_recorded`か`run_env_changed`の印を取り消したことを`mark_retracted`として記録する。取り消された印は消えず、`marks`で`retracted_by`に取り消しのevent IDを持つ（KPIの集計はこの印を区切りに使わない）。supervisorの起動・停止と取り消し自体は取り消せない。同じ印の2度目の取り消しは拒否する。

inbox・planner・人のsessionから打てる。observer（許可の一覧に無い）とheadlessのreview・triage・plan reviewのjob（`DAGQ_ROLE=reviewer`は読むコマンドだけ）は打てない。

## `dagq marks`

`dagq marks [--since <cursor>] [--until <cursor>]`は記録する印と導く印を`{"marks": [...]}`で、効いた時刻（`at`）の順に出す。読むだけのコマンドで（read-onlyの接続）、observerとjobも読める。各印は`id`（記録する印のevent ID、導く印はnull）、`kind`、`at`、`recorded_at`（eventの時刻）、`label`（人が読む1行）、`retracted_by`、`detail`（記録する印はpayload、導く印は上の項目）を持つ。`--since`は`at`がその時刻より後、`--until`はその時刻以前の印に絞る。event IDのcursorはそのID以前の最新のeventの時刻と読む。
