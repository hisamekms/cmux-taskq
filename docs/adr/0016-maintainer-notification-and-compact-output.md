---
id: adr-0016
type: adr
title: maintainerを使い捨てのsessionにし、status / watch / doctorの通知経路と圧縮出力、pluginの起き直しhookを持たせる
status: accepted
created: 2026-09-23
updated: 2026-09-23
accepted_on: 2026-09-23
owners:
  - hisamekms
tags:
  - runtime
  - maintainer
  - plugin
  - operations
related:
  - adr-0008
  - adr-0010
  - adr-0011
  - design-supervisor-lifecycle
  - design-plugin-integration
  - design-persistence
---

# ADR-0016: maintainerを使い捨てのsessionにし、status / watch / doctorの通知経路と圧縮出力、pluginの起き直しhookを持たせる

## Context

supervisorからmaintainer（[ADR-0010](0010-maintainer-and-resident-supervisor.md)の常駐Claude Code session）へ状態変化を届ける経路が無い。唯一の接点は`up`がmaintainer workspaceを作るときの一度きりで、`src/lifecycle.rs`の`maintainer_command`が`src/runtime.rs`の`maintainer_prompt`を`claude`の起動引数（`--`の後の初期prompt）に渡す。その後はrunが`awaiting_integration`・`needs_session`・`failed`になっても、終了要求が`exit_request_timed_out`になっても、supervisorが止まっても、人が声をかけるかmaintainerが自分でpollingするまで誰も気づかない。

一方でmaintainerは長期sessionになり、コンテキストが肥大する。このrepositoryの本番queueで実測した1回の出力量は次のとおり。

| 出力 | 大きさ |
| --- | --- |
| `list` | 143 KB |
| `goal show` | 70 KB |
| `show`（events全文） | 11 KB |
| `doctor`（maintainerが20秒ごとにpolling） | 2 KB × 回数 |
| plugin skill（`dagq` / `dagq-maintain` / `dagq-recover`の`SKILL.md`合計） | 39 KB |

これにレビューのdiff全文が加わる。compactionで要約されるか`/clear`・再起動で失われると、maintainerは「何を待っていたか」を自分の記憶から復元できず、同じ大きな出力を読み直す。

## Decision

**原則。** maintainerは状態を持たない使い捨てのsessionとする。runtimeは「maintainerが起きた瞬間に必要な情報だけを、上限のある大きさで返す」責任を負う。compaction・`/clear`・再起動は同じ「起き直し」として扱い、起き直しに必要なものはすべてqueueから1コマンド（`status`）で再導出できる。以下の8点を決める。

1. **maintainerの情報源は`status` / `watch` / `doctor`の3つに固定する。**
   - `status`は起き直しの起点。supervisorの健全性（登録・stale・mode・version）、未完了run、attention（下の2）の一覧、次に`watch`へ渡すcursorを返す。出力はrun数と未解決attention数に比例する行数に収め、events全文やreceipt全文は含めない。
   - `watch --after <cursor>`は差分。cursorより後にattentionイベントが起きるか、supervisorの健全性が変わるまでblockし、起きたattentionと新しいcursorを返して終了する。出力は返すattentionの件数に比例する。
   - `doctor`は診断で、人（またはmaintainer）が詰まりを調べる時だけ使う。pollingの手段にはしない。
2. **run_eventsのkind名を公開契約にし、attentionの判定をdomainに置く。** 既存のkind名とpayloadは変えず、追加だけ行う。どの遷移がattentionかはdomainの関数が決める。attentionは次のとおり: runが`awaiting_integration`になる、`needs_session`になる、`failed`になる、`exit_request_timed_out`、supervisorの停止またはstale。supervisorの起動・停止はrun_eventsに載せず、`supervisors`表（登録・heartbeat・PID）から導出する。schema（`user_version`）は変えない。cursorはrun_eventsの`id`とsupervisor健全性の要約から作る。
3. **runtimeはmaintainerのterminalに文字を打ち込まない。** maintainerは`watch`をbackgroundで走らせ、その終了で起きる（Claude Codeのbackground commandの完了通知を使う）。起きたら報告し、次の`watch`を張り直す。workerへの`/exit`はidle markerで入力可能な状態を確かめられるので従来どおり送る。
4. **人への通知は`cmux notify`。** supervisorはattentionを観測するたびにmaintainer workspaceへ`cmux notify`を送る。通知は人に向けたもので、maintainerの入力にはならない。
5. **`integrate`は自動で呼ばれない。** `watch`の中からも、イベントの副作用としても`integrate`を呼ばない。`awaiting_integration`はattentionとして報告されるだけで、着地の承認はユーザーに残す（[ADR-0008](0008-merge-queue-squash-landing.md)のmerge queueはmaintainerが呼ぶ）。
6. **maintainer経路のコマンドは既定で圧縮し、全文は`--full`のopt-in。** `status` / `show` / `goal show` / `doctor`（と`list`）の既定出力はrun数またはtask数に比例する行数に収める。圧縮はJSONの既存キー名を変えずに「省く」か「切り詰める」で行い、`--full`で従来の全文を返す。
7. **レビューは文脈の外で行う。** `review ID`がrunの差分・receipt・検証結果を`<run_dir>/review.md`に書き、pathを返す。maintainerはdiff全文を読まず、subagentにそのpathを渡して結論だけ受け取る。
8. **pluginが起き直しを自動化し、skillとpromptを縮める。**
   - pluginに`SessionStart` hook（matcher `compact` / `clear`）を持たせ、`DAGQ_ROLE=maintainer`の時だけ`status`を出力してコンテキストに入れる。それ以外のsession（workerや他のClaude Code session）では何も出力しない。
   - `dagq-maintain` skillを役割ごと（起動、監視、レビューと着地、`needs_session`など）に分割し、コマンドの詳細やJSONの読み方のような参照情報は`reference/`に出して必要な時だけ読む。
   - `maintainer_prompt`を「`status`から始め、`watch`をbackgroundで回し、attentionを報告して承認を待つ」に縮める。

実装はgoal 6の後続taskが行う（runtimeの`status` / events / `watch`、`cmux notify`、圧縮と`--full`、`review`、pluginのhookとskill分割）。本ADRの時点では未実装。

## Alternatives

- **supervisorがmaintainerのpromptに`cmux send`で打ち込むpush型**: 追加の仕組みなしにmaintainerを起こせるが退ける。(a) maintainerのterminalのUI状態が分からない。permission dialogや選択肢が開いている時に通知の文字列とEnterが届くと、通知が選択肢を押してしまう。workerにはidle marker（`Stop` hook）があるが、maintainerは人と対話しているので「入力してよい瞬間」を定義できない。(b) 送達確認が無い。打ち込んだ文字がpromptに入ったか、処理されたかをruntimeは知れず、取りこぼしも二重送信も検出できない。(c) Claude CodeのTUIに結合する。人がmaintainerを務める（Claude Codeを使わずにCLIを叩く）と機能せず、画面の構造が変わると壊れる。pull型の`watch`はこのどれも持たず、同じattentionを`status`からも再導出できる。
- **maintainerが`doctor`をpollingし続ける（現状）**: 実装は要らないが、2 KBを20秒ごとにコンテキストへ積み、判断もmaintainerの記憶に頼る。
- **attentionをrun_eventsとは別の表に書く**: 通知の既読管理はしやすいが、schemaが変わり、run_eventsと二重の真実になる。attentionはrun_eventsとsupervisors表から導出できる。

## Consequences

- [ADR-0010](0010-maintainer-and-resident-supervisor.md)の決定7（maintainerの初期promptに役割・queueの場所・最初に打つコマンドを含める）を改める: 初期promptは`status`・`watch`・報告と承認待ちの手順に縮め、起き直しはpromptではなく`status`とSessionStart hookが担う。
- [plugin-integration](../design/plugin-integration.md)の「hook・agent・MCPは持たない」を改め、Claude Code pluginは`SessionStart` hookを持つ。hookは`DAGQ_ROLE`で分岐し、maintainer以外には影響しない。
- run_eventsのkind名が公開契約になるので、kind名の変更・削除は以後breaking changeとして扱い、追加だけ自由に行う。
- 既定出力が圧縮されるので、全文を前提にしたskillの手順と人の使い方は`--full`を付ける必要がある。
- レビューの一次資料は`review.md`になり、maintainerのコンテキストにはsubagentの結論だけが残る。
- 対象外: `ask` / `answer`によるworkerからmaintainerへの相談経路、`needs_session`のrunをruntimeが自動でresumeする仕組み、承認なしの自動着地は本ADRでは決めない。後続ADRで扱い、そのときもmaintainerへの経路は`watch`（attentionの追加）を使い、terminalへの打ち込みは使わない。
