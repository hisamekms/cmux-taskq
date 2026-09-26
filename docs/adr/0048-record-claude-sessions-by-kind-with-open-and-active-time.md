---
id: adr-0048
type: adr
title: dagqが使うClaude sessionをkindごとの区間としてrun_eventsに記録し、開いている時間と、transcriptのturnから導く稼働時間をstatsで集計する
status: accepted
created: 2026-09-26
updated: 2026-09-26
accepted_on: 2026-09-26
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - provider
  - plugin
  - operations
related:
  - adr-0026
  - adr-0027
  - adr-0034
  - adr-0047
  - adr-0049
  - adr-0051
  - design-supervisor-lifecycle-stats
  - design-supervisor-lifecycle
  - design-provider-lifecycle
  - design-plugin-integration
---

# ADR-0048: dagqが使うClaude sessionをkindごとの区間としてrun_eventsに記録し、開いている時間と、transcriptのturnから導く稼働時間をstatsで集計する

## Context

`dagq stats`（[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定5、[stats](../design/supervisor-lifecycle/stats.md)）はrunの工程の時間（`work` / `validate` / `wait_to_land` / `startup`と`land_phases`）を出すが、dagqが使うClaude sessionの時間を種類ごとには出さない。

- **workerのidleと稼働を分けられない。** workerのsessionはreceiptの後もreviewの間開いたままにする（[ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)）。`work`（claim→receipt）はsessionの起動待ちを含み、receiptの後の開いている時間は数えない。どちらもClaudeが実際にturnを処理していた時間とは別物である。
- **jobの時間が集計されない。** review・triage・plan reviewの`*_finished`は`duration_secs`を持ち、observerは`observe_finished`に持つが、kindをまたいだ合計・中央値・件数は出ない。失敗・timeoutのjobは`duration_secs`を持たないことがある。
- **常駐のsessionが見えない。** inboxと人が開くplannerはrunを持たず、runtimeはそのsession_idも開始・終了も知らない。runtimeが立てるplannerも、開いたこと（`session_workspaces`の行）しか残らない。
- **トークン数（task 199）が同じ単位を要る。** task 199はsessionごとのトークン数をClaude Codeのtranscriptから読む。sessionの記録（kind・session_id・開始・終了）と、transcriptの読み取りを別々に作ると、kindの境界と「読めない版」の扱いが2通りになる。

人の決定（goal 35のconstraints、2026-09-26）:

- 時間はeventの時刻とClaude Codeのtranscriptから導き、単価や推定で埋めない。
- transcriptの読み取りは1か所にまとめ、task 199もそれを使う。読めない版を検出したら記録せず、理由をtracingに書く。
- 計測の失敗でrun・jobを失敗にしない。
- `stats`の既存の出力項目は変えず、足すだけにする。
- Claude Code自身のtelemetry（OTLP）は使わない。

ADR-0049の決定5は「集計はrun_eventsから再導出し、新しい表は持たない」とする。このADRはそれに従い、sessionの記録を新しい表ではなくrun_eventsのkindとして足す（決定2）ので、ADR-0049の決定を変えない。

Claude Codeのtranscriptは`<config dir>/projects/<cwdを符号化した名前>/<session_id>.jsonl`の1行1レコードのJSONLで、レコードは`type`（`user` / `assistant` / `system`など）、`timestamp`、`sessionId`、`isSidechain`、`isMeta`、Claude Codeの`version`などを持つ（2026-09-26、Claude Code 2.1.x）。形式はClaude Codeの内部のもので、版で変わりうる。hookのstdinは`session_id`と`transcript_path`を持つ。`/clear`はsession_idを変え、`--resume`は同じsession_idのtranscriptに追記する。

## Decision

### I. 区間とkind

1. **計測の単位を「区間」にする。** 区間はClaude sessionが1つの目的のために開いている時間で、`(kind, session_id, 開始, 終了)`を持つ。1つのClaude session（session_id）は複数の区間に分かれうる（workerのsessionはreviseの間`revise`の区間になる）。同じsession_idの区間は重ならず、sessionの寿命を切り分ける。kindは次の10個にする。

   | kind | 区間の開始 | 区間の終了 | session_id |
   | --- | --- | --- | --- |
   | `worker` | wrapperの`agent_started`（初回のsession） | 最初の`revise_requested`か、そのsessionの`session_exited`の早い方 | run ID（adapterが`--session-id`に渡す） |
   | `resume` | resumeのwrapperの`agent_started`（`resume_started`の後） | その後の最初の`revise_requested`か、そのsessionの`session_exited`の早い方 | run ID（`--resume <run-id>`は同じsession_idに追記する） |
   | `revise` | `revise_requested` | 次の`revise_requested`か、そのsessionの`session_exited`の早い方 | 送った先のsessionのもの |
   | `review` | `review_started` | `review_finished` / `review_failed` | runtimeが起動時に作るUUID（決定4） |
   | `triage` | `triage_started` | `triage_finished` / `triage_failed` | 同上 |
   | `observer` | `observe_started` | `observe_finished` | 同上 |
   | `plan_review` | `plan_review_started` | `plan_review_finished` / `plan_review_failed`か、行を`interrupted`で閉じたとき | 同上 |
   | `runtime_planner` | supervisorが立てたplannerのsessionの開始（決定6） | そのsessionの終了（決定6・7） | hookが渡すもの |
   | `inbox` | inboxのsessionの開始（決定6） | そのsessionの終了（決定6・7） | hookが渡すもの |
   | `planner` | 人が`dagq plan`で開いたplannerのsessionの開始（決定6） | そのsessionの終了（決定6・7） | hookが渡すもの |

   - reviseの後に`review_started`が来ても区間は`revise`のまま終わらない（sessionは次のreviseか終了まで開いている）。reviewを待つidleは、その前の区間（`worker` / `resume` / `revise`）の開いている時間に入り、稼働時間には入らない。これがworkerのidleと稼働を分ける。
   - 区間の中でsessionに送る他の文面（askのanswer、stallのnudge、merge-treeの衝突の解消の依頼（`requested: true`の`conflict_precheck`）、`/exit`）は、そのときの区間のturnとして数え、kindを分けない。衝突の解消を分けたくなったら、後のADRでkindを足す。
   - kindの集合は閉じていない。ADR-0047が決めた復旧jobとgoal reviewのjobなど、後から足すheadlessのjobは、同じ規則（jobの`*_started`で開き`*_finished` / `*_failed`で閉じ、runtimeがsession_idを渡す）で新しいkindを足す。そのtaskがkindの名前を決め、このADRを置き換えない。
   - Codexなど他のproviderのsessionも区間は記録する（開いている時間は出る）。稼働時間はそのproviderのtranscriptの読み手が無ければ記録しない（決定9）。

2. **区間はsession単位の新しいevent kindで記録し、既存のeventは変えない。** kindは`session_opened`と`session_closed`で、run_eventsに書く。
   - runに属するkind（`worker` / `resume` / `revise` / `review` / `triage`）はそのrunのeventにする。`plan_review`と`runtime_planner`は`plan_review_started`と同じく、proposalの最初のtask（IDの最小）のeventにして`proposal_id`を持たせる。`observer` / `inbox` / `planner`は`observe_started`と同じくtaskの無いeventにする。
   - 上の表の開始・終了のeventを書く同じトランザクションで、同じ時刻の`session_opened` / `session_closed`を書く（`agent_started`と`session_exited`はwrapperが、jobのeventはsupervisorとobserveが書く）。eventを書く場所が1つなので、区間の端が既存のeventとずれない。対になる既存のeventが無いところ（plan reviewの行を`interrupted`で閉じるとき、決定6のhook、決定7の推定）では`session_closed`だけを書く。
   - 1つの区間を閉じるのは1回だけで、閉じた区間への2回目の`session_closed`（例: `/clear`の`SessionEnd`と、新しいsession_idの`SessionStart`の`next_span`）は何も書かない。
   - `session_opened`のpayload: `kind`、`session_id`（知らなければnull）、`cwd`、`transcript_path`（知っていれば）、runに属するkindは`attempt`（resumeとreviseとjobの何回目か）、`workspace_id`（あれば）、`proposal_id`（`plan_review` / `runtime_planner`）、`provider`。
   - `session_closed`のpayload: 対にする`session_opened`のevent id（`opened_event_id`）、`kind`、`session_id`、`reason`（`exited` / `job_finished` / `next_span`（次の区間に切り替わった） / `clear` / `logout` / `inferred`など）、稼働時間を記録したか（`active`: `recorded` / `unavailable`、`unavailable`なら短い理由のコード`active_unavailable`。詳細はtracing）。
   - 既存のeventから区間を導く案（下のAlternativesのA）を採らない理由: 開始・終了のeventがkindごとに違い、session_idを持たないものが多く、inboxとplannerには対応するeventが無い。task 199も同じ区間の鍵（`opened_event_id`とsession_id）で数える。
   - このADRが入る前のrunには区間が無い。過去のrunは既存のeventから埋め直さない（決定を入れた後に始まった区間だけを数える）。

3. **開いている時間は区間の開始から終了までのwall-clockとする。** eventの`created_at`の差で、秒に切り捨てる。終わっていない区間は、`stats`を読んだ時刻（期間集計では窓の終わり）までの長さとし、終わっていない件数を別に出す（決定12）。

4. **headlessのjob（`review` / `triage` / `observer` / `plan_review`）には、runtimeがUUIDを作って`--session-id`で渡す。** adapterの`review_command` / `headless_command`がそれを受け取る。これでtranscriptのファイルをjobの終了前から特定でき、`-p`のstdout（verdictのJSON）の形は変えない。jobのsettingsにhookは足さない（reviewのsettingsが`Stop` hookを持たない理由は[provider-lifecycle](../design/provider-lifecycle.md)のとおり）。

### II. 稼働時間

5. **稼働時間は、区間に属するtranscriptのturnの長さの合計とする。**
   - **turn**: 実際の入力（`type: user`のレコードのうち、`isSidechain`でなく、`isMeta`でなく、中身が`tool_result`だけでなく、compactionの要約でないもの）から、次の実際の入力の前にある最後の`assistant`か`tool_result`のレコードまで。長さはその2つの`timestamp`の差。入力の後に何も無いturn（入力しただけで終わった、すぐ中断した）は0秒。
   - tool（Bash、subagentなど）の実行時間はturnの中に入る。subagentのsidechainのレコードはturnの区切りに使わない（親のturnの中の時間として入る）。backgroundの処理がturnの外で動いている時間は入らない（Claudeはturnを処理していない）。
   - **区間への割り当て**: turnは開始（入力の時刻）が入っている区間に属し、長さは区間の終わりで切る。区間の外で始まったturn（`/exit`の前の入力など、区間の間）は数えない。
   - 期間集計（`--since` / `--until`）では、turnと窓の重なりだけを数える。
   - 稼働時間はturnの合計だけで、単価・トークン・推定で埋めない。

6. **inbox・planner・runtimeが立てるplannerの区間は、pluginのhookがCLIで記録する。**
   - pluginの`hooks/hooks.json`に`SessionStart`（matcherは全部: `startup` / `resume` / `clear` / `compact`）と`SessionEnd`のhookを足す。hookは`DAGQ_ROLE`が`inbox`か`planner`で`DAGQ_QUEUE`があるときだけ、stdinの`session_id`・`transcript_path`・`source` / `reason`と、workspaceの`--env`の`DAGQ_SESSION_KIND`を、隠しコマンド（例: `dagq session-event open|close`）に渡す。それ以外のsession（workerを含む。workerの区間はruntimeが書く）では何もしない。今の`compact|clear`の`status`の出力はそのまま残す。
   - `up`・`dagq plan`・runtimeが立てるplannerは、workspaceの`--env`に`DAGQ_SESSION_KIND`（`inbox` / `planner` / `runtime_planner`）を置く。無い古いworkspaceは`DAGQ_ROLE`から`inbox` / `planner`とする。
   - hookは失敗してもsessionを止めない（exit 0で、dagqが無い・queueが開けないときは何も書かない）。hookのCLIは区間の記録だけを書き、runやproposalの状態を変えない。
   - **session_idが変わるとき**: CLIは区間をsession_idで引く。同じsession_idの`SessionStart`（`resume`・`compact`、同じsession_idのままの`clear`）が来たら、開いている区間をそのまま続ける（2つ目の`session_opened`を書かない）。違うsession_idの`SessionStart`が同じworkspaceで来たら（`/clear`で新しいsession_idになった）、そのworkspaceの開いている区間を`reason: next_span`で閉じてから新しい区間を開く。`SessionEnd`（`reason`: `clear` / `logout` / `prompt_input_exit` / `other`）はそのsession_idの区間を閉じる。同じworkspaceの区間はworkspace_idで辿れるが、kindの集計は区間ごとに数える。
   - `-p`でないsessionのtranscriptのpathはhookの`transcript_path`を記録して使う。

7. **閉じる記録が来なかった区間はruntimeが推定で閉じる。** `SessionEnd`はcmuxのworkspaceを閉じたときやClaude Codeの異常終了で来ないことがある。
   - supervisorは下の取り込み（決定8）のたびに、開いている`inbox` / `planner` / `runtime_planner`の区間のworkspaceが`cmux workspace list`に居るかを見て、居なければ`reason: inferred`で閉じる。時刻はtranscriptの最後のレコードの`timestamp`（読めなければ見つけた時刻）にする。
   - runに属する区間は、wrapperの`session_exited`が書かれないとき（agent起動後のerror、[session wrapper](../design/supervisor-lifecycle/session-wrapper.md)の4）、runを`recover`・abandon・triageで終わらせたeventで`reason: inferred`として閉じる。
   - jobの区間は、終わりのeventの無いまま次が始まったら閉じる: 別のsupervisorが引き継いでreviewを最初からやり直す（新しい`review_started`）、staleなleaseを置き換えてtriageを始め直す（新しい`triage_started`）、plan reviewの行を`interrupted`で閉じる、のとき、同じrun（plan reviewは同じproposal）の開いている同じkindの区間を`reason: inferred`で閉じる。observerの子プロセスがsupervisorと一緒に死んで`observe_finished`が無いときは、次の`observe_started`で開いている`observer`の区間を閉じる。どれも時刻はtranscriptの最後のレコードの`timestamp`（読めなければ閉じる時刻）にする。
   - 推定で閉じた区間の数を`stats`に出す（決定12の`inferred`）。

8. **transcriptからturnを取り込むのはsupervisorと区間を閉じる記録で、`stats`はtranscriptを読まない。**
   - turnは`session_turns`のevent（`opened_event_id`、`session_id`、`turns`: `[[開始, 終了], ...]`（RFC 3339）、`through`（読んだ最後のレコードの`timestamp`））として、区間と同じrunかtaskの無いeventに書く。`stats`はrun_eventsだけから集計し（ADR-0049の決定5）、読むたびにtranscriptを開かない。
   - 区間を閉じるとき（決定2の`session_closed`と同じ処理）に、残りのturnを全部取り込む。
   - 開いている区間（常駐のinboxは何日も開いている）は、supervisorがループで既定10分ごとと、observerを起動する前に、前回の`through`より後の完了したturn（次の実際の入力が来たturn）だけを取り込む。進行中のturnは完了するか区間が閉じるまで書かない。supervisorが居ない間の開いている区間は、閉じる記録か次のsupervisorが取り込む。
   - 取り込みに失敗しても（ファイルが無い、読めない、DBに書けない）run・job・sessionの結果は変えず、tracingに書いて次の取り込みで読み直す。区間を閉じるときに読めなければ、`session_closed`を`active: unavailable`で書き、稼働時間を記録しない（決定9）。

9. **transcriptの読み取りは1つのモジュールにまとめ、task 199のトークン数もそれを使う。**
   - adapterのinfrastructure（Claude Codeは`src/infrastructure/`の1モジュール）に置き、applicationには`Transcripts`のport（区間の`session_id`・`cwd`・`transcript_path`を受けて、レコードの列を`TranscriptRecord`（時刻、実際の入力か、assistant / tool_resultか、sidechainか、task 199が要るusage）として返す）で見せる。turnの組み立て（決定5）はdomainの純粋関数にする。
   - pathの解決もここだけで行う: 記録した`transcript_path`、無ければ`$CLAUDE_CONFIG_DIR`（無ければ`~/.claude`）の`projects/<cwdの符号化>/<session_id>.jsonl`、それも無ければ`projects/*/<session_id>.jsonl`を探す。
   - **読めない版の検出**: ファイルが無い、1行もJSONとして読めない、決定5が要るフィールド（`type`・`timestamp`・`sessionId`）を持つレコードが無い、`sessionId`が区間のsession_idと違う、のどれかなら「読めない」とし、区間の稼働時間（とtask 199のトークン数）を記録しない。部分的に読めない行は飛ばし、飛ばした数をtracingに書く。理由（コード: `transcript_missing` / `transcript_unparsable` / `transcript_unsupported` / `session_mismatch`）と、読めたレコードの`version`をtracingに書き、`session_closed`の`active_unavailable`にコードを載せる。
   - task 199はこのportのusageを同じ区間の鍵（`opened_event_id`）で数え、transcriptを自分では開かない。形式が変わったときに直すのはこのモジュールだけにする。

10. **計測は結果を変えない。** 区間とturnの記録に失敗しても、run・job・sessionの遷移と結果は変えない。区間のeventは遷移のeventと同じトランザクションで書くが、書けないのは遷移も書けないときだけで、そのときは既存の扱い（DB障害）に従う。hookと取り込みの失敗はtracingにだけ残す。Claude CodeのOTLPのtelemetryは使わない。

### III. `stats`

11. **`stats`は区間を読むだけで、既存の出力項目を変えず、足すだけにする。** 窓（`--since` / `--until` / `--full`）と`--goal`の規則は既存のものを使う。

12. **足す項目。**
    - **`sessions`**（トップレベル）: `{window, by_kind: {<kind>: {count, open, active, open_now, inferred, active_unavailable}}}`。`window`は`backend_failures`と同じ窓（`--since`があればcursorより後から`next_cursor`まで、無ければ対象のrunの最初のeventのうち最も古いもの以降、`--full`か対象のrunが無ければ全件）。窓と重なる区間を数え、長さは窓で切る（決定3・5）。`open`と`active`は`{count, total, median, p90, max}`（秒。`median`と`p90`は既存と同じ規則）で、`active`は稼働時間を記録した区間だけを数える。`open_now`は終わっていない区間の数、`inferred`は推定で閉じた区間の数、`active_unavailable`は稼働時間を記録しなかった区間の数。10個のkindは記録が0でも必ず出す。`--goal`があれば、そのgoalのrunの区間と、そのgoalのtaskを含むproposalの`plan_review` / `runtime_planner`の区間だけにし、`observer` / `inbox` / `planner`は0にする。
    - **`runs`の各行**: `sessions`（`{<kind>: {count, open, active}}`、`open` / `active`は秒の合計。区間の無いkindは出さない）。窓で切らず、そのrunの区間全部を数える。
    - **`goals`と`overall`**: `sessions`（`{<kind>: {count, open, active}}`、`open`と`active`は`{count, total, median}`）。対象のrunの区間を数える（`goals`と`overall`の既存の区間と同じ母集団）。
    - observerは`stats`を入力に読むので、そのまま載る。ADR-0051のKPI（goal 40）がkindごとの時間を使うときもこの項目を読む。

### IV. 進め方

13. **後続taskの順。** (1) runtime: 決定2・3のevent、決定4の`--session-id`、決定1の表のkind（`worker` / `resume` / `revise` / `review` / `triage` / `observer` / `plan_review`）の区間の記録と、決定12の`stats`の`open`（`--evidence e2e`とllvm-covの検証を付ける）。(2) runtime: 決定9のtranscriptのport、決定5のturn、決定8の取り込みと`active`。(3) runtimeとplugin: 決定6・7の`DAGQ_SESSION_KIND`、hookと隠しコマンド、推定の終了。(4) docs: [stats](../design/supervisor-lifecycle/stats.md)・[provider-lifecycle](../design/provider-lifecycle.md)・[plugin-integration](../design/plugin-integration.md)とpluginのskillが新しい`stats`を説明する。task 199は(2)の後に同じportを使う。

## Alternatives

- **A. 既存のeventから区間を導き、新しいeventを足さない。** `agent_started` / `session_exited` / `*_started` / `*_finished`で多くのkindは端が決まる。採らない: kindごとに対にする規則が違い（reviewの失敗、plan reviewの`interrupted`、resumeの`session_exited`の対応など）、session_idを持たないものが多く、inboxとplannerには対応するeventが無い。task 199が同じ鍵で数えるにも、区間の記録が1か所にある方がよい。
- **B. 新しい表（`claude_sessions`と`claude_session_turns`）を持つ。** 窓の集計が速い。採らない: ADR-0049の決定5（statsはrun_eventsから導き、新しい表を持たない）を変えることになり、ADR-0042によりADR-0049を丸ごと置き換える統合が要る。run_eventsのkindとturnのeventで足りる。
- **C. `stats`を読むたびにtranscriptを読む。** 記録が要らない。採らない: `stats`が純粋な集計でなくなり、observerの入力が`~/.claude`のファイルに依存し、読むたびに何日分のinboxのtranscriptを読む。消えたtranscriptは二度と数えられない。
- **D. 稼働時間を`Stop` hookのidle markerと入力の印（`prompt-submit.json`）の時刻から導く。** runのsessionにはhookがある。採らない: headlessのjobとinbox / plannerには無く、markerは上書きされ、runの`idle.log`も入力の時刻を持たない。transcriptは全kindで同じ規則で読める。
- **E. `-p`のjobを`--output-format json`で起動し、結果の`duration_ms`を使う。** 採らない: review・triage・plan reviewのstdoutのverdictの形を変え、timeoutでkillしたjobには結果が無い。jobの開いている時間はeventから、稼働時間はtranscriptから他のkindと同じく導く。
- **F. Claude CodeのOTLPのtelemetry。** 人の決定で使わない。
- **G. reviseをworkerの区間の中に含め、kindを分けない。** 採らない: goal 35はreviseの差し戻しの時間を分けて見ることを求める。区間の切り替えで同じsessionを分ければ、session単位の合計（workerとreviseの和）も失わない。

## Consequences

- workerのsessionの開いている時間と稼働時間の差として、reviewを待つidleが見える。reviseとresumeの時間も初回と分けて見える。
- review・triage・observer・plan reviewのjobの時間が、kindごとの合計・中央値・件数で出る。
- inboxとplannerの時間は期間集計で出る。常駐のinboxは開いている時間が長く、稼働時間との差が大きい。
- run_eventsに`session_opened` / `session_closed` / `session_turns`が増える。inboxのturnは10分ごとにまとめて書くので、1つのeventが多数のturnを持つ。`events` / `watch`は新しいkindを流すので、inboxの`watch`が拾わないように、既存のattentionの判定を変えないことを後続taskで確かめる。
- Claude Codeのtranscriptの形式に依存する。変わったら決定9のモジュールだけを直し、直すまでは稼働時間（とtask 199のトークン数）が記録されない（`active_unavailable`で数えられる）。runの結果は変わらない。
- pluginが`SessionStart`の全matcherと`SessionEnd`のhookを持つ。`DAGQ_ROLE`の無いsessionでは何もしないので、dagqの外のClaude Codeの利用には影響しない。
- 過去のrunには区間が無い。前後の比較は区間を記録し始めた後だけで行う。
