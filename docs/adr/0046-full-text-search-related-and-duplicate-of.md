---
id: adr-0046
type: adr
title: taskの全文検索（search）と決まった規則の関連（related）と重複の記録（cancel --duplicate-of）を持ち、plannerとplan reviewはその候補だけをLLMで判断する
status: accepted
created: 2026-09-25
updated: 2026-09-25
accepted_on: 2026-09-25
owners:
  - hisamekms
tags:
  - runtime
  - planner
  - plugin
  - persistence
related:
  - adr-0029
  - adr-0040
  - adr-0041
  - adr-0045
  - design-persistence
  - design-plugin-integration
---

# ADR-0046: taskの全文検索（search）と決まった規則の関連（related）と重複の記録（cancel --duplicate-of）を持ち、plannerとplan reviewはその候補だけをLLMで判断する

## Context

taskが積み上がると、重複や実装済みのtaskを見つけるのに全文を読んで比べるしかない。2026-09-25の棚卸しでは約100件の全文を読み、着地済みのコードやADRと突き合わせて、重複を約10組（16/17、179/302、203/230、313/247、316/320/311、323/280、289/285、324/317など）、実装済みを11件見つけた。組み合わせは件数の2乗で増え、plannerのcontextにも、[ADR-0041](0041-on-demand-planners-proposals-submitted-and-plan-review-job.md)のplan review job（LLM）のpromptにも収まらなくなる。

見つけた手がかりの多くは、同じテスト名・ファイル名・ADR番号と、同じrunのfollow_upだった。どれも文字列か構造で取り出せ、意味の判断は要らない。一方dagqには全文検索が無く、`dagq lint`（task 276）が見る重複は1つのproposalの中のtitleだけである。重複としてcancelしたことも、cancelの理由の文章にしか残らない。

2026-09-25のplannerとの対話で、ユーザーは次を決めた（goal 33のconstraints）。

- 全文検索はSQLiteのFTS5を使い、task（title・description・acceptance・context）、goal（title・description・acceptance・constraints）、note、着地したcommitのmessageを対象にし、状態で絞り込める。
- relatedは決まった規則の手がかり（宣言した`paths`の重なり、本文に出るファイル名・テスト名・ADR番号、同じrunのfollow_up、同じgoal）で点数を付けて並べ、理由を出す。埋め込み（embedding）は今回は入れない。
- `cancel --duplicate-of X`で、どのtaskの重複としてcancelしたかをeventに残し、`show` / `list`に出し、`stats`に件数を出す。
- plannerは`add`の前に`search` / `related`で候補を確かめる。plan review（goal 29）、follow_upのplanner（task 282）、findingからのplanner（goal 31）は、`search` / `related`で候補を取り出してからLLMで判断する。
- migrationはgoal 32のtask 308の規則（[ADR-0045](0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)の互換の宣言、openではmigrateしない）に従う。

## Decision

この ADR は既存の ADR の決定を変えない。ADR-0041の決定10〜11（plan reviewの入力とverdict）と決定16（follow_upのplanner）には、入力と記録の手段を足すだけで、そこに書かれた入力・権限・verdictは変えない。

1. **`dagq search QUERY`は、SQLiteのFTS5で4種類の文書を全文検索する。**
   - 対象: task（title・description・acceptance・context）、goal（title・description・acceptance・constraints）、note（kindが`observation`のrun_event）、着地したcommitのmessage（決定3）。対象はすべての状態を含み、`draft`から`completed`・`canceled`まで、closeしたgoalも含む。
   - 絞り込み: `--status`（複数可。taskとgoalの状態）、`--kind task|goal|note|commit`（複数可）、`--goal ID`、`--limit`（既定20）。既定ではすべての状態とkindを返す。実装済みのtaskを見つけるには`completed`とcommitが要るので、既定から外さない。
   - 出力: FTS5の`bm25`の順に、kind・ID・状態・title・一致した欄と前後の抜粋（`snippet`）を返す。commitはそれを着地させたtaskとrunのIDを添える。`--json`でも同じ欄を返す。
   - tokenizerは`trigram`にする。本文の大半は日本語で、空白で語が区切られないので、`unicode61`では文全体が1語になり部分一致が引けない。語の組み合わせはFTS5の問い合わせ構文（`AND` / `OR` / `NOT` / 句）に従う。
   - trigramは3文字未満の語を`MATCH`で引けないので、3文字未満の語（「重複」など）は同じ索引の本文への`LIKE`で絞る（全件走査になるが、queueの規模では問題にならない）。3文字以上の語があれば、`MATCH`で引いた行を`LIKE`で絞り、順位と抜粋は`MATCH`の`bm25`と`snippet`で出す。3文字未満の語だけのときは、更新の新しい順に並べ、抜粋は一致した位置の前後を切り出す。

2. **索引はDBの中のtriggerで保ち、どのバイナリの書き込みでも最新にする。**
   - 索引は4種類の文書を1つの通常の（contentを自分で持つ）FTS5の仮想表にまとめる。検索の対象の欄のほかに、kind・ID・状態・goalを`UNINDEXED`の列で持つ。表を分けると`bm25`の統計が表ごとになり、kindをまたいで順位を比べられないからである。noteの本文は`run_events.payload`のJSONの中にあり、kindが`observation`の行だけが対象なので、外部contentの表にはできない。
   - 索引は`tasks` / `goals` / `run_events`（kindが`observation`の行）/ 決定3の表への挿入・更新・削除のtriggerが、`json_extract`などで欄を詰めて同期する。状態の変更も同じtriggerが索引の状態の列に写す。triggerはDBに入っているので、索引を知らない古いバイナリが書いても索引は追いつく。
   - 今後、元の表を作り直すmigration（migration 0021の`tasks`のように）はtriggerを消すので、そのmigrationがtriggerを作り直し、索引を作り直す。
   - migrationは仮想表・trigger・決定3の表を足し、既存の行から索引を作る。ADR-0045の決定6の**互換**として宣言する。根拠: 既存の表と列は変えず、古いバイナリは新しい表を読まない。triggerは古いバイナリの書き込みでも走るが、索引の表に書くだけで元の書き込みの結果を変えず、FTS5はbundledのSQLiteに含まれるので、古いバイナリの書き込みを失敗させない。この migration は goal 32 の task 308（ADR-0045の決定5〜7の下限の仕組み）が入った後に足す。それより前のmigrationはすべて非互換とみなされる（ADR-0045の決定7）からである。openではmigrateせず、`dagq migrate`（と入れ替えの手順）で適用する（ADR-0045の決定5）。

3. **`integrate`は、着地させたcommitのmessageをqueueに記録する。**
   - 着地のcommit messageはtaskのtitleとreceiptのsummaryからruntimeが作り、`main`にしか残らない。検索のたびにgitを読むのはやめ、`run_integrated`と同じトランザクションで、run・task・commit・messageの1行を表に書く。
   - migrationより前に着地したrunと、この決定を知らないバイナリが着地させたrunは、`run_integrated`の`result_commit`からgitでmessageを読んで埋める。埋める契機（`migrate`か、`search`が欠けを見たときか）は実装taskが決める。

4. **`dagq related TASK`は、決まった規則の手がかりで点数を付けて関連の強いtaskを並べ、理由を出す。**
   - 手がかりは次の5つに限る。
     - **宣言した`paths`の重なり**（[ADR-0029](0029-task-declares-paths-and-verification-follows-the-kind-of-change.md)）: 2つのtaskのglobが同じか、一方が他方の一致する範囲を含む。
     - **本文に出るファイル名**: title・description・acceptance・contextと、completedのtaskでは着地したcommitのmessageから、repositoryのpathの形（`src/...rs`、`tests/*.rs`、`docs/...md`、`migrations/...sql`など）を取り出し、同じものを共有する。
     - **本文に出るテスト名**: 同じ本文から、テスト関数の名前の形（snake_caseで複数語の識別子で、`#[test]`の関数名に使われる形）と`--test NAME`の形を取り出し、同じものを共有する。
     - **本文に出るADR番号**: `ADR-NNNN`と`docs/adr/NNNN-`の形を取り出し、同じ番号を共有する。
     - **同じrunのfollow_up**: 2つのtaskが同じrunの`follow_up_registered`で登録された、または一方が他方のrunのfollow_upとして登録された。
     - **同じgoal**: 2つのtaskが同じgoalに属する。
   - 点数は手がかりごとの重みの和にする。ファイル名・テスト名・ADR番号は、多くのtaskに出るもの（`AGENTS.md`など）ほど効かないよう、出るtaskの数で重みを割り引く（IDFの形）。重みの数値と割り引きの式は実装taskが決め、[persistence](../design/persistence.md)などのdesign文書に書く。重みはこのADRを置き換えずに変えてよい。
   - 対象はすべての状態のtaskで、`--status`で絞り込める。既定の件数は10件。出力はtaskごとに、ID・状態・title・点数と、点数に効いた手がかりの一覧（例: `test: a_ready_task_lifts_what_it_waits_for`、`path: src/infrastructure/sqlite.rs`、`adr: 0041`、`follow_up of run <id>`、`goal 33`）を返す。重複としてcancelされたtask（決定5）は、その重複先を添える。
   - 手がかりの妥当性は、2026-09-25の棚卸しで見つけた重複の組で確かめる: 少なくとも半分の組で、一方の`related`の上位5件にもう一方が出ること（goal 33のacceptance (4)）。実装taskは本番のqueueを読み取りだけで使ってこれを確かめ、結果をreceiptに書く。

5. **`cancel TASK --duplicate-of X`で、重複としてcancelしたことを構造として残す。**
   - `X`はTASKと違う存在するtaskで、`canceled`でないこと。`X`が`completed`なら「Xで実装済み」の意味になり、実装済みのtaskもこの形で記録する。`X`自身が重複でcancelされていれば拒否し、その重複先を案内する。
   - 記録はcancelのevent（`task_status_changed`）のpayloadの`duplicate_of`に置く。schemaは変えない。
   - `show TASK`は`canceled (duplicate of X)`を出し、`show X`は`X`を重複先とするtaskの一覧を出す。`list`はcancelされたtaskの行に重複先を出す。`stats`は`--since`以降に重複としてcancelした件数を出す（ADR-0040の決定5と同じくrun_eventsから再導出する）。
   - plan review jobが明らかな重複をcancelするとき（ADR-0041の決定11の`actions`の「重複先のtaskを示す」）と、follow_upのplannerが重複のdraftを閉じるときも、この形で記録する。

6. **plannerは`add`の前に`search`で、`submit`の前に`related`で候補を確かめる。**
   - 人が開いたplannerは、taskを登録する前に、課題のファイル名・テスト名・ADR番号・titleの語で`search`を打つ。登録してから（`draft`のうちに）`related`を打ち、上位の候補の全文だけを読む。重複か実装済みと分かれば、submitせずに`cancel --duplicate-of`で閉じる。
   - goal 33のconstraintsは「`add`の前に`search` / `related`」とするが、`related`は登録前には打たず、`draft`の登録の後・`submit`の前に打つ。`related`の手がかり（宣言した`paths`、本文、goal）はtaskとして登録した形から取り出すので、登録前の文章を受け取る別の入力を持たないためである。`draft`はclaimされずsubmitの前なので、この順でも重複はqueueを流れない。これはconstraintsから意図して変えた点である。
   - 手順は`dagq`の skill と`dagq-planner`の skill に書く（goal 33のacceptance (5)）。

7. **plan review・follow_upのplanner・findingからのplannerは、`search` / `related`で候補を取り出してから、その候補についてLLMで判断する。**
   - **plan review job**（goal 29、ADR-0041の決定10〜11）: jobは、proposalの各taskについて自分で`related`を打ち（必要なら`search`も打ち）、上位の候補について「他のtaskとの重複」「既に実装済み」をLLMで判断し、候補の全文が要れば`show`で読む。`related`と`search`は状態を変えないので、jobが打ってよい。ADR-0041の決定10のpromptの入力（他のsubmittedのproposalと既にreadyのtaskの一覧など）は変えず、jobへの指示（skill か prompt の手順の文）で候補の取り出し方を示す。明らかな重複のcancelは決定5の形で記録する。
   - **follow_upのplanner**（task 282、ADR-0041の決定16）: draftごとに`related`を打ち、上位の候補と照らして採否を決める。重複なら決定5の形で閉じる。
   - **findingからのplanner**（goal 31）: findingからtaskを起こす前に`search`を、起こした後に`related`を打ち、既にあるtaskなら起こさないか重複として閉じる。
   - この goal の task が入る前は、どれも今までどおりに動く。候補の取り出しは入力を足すだけで、それぞれの権限とverdictは変えない。

8. **埋め込み（embedding）は今回は入れない。**
   - 棚卸しで重複を見つけた手がかりは、同じテスト名・ファイル名・ADR番号・同じrunのfollow_upで、文字列と構造で取り出せる。埋め込みは、モデルかAPIへの依存・ネットワーク・費用・索引の作り直しを持ち込み、結果が決まった規則で説明できない。relatedが理由を出せることは、LLMと人が候補を短く確かめるために要る。
   - 決定4の確かめ（上位5件に半分）が通らない、または言い換えの重複（同じ語を使わない重複）が多く漏れると分かったときに、別のADRで検討する。

## Alternatives

- **全文をLLMに読ませて比べる（今のやり方）**: 追加の実装は要らないが、件数の2乗で増え、plannerのcontextとplan reviewのpromptに収まらなくなる。今回の棚卸しで1 sessionの大半を使った。
- **FTS5を使わず`LIKE`で全件を走査する**: schemaは変わらないが、順位付けも抜粋も無く、語の組み合わせも書けない。3文字未満の語だけに使う（決定1）。
- **tokenizerを`unicode61`にする**: 英語には十分だが、日本語の文が1語になり部分一致が引けない。
- **`dagq lint`の重複検査をqueue全体のtitleに広げる**: titleだけでは言い回しの違う重複を拾えず、実装済みも見えない。lintは決まった規則で通すか止めるかを決める検査で、候補を理由つきで並べる用途に合わない。
- **relatedに着地したcommitが実際に変えたファイルを使う**: 実装済みの検出には強い手がかりだが、goal 33のconstraintsの手がかりに入っていない。決定4の確かめが足りないときの次の候補にする。
- **埋め込みによる意味の検索**: 決定8のとおり今回は入れない。

## Consequences

- plannerとplan reviewは、queue全体を読まずに、理由のついた少数の候補だけを読んで重複と実装済みを判断できる。plan reviewはqueueが大きくなっても動く前提を得る（goal 29）。
- 重複と実装済みのcancelが`duplicate_of`として残り、`show` / `list` / `stats`で数えられる。observerは重複の件数の傾向を読める。
- FTS5の索引の分だけDBが大きくなり、書き込みごとにtriggerが走る。queueの規模（数百〜数千件）では問題にならない。
- 手がかりは文字列の形に依存するので、本文にファイル名・テスト名・ADR番号を書かないtaskは`related`に出にくい。skillは、taskの本文にこれらを具体的に書くよう求める。
- 重みの数値はdesign文書に置き、このADRを置き換えずに調整できる。手がかりの種類を足す・減らすときは、このADRを置き換える統合ADRを書く。
