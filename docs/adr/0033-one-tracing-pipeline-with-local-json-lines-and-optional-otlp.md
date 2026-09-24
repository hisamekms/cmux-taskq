---
id: adr-0033
type: adr
title: 計測をRustのtracingの1系統にし、出口としてローカルのJSON Lines（常に有効）とOTLP（既定は無効）を足す
status: proposed
created: 2026-09-24
updated: 2026-09-24
owners:
  - hisamekms
tags:
  - runtime
  - observability
  - operations
related:
  - adr-0010
  - adr-0011
  - adr-0020
  - adr-0023
  - adr-0032
  - adr-0034
  - design-supervisor-lifecycle
---

# ADR-0033: 計測をRustのtracingの1系統にし、出口としてローカルのJSON Lines（常に有効）とOTLP（既定は無効）を足す

## Context

runtimeの進行と診断のメッセージは3つの経路に分かれている（2026-09-24、goal 21の調査）。

- `NoteLog::note`（`SupervisorLog`）: stderrと`<queue dir>/logs/supervisor-<started_at>-<pid>.log`に1行の自由文を書く（[ADR-0010](0010-maintainer-and-resident-supervisor.md)の決定6）。
- `eprintln!`（`src/`の21か所）: `integrate`の進行と着地、pushの結果、follow_upの登録失敗、supervisorのheartbeatの失敗、session wrapperのエラー。fileには残らない。in-cmux mode（[ADR-0011](0011-cmux-socket-password-and-in-cmux-fallback.md)）ではsupervisorのworkspaceを閉じると消え、`integrate`を人が打ったときは人のterminalにだけ出る。
- `logs/rebind.jsonl`（[ADR-0020](0020-rebind-queue-to-a-moved-repository.md)）: queue単位の出来事を個別のJSON Linesに書いている。

どれも自由文か個別の書式で、runやtaskのIDで横断して読めない。[ADR-0032](0032-classify-records-into-domain-events-diagnostics-coordination-and-bodies.md)で、この種の記録を「診断telemetry」に分類し、置き場所をローカルの`logs/`、有効ならOTLPとした。ユーザーはOTLPの送り先として自分のbackendとSaaSの両方がありうると考えていて、SaaSへ既定でpathやコマンド行が流れることは避けたい。

workerのClaude Codeは自分でOpenTelemetryを出せる（`CLAUDE_CODE_ENABLE_TELEMETRY`と標準の`OTEL_*` env）。dagqのrunとworkerのsessionを同じbackendで突き合わせられれば、トークン数やtool呼び出しの遅さをrunの経過と並べて見られる。

## Decision

1. **計測はRustの`tracing`の1系統にする。** runtimeの進行・診断のメッセージ（`NoteLog`と`eprintln!`）は`tracing`のevent（`info!` / `warn!` / `error!`）とspanで出し、出口（subscriberのlayer）を足す形にする。メッセージは自由文だけでなく、`run_id`・`task_id`・`goal_id`・`ask_id`・`op`・分類コード（[ADR-0034](0034-domain-events-carry-reason-codes-actor-and-configuration-changes.md)）を構造化したfieldとして持つ。人が読む1行の文言はmessageに残す。ドメインevent（`run_events`）の書き込みはtracingに置き換えない。queue DBが正本のままで、tracingは同じ出来事を診断として並べて見るための写しを出してよい。

2. **出口1: ローカルのJSON Lines（常に有効）。** queueの`logs/`に1行1eventのJSON Linesで書く。今の`supervisor-<started_at>-<pid>.log`のテキストlogを置き換える。supervisorだけでなく、`integrate`・`up` / `down`・`observe`・session wrapperなどqueueを開くすべてのprocessが書く。fileはprocessごと（名前に役割・開始時刻・pidを含める）にし、複数processの同時追記で行が混ざらないようにする。各行は時刻、level、target、message、fields、span（run / taskのID）を持つ。ローテーションと保持期間は実装taskで決める（今は人が消す、ADR-0010と同じ）。stderrへの人向けの出力は今の文言のまま残す（既存CLIの出力を変えない）。`rebind.jsonl`は既存の契約として残し、同じ出来事をtracingにも出す。

3. **出口2: OTLP（既定は無効）。** `dagq.toml`の`[telemetry]`か、標準の`OTEL_EXPORTER_OTLP_ENDPOINT`などの`OTEL_*` envで有効にする。どちらも無ければ出さない。有効なときも、既定で送る属性は次に絞る。
   - 送る: ID（run・task・goal・askのID、trace / span ID）、時刻と所要時間、分類コード、kind、status、verdict、数値（exit code、attempt、parallel、load、トークン数、コスト）、dagqとClaude Codeのversion。
   - 送らない: path（worktree、run dir、log、repository）、コマンド行と`[run.env]`の値、自由文のmessageとerror、画面・log・`output_tail`などの本文（ADR-0032の本文）、pid、cmuxのworkspace / surface ID、hostname。
   - 自分のbackendに送る人は、`dagq.toml`の`[telemetry]`で送る属性の許可を広げられる（例: 診断のfieldとmessageを許す）。本文を送る許可は設けない（ADR-0032の決定2。本文は明示のexportだけ）。

4. **runを1本のtraceにする。** trace IDはrun IDから決定的に作る（同じrunならsupervisorが入れ替わってもresumeでも同じtrace）。claim・worktree作成・workerのsession・validating・review・triage・resume・integrate（検証コマンドを含む）を子のspanにする。子processには`TRACEPARENT`を渡し、session wrapper、`integrate`の検証コマンド、headlessのreview / triage / observerのjobが同じtraceにつながるようにする。

5. **workerのClaude Codeと突き合わせる。** OTLPが有効なときは、workerのworkspaceのenvに同じ送り先（`OTEL_EXPORTER_OTLP_*`）と`OTEL_RESOURCE_ATTRIBUTES=dagq.run_id=<run-id>,dagq.task_id=<task-id>`を渡す。Claude Code側のtelemetryを有効にするか（`CLAUDE_CODE_ENABLE_TELEMETRY`）と何を送るかはClaude Codeの設定に従い、dagqは上書きしない。無効なときはこれらのenvを渡さない。

6. **送信で止めない。** OTLPの送信は上限付きのキュー（batch exporter）で行い、あふれたら捨ててsupervisorを止めない。捨てた件数はローカルのJSON Linesに残す。短命のprocess（`integrate`、`observe`、CLIの各command）は終了前にflushし、flushにも上限時間を付ける。

7. **DBに書けない失敗とpanicはローカルfileとstderrに残す。** queue DBが開けない・書けない失敗と、panic（panic hook）は、tracingのローカルの出口とstderrの両方に出す。この経路はqueue DBに依存しない。

8. **OTLPは監査の正本にしない。** 監査と「何が起きたか」の正本はドメインevent（今はqueue DBの`run_events`、将来はWeb、ADR-0032）である。OTLPは次の理由で正本にならない: 送信が上限付きで欠落しうる（決定6）、backend側でsamplingされうる、保持期間がbackendの設定で決まり短いことがある、そして既定で送る属性を絞っている（決定3）ので事実の一部しか無い。

9. **今回のgoal（21）で実装するのは決定1・2・7まで。** OTLPの出口（決定3〜6）は実装せず、OpenTelemetryの依存も足さない。tracingのfieldとspanを決定3の属性の区別を意識して付けておき、後でexporterのlayerを足すだけで済むようにする。

## Alternatives

- **今のテキストlogと`eprintln!`のまま、fileに書く経路だけを足す。** 画面を閉じても残るようにはなるが、自由文のままでrun IDで横断できず、OTLPに出すときにもう一度書き換えることになる。
- **ドメインevent（`run_events`）もtracingで出し、OTLPを正本にする。** 決定8の理由で監査に使えない。queue DBはleaseやstatusと同じtransactionで書けるので、状態と記録が食い違わない。
- **OTLPを既定で有効にし、全属性を送る。** SaaSを送り先にした人のbackendにpathやコマンド行が流れる。既定を絞り、自分のbackendの人が広げる方が事故が少ない。
- **`log` crateとenv_loggerにする。** spanが無く、runを1本のtraceにできない。OpenTelemetryへの橋も`tracing`の方が揃っている。

## Consequences

- 画面を閉じても、`integrate`を人が打っても、進行と診断が`logs/`に残る。run IDで`jq`で絞れる。
- `logs/`のfile数と容量が増える。ローテーションを入れるまでは人が消す。
- 決定2はADR-0010の決定6（テキストのsupervisor log）を置き換える。本ADRをacceptedにするときに、ADR-0010の決定6を部分的に置き換えたことをここに書く（既存ADRは書き換えない）。
- `NoteLog`のportと`SupervisorLog`は、tracingのsubscriberを組み立てるinfrastructureに置き換わる。testは`tracing`のtest用subscriberでeventを捕まえて確かめる。
- OTLPを実装するときに、trace IDの作り方、`[telemetry]`の書式、許可を広げる属性の名前、Claude Codeへ渡すenvを実装taskで具体化する。本ADRはproposedのまま方針だけを置く。
- workerのClaude Codeのtelemetryと突き合わせる経路は、Claude Code側のtelemetryの仕様に依存する。変わったらこのADRを見直す。
