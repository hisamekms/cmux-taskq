---
id: adr-0009
type: adr
title: 複数のtaskが解く上位の課題をgoalとして表現し、workerのpromptに流す
status: proposed
created: 2026-09-22
updated: 2026-09-22
owners:
  - hisamekms
tags:
  - domain
  - persistence
  - prompt
related:
  - design-domain-model
  - design-persistence
  - design-supervisor-lifecycle
  - design-plugin-integration
  - adr-0007
---

# ADR-0009: 複数のtaskが解く上位の課題をgoalとして表現し、workerのpromptに流す

## Context

queueの`Task`はtitle、description、acceptance、verification_commandsだけを持ち、taskの間の関係は依存の辺しかない。workerへのprompt（`src/runtime.rs`の`prompt`）はそのtask単体の情報だけを含む。一方で実際のtaskの多くは1つの課題（plan のステップに相当）を分割したもので、その構造はdocs（`plans/current.md`のステップと journal の`plan_step`）にだけあり、queueもworkerも知らない。

この結果、(1) 命名やモジュール境界など description に書ききれない判断を兄弟taskがばらばらに決めて統合後に噛み合わない、(2) workerが隣のtaskの範囲まで手を出して並列runの衝突を作る、(3) 依存元taskが何をしたかが後続のworkerに伝わらない（Bは Aの変更を含むmainから始まるが、Aのreceipt summaryもresult commitも知らされない）、(4) 全taskが`completed`でも課題が未達である「分解の抜け」を誰も検証しない、という精度の損失がある。

## Decision

- 新エンティティ`Goal`を追加する。項目はtitle、description、acceptance、constraints（任意。命名・境界・やらないこと）、doc（任意。repository内の参照文書のパス）。`Task.goal_id`はnullableで、goalのないtaskを許す。goalはqueueに属するのでrepository単位。repositoryをまたぐgoalは扱わない。
- goalは状態機械を持たない。進捗はtaskのstatusから導出する。完了は`goal close ID --verdict achieved|abandoned`の1回のイベントで記録する。`achieved`は未終端のtaskがあれば拒否し、`abandoned`は`in_progress`のtaskがあれば拒否する。閉じたgoalへのtask追加と付け替えは拒否する。続きは新しいgoalを作る。
- goalにverification_commandsは持たせない。課題レベルの機械検証が要るなら、全taskに依存する検証taskをgoalの末尾に登録する。
- goalの記述は`goal edit`で編集できる。`goal_updated`イベントに新旧を残す。走行中のrunは`prompt.txt`のスナップショットのままで、次のclaimから新しい記述が使われる。
- taskのgoal付け替え（`set-goal`）は`draft`/`ready`だけに許す。依存の追加・削除と同じ規則。
- 依存はgoalをまたいでよい。同じgoalを強制しない。
- promptにはgoalの記述、acceptance、constraints、docに加えて、直接の依存元task（integrated runのreceipt summaryとresult commit付き）と、claim時点で`in_progress`の兄弟task（titleのみ）を載せる。goalの全taskは載せない。goalがないときも「Goal: none, this task stands alone」と書き、promptの形をgoalの有無で変えない。「担当はこのtaskだけ。兄弟の範囲に手を出さず、必要ならreceiptに書く」を明記する。
- Taskに任意の`context`（背景と参照文書）を足し、goalの有無に関係なくpromptに載せる。「なぜやるか」はtask単位の`context`、「何と一緒にやるか」はgoal、と役割を分ける。
- receiptに任意の配列`follow_ups`（workerが提案する後続task）を許す。`Receipt::check`は形だけ確認し、検証には使わない。SVが`show`で見てgoalへ登録するかを判断する。
- スケジューリングは変えない。`candidates`はID順のまま。「進行中のgoalを優先」「同一goalの同時実行上限」は衝突が実際に起きてから検討する。
- 永続化はschema v6。`goals`テーブル、`tasks.goal_id`（FK、index）、`tasks.context`。イベントは`goal_created`、`goal_updated`、`goal_closed`、`task_goal_changed`で、run作成前なので`run_id`はnull。
- plugin の`taskq` skillは「課題を聞く → goalを登録 → taskに分解して登録」を標準手順にし、一発taskだけgoalなしを許す。
- 着手はドッグフーディング（journal 014）のあと。段階1（依存元のsummaryとresult commitをpromptへ）、goalエンティティ、prompt拡張、plugin skillの4件をcmux-taskq自身のtaskとして登録する。この4件が最初のgoalの実例になる。
- journal テンプレートの「## Goal」節はtask単体の到達点を指しており名前が衝突する。019でjournalの運用を書き換えるときに節名を「## Scope」などに変える。エンティティ名は`goal`のままにする。

## Alternatives

- 全taskにgoalを必須にする: goalの価値は複数taskの間で判断を揃えることにあり、1 taskではtitleとacceptanceの二重書きになる。typo修正やclippy警告解消のような一発taskに摩擦を足すとqueueを通さず手で直すようになる。`add`で1 task goalを自動生成する案は、goal一覧が1 task goalで埋まり、課題レベルのacceptance検証が意味を失う。
- goalを「子を持つTask」として木構造にする: エンティティは減るが、claimが親を除外する、親のstatusを子から導出する、といった分岐が状態機械に入り、`in_progress`はclaimしたtaskだけが持つという invariant が崩れる。
- goalに状態機械（open → reviewing → achieved/abandoned）を持たせる: 将来の自動判定には繋げやすいが、実装とinvariantが増える。1回のclose記録で足りる。
- goalにverification_commandsを持たせてclose時に実行する: closeが長時間処理になり、どのcheckoutで実行するかの規則が増える。検証taskで代用できる。
- promptに同じgoalの全taskを載せる: 分解の全体像は見えるが、task数が増えるとpromptが肥大し、workerが兄弟の範囲に手を出しやすくなる。
- goal作成後は不変にする: typo直しでも付け替えが要る。
- 依存元の情報だけをpromptに載せ、goalエンティティを作らない（段階1のみ）: 前提の伝達には効くが、並列に走る兄弟との衝突回避と分解の抜けの検出には効かない。段階1は先行実装として含める。
- エンティティ名を`objective`にしてjournalの節名を残す: 衝突はないがCLIの語として長い。

## Consequences

- workerは自分のtaskがどの課題の一部か、依存元が何をしたか、隣で何が走っているかを知って作業する。判断の一貫性と越境の抑制が期待できる。効果はjournal 013のA → Bで、Bのreceipt summaryとdiffがAの変更を前提にできているかで見る。数値では`needs_session`の回数とSVレビューでの差し戻し数を導入前後で比べる。
- SVは全task完了後にgoalのacceptanceに照らしてレビューし、未達なら後続taskを同じgoalに登録してからcloseする。分解の抜けを検出する責任がSVに明示される。
- goalのないtaskは今までどおり動く。promptの形は変わらないので、goalの有無でworkerの読み方が変わらない。
- `prompt.txt`はclaim時点のスナップショットで、goalの編集や兄弟の状態変化は走行中のrunに届かない。
- schema v6への移行は`init`または最初のopenで自動。v5のバイナリはv6のDBを拒否する。
- 設計文書（domain-model、persistence、supervisor-lifecycle のprompt、plugin-integration）は実装時に更新する。
